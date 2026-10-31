//! Supervision for the arca file chooser (`arca --chooser-prompt`).
//!
//! ADR-0026 retired the prompter process and left exactly one out-of-process
//! prompt: arca owns the FileChooser dialog (ADR-0027). This module owns the
//! child, and nothing else does.
//!
//! ## Lifetime and hang policy
//!
//! Each request spawns one child, writes one JSON request, and waits for it to
//! exit — the dialog's window lifetime *is* the request's lifetime, so a user
//! who leaves the chooser open keeps the child alive for as long as they like.
//! There is deliberately **no wall-clock timeout**: any bound short enough to
//! be safe would kill a legitimate folder-browsing session, and the child
//! holds no lock or exclusive resource the daemon needs. The bound that does
//! exist is *count*, not time:
//!
//! - `Request.Close` (delivered through the `cancellation` closure) terminates
//!   the child immediately and answers the caller with code 1;
//! - `MAX_ACTIVE_FILE_CHOOSERS` caps concurrent dialogs, so a caller that
//!   spawns-and-forgets saturates its own cap and further requests are refused
//!   with code 2 instead of accumulating without end;
//! - a crashed or hung-with-closed-stdout child is reaped by the exit poll
//!   below; a child that never exits and never answers stays alive only until
//!   the frontend gives up on the request.
//!
//! The wait is a 20 ms `try_wait` poll so `cancellation` is observed without a
//! signal: one sleeping thread per open dialog is the accepted cost (an
//! in-flight dialog is user-visible work, not idle load).
//!
//! ## Failure visibility
//!
//! The child's stderr is tee'd: forwarded live to the daemon's stderr while
//! its tail is retained for failure reporting. A child the dynamic loader
//! refuses (exit 127, empty stdout) therefore surfaces the loader's own line —
//! naming the missing library — inside the D-Bus error, not only in a journal
//! line that may have rotated away by the time the failure is investigated.
//!
//! ## Response hygiene
//!
//! The response carries filesystem paths, so the read buffer is a
//! [`Zeroizing`] allocation reserved to the expected size: the decoded
//! envelope is dropped as zeroed bytes rather than left in freed heap.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use atrium_arca_contract::{ARCA_CONTRACT_VERSION, FileChooserRequest, FileChooserResponse};
use zeroize::Zeroizing;

/// Overrides the resolved arca executable (tests and packagers).
const ARCA_ENV: &str = "ATRIUM_FILE_CHOOSER_PROMPTER";
/// The argv flag that puts arca into chooser mode.
const CHOOSER_ARG: &str = "--chooser-prompt";
const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Cap on the child's retained stderr when folding it into a failure message.
const STDERR_TAIL_BYTES: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub(crate) enum InvokeError {
    #[error("portal request was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

/// The versioned envelope written to arca's standard input.
#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ChooserEnvelope<'a> {
    version: u32,
    request: &'a FileChooserRequest,
}

/// The versioned envelope read from arca's standard output.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChooserReply {
    version: u32,
    response: FileChooserResponse,
}

/// Run one arca chooser for `request`.
///
/// Returns the response exactly as arca emitted it; the caller validates it
/// against the request with [`FileChooserResponse::validate_for`] before it
/// becomes a portal result.
pub(crate) fn invoke(
    request: &FileChooserRequest,
    cancellation: Option<&dyn Fn() -> bool>,
) -> Result<FileChooserResponse, InvokeError> {
    let executable = executable().map_err(InvokeError::Failed)?;
    let mut cmd = Command::new(&executable);
    cmd.arg(CHOOSER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|error| {
        InvokeError::Failed(format!("could not start {}: {error}", executable.display()))
    })?;

    // Take the pipes out of `child` before any early return: each of the
    // three drainers (stdin write, stdout reader, stderr tail) must be
    // dropped or joined on every path, or a pipe writer outlives the request.
    let stderr_tail = spawn_stderr_tail(child.stderr.take().expect("stderr was piped above"));
    let Some(mut stdin) = child.stdin.take() else {
        terminate(&mut child);
        let _ = stderr_tail.join();
        return Err(InvokeError::Failed("arca stdin was not piped".to_owned()));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        let _ = stderr_tail.join();
        return Err(InvokeError::Failed("arca stdout was not piped".to_owned()));
    };
    let envelope = ChooserEnvelope {
        version: ARCA_CONTRACT_VERSION,
        request,
    };
    let send_result = serde_json::to_writer(&mut stdin, &envelope)
        .map_err(|error| error.to_string())
        .and_then(|()| stdin.write_all(b"\n").map_err(|error| error.to_string()));
    // A child that dies before reading its request (the loader-refusal shape:
    // exit 127 before the first read) closes stdin's write end, so the write
    // fails with EPIPE. That is the child dying, not a transport failure — fall
    // through to the wait below so the exit status and the tee'd stderr line
    // reach the caller. Other write errors abort as before.
    if let Err(error) = &send_result {
        let is_broken_pipe = error.contains("Broken pipe")
            || error.contains("broken pipe")
            || error.to_lowercase().contains("epipe");
        if !is_broken_pipe {
            terminate(&mut child);
            let _ = stderr_tail.join();
            return Err(InvokeError::Failed(format!(
                "could not send the arca request: {error}"
            )));
        }
    }
    drop(stdin);

    let reader = std::thread::spawn(move || {
        // A file list is the largest response the chooser emits; reserving a
        // screenful upfront keeps the common case in one allocation.
        let mut bytes = Zeroizing::new(Vec::with_capacity(4096));
        stdout
            .take(MAX_MESSAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });

    let status = loop {
        if cancellation.is_some_and(|cancelled| cancelled()) {
            terminate(&mut child);
            let _ = reader.join();
            let _ = stderr_tail.join();
            return Err(InvokeError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate(&mut child);
                let _ = reader.join();
                let _ = stderr_tail.join();
                return Err(InvokeError::Failed(format!(
                    "could not wait for arca: {error}"
                )));
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| InvokeError::Failed("arca response reader panicked".to_owned()))?
        .map_err(|error| InvokeError::Failed(format!("could not read arca response: {error}")))?;
    // Join the stderr drainer after the response is read: the child has
    // exited, so its pipe is at EOF and the join is immediate. Joining before
    // the stdout read could deadlock a child blocked writing stderr ahead of
    // its response.
    let stderr_tail = stderr_tail.join().unwrap_or_default();
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(InvokeError::Failed(
            "arca response exceeds the 8 MiB contract limit".into(),
        ));
    }
    if bytes.is_empty() {
        // Exit code 127 is the dynamic loader's refusal: it cannot come from
        // arca's own code (usage errors are 2, contract failures 1). Fold the
        // loader's own stderr line into the error so the failure names the
        // missing library instead of a bare status.
        let diagnostic = (status.code() == Some(127))
            .then(|| stderr_diagnostic(&stderr_tail))
            .flatten();
        return Err(InvokeError::Failed(match diagnostic {
            Some(diagnostic) => format!("arca exited with {status}: {diagnostic}"),
            None => format!("arca exited with {status} and no response"),
        }));
    }
    let reply: ChooserReply = serde_json::from_slice(&bytes).map_err(|error| {
        let stdout_lossy = String::from_utf8_lossy(&bytes);
        let preview = if stdout_lossy.len() > 256 {
            format!("{}...", &stdout_lossy[..256])
        } else {
            stdout_lossy.into_owned()
        };
        InvokeError::Failed(format!(
            "arca exited with {status} and returned invalid JSON: {error}; stdout was: {preview:?}"
        ))
    })?;
    if reply.version != ARCA_CONTRACT_VERSION {
        return Err(InvokeError::Failed(format!(
            "arca answered contract version {}; expected {ARCA_CONTRACT_VERSION}",
            reply.version
        )));
    }
    Ok(reply.response)
}

/// Resolve the arca executable: the override, then a sibling of this binary,
/// then `PATH`, then the standard install directories.
fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(ARCA_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("arca");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("arca");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    for installed in [
        PathBuf::from("/usr/local/bin/arca"),
        PathBuf::from("/usr/bin/arca"),
    ] {
        if installed.is_file() {
            return Ok(installed);
        }
    }
    Err(format!(
        "arca was not found in PATH or standard bin directories; install arca or set {ARCA_ENV}"
    ))
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Live-forward the child's stderr while retaining its last
/// [`STDERR_TAIL_BYTES`] for failure reporting.
///
/// The thread exits when the pipe reaches EOF: on child death, on
/// [`terminate`], or after a normal exit. It must be joined on every return
/// path or a slow stderr writer could outlive the request.
fn spawn_stderr_tail(stderr: std::process::ChildStderr) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0_u8; 512];
        let mut sink = std::io::stderr();
        let mut stderr = stderr;
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => return tail,
                Ok(read) => {
                    let _ = sink.write_all(&chunk[..read]);
                    tail.extend_from_slice(&chunk[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                }
            }
        }
    })
}

/// Fold a retained stderr tail into one diagnostic line.
fn stderr_diagnostic(tail: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(tail);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next()?;
    let hint = "the arca image could not be started — usually a shared library it was linked \
                against is missing or was replaced under it; rebuild and reinstall arca";
    Some(match lines.next() {
        Some(second) => format!("{first}; {second} ({hint})"),
        None => format!("{first} ({hint})"),
    })
}
