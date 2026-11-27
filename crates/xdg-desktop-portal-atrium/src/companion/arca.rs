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
use std::time::{Duration, Instant};

use atrium_arca_contract::{
    ARCA_CONTRACT_VERSION, FileChooserRequest, FileChooserResponse, MAX_REQUEST_BYTES,
};
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

/// Encode the exact bytes written to arca's standard input: the versioned
/// envelope plus the terminating newline.
///
/// The FileChooser interface calls this before enqueueing a request, so an
/// oversized request is refused as `InvalidArgs` up front instead of failing
/// inside arca after a child was spawned for it.
pub(crate) fn encode_request(request: &FileChooserRequest) -> Result<Vec<u8>, String> {
    let envelope = ChooserEnvelope {
        version: ARCA_CONTRACT_VERSION,
        request,
    };
    let mut bytes = serde_json::to_vec(&envelope)
        .map_err(|error| format!("could not encode FileChooser request: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "FileChooser request exceeds the {MAX_REQUEST_BYTES}-byte limit"
        ));
    }
    Ok(bytes)
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
    invoke_executable(&executable, request, cancellation)
}

/// [`invoke`] against an already-resolved chooser executable.
fn invoke_executable(
    executable: &std::path::Path,
    request: &FileChooserRequest,
    cancellation: Option<&dyn Fn() -> bool>,
) -> Result<FileChooserResponse, InvokeError> {
    let request_bytes = encode_request(request).map_err(InvokeError::Failed)?;
    let mut cmd = Command::new(executable);
    cmd.arg(CHOOSER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|error| {
        InvokeError::Failed(format!("could not start {}: {error}", executable.display()))
    })?;

    // Take the pipes out of `child` before any early return: each of the
    // three drainers (stdin writer, stdout reader, stderr tail) must be
    // joined on every path, or a pipe end outlives the request.
    let stderr_tail = spawn_stderr_tail(child.stderr.take().expect("stderr was piped above"));
    let Some(mut stdin) = child.stdin.take() else {
        terminate(&mut child);
        let _ = join_within(stderr_tail, Instant::now() + DRAIN_GRACE);
        return Err(InvokeError::Failed("arca stdin was not piped".to_owned()));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        let _ = join_within(stderr_tail, Instant::now() + DRAIN_GRACE);
        return Err(InvokeError::Failed("arca stdout was not piped".to_owned()));
    };

    // The request can exceed the pipe buffer, so it is written off-thread: a
    // child that stalls before reading stdin must not block the cancellation
    // poll below. Terminating the child breaks the pipe, which ends the write.
    let writer = std::thread::spawn(move || {
        let result = stdin.write_all(&request_bytes);
        drop(stdin);
        result
    });
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
            let deadline = Instant::now() + DRAIN_GRACE;
            let _ = join_within(writer, deadline);
            let _ = join_within(reader, deadline);
            let _ = join_within(stderr_tail, deadline);
            return Err(InvokeError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate(&mut child);
                let deadline = Instant::now() + DRAIN_GRACE;
                let _ = join_within(writer, deadline);
                let _ = join_within(reader, deadline);
                let _ = join_within(stderr_tail, deadline);
                return Err(InvokeError::Failed(format!(
                    "could not wait for arca: {error}"
                )));
            }
        }
    };

    // arca has exited, so every pipe reaches EOF unless a descendant it left
    // behind inherited one. arca is a fault boundary: such a descendant must
    // not park this worker, so the drainers get a bounded grace and are
    // detached (they finish once the descendant lets go) when it lapses.
    let deadline = Instant::now() + DRAIN_GRACE;
    let held_open = || {
        InvokeError::Failed(format!(
            "arca exited with {status} but a process it left behind still holds its pipes"
        ))
    };
    // A broken pipe means the child died before reading its request (the
    // loader-refusal shape: exit 127 before the first read); that is reported
    // below from the exit status and stderr, so only other write errors are
    // transport failures.
    let write_result = join_within(writer, deadline).ok_or_else(held_open)?;
    if let Err(error) = write_result
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        let _ = join_within(reader, deadline);
        let _ = join_within(stderr_tail, deadline);
        return Err(InvokeError::Failed(format!(
            "could not send the arca request: {error}"
        )));
    }
    let bytes = join_within(reader, deadline)
        .ok_or_else(held_open)?
        .map_err(|error| InvokeError::Failed(format!("could not read arca response: {error}")))?;
    // Join the stderr drainer after the response is read: joining it first
    // could deadlock a child blocked writing stderr ahead of its response. A
    // tail that is still held open only costs the diagnostic.
    let stderr_tail = join_within(stderr_tail, deadline).unwrap_or_default();
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
        let preview = match stdout_lossy.char_indices().nth(256) {
            Some((cut, _)) => format!("{}...", &stdout_lossy[..cut]),
            None => stdout_lossy.into_owned(),
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

/// How long the pipe drainers may take to reach EOF once arca has exited or
/// been terminated. EOF is immediate unless a descendant inherited a pipe.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Join `handle` if it finishes before `deadline`; otherwise detach it.
///
/// A panicked drainer is reported as `None`, like one that never finished:
/// either way its result is unavailable.
fn join_within<T>(handle: std::thread::JoinHandle<T>, deadline: Instant) -> Option<T> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().ok()
}

/// Terminate and reap `child`: send `SIGTERM`, allow a 250 ms grace window,
/// then escalate to `SIGKILL` if the process is still running.
///
/// `SIGTERM` gives a chooser that installs a handler the chance to exit on
/// its own terms; one that keeps the default disposition dies immediately,
/// exactly as it would under `SIGKILL`. Either way the child is reaped
/// before this returns, so no zombie outlives the request.
fn terminate(child: &mut Child) {
    const GRACE_WINDOW: Duration = Duration::from_millis(250);
    const POLL_STEP: Duration = Duration::from_millis(10);

    if let Ok(pid) = libc::pid_t::try_from(child.id())
        && pid > 0
    {
        // SAFETY: `pid` is the positive process ID of our un-reaped direct child
        // owned by `&mut Child`; sending `SIGTERM` has no memory-safety preconditions.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + GRACE_WINDOW;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(POLL_STEP),
                Err(_) => break,
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use atrium_arca_contract::FileChooserMode;

    fn open_request(title: String) -> FileChooserRequest {
        FileChooserRequest {
            mode: FileChooserMode::OpenFile,
            app_id: "dev.tessera.Test".into(),
            title,
            accept_label: None,
            modal: true,
            parent_window: None,
            multiple: false,
            current_folder: None,
            current_name: None,
            current_file: None,
            filters: Vec::new(),
            current_filter: None,
            choices: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Write `body` as an executable `/bin/sh` stand-in for arca.
    fn stand_in(dir: &std::path::Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("arca");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make stand-in executable");
        path
    }

    #[test]
    fn invoke_round_trips_one_v1_exchange() {
        let dir = tempfile::tempdir().expect("tempdir");
        let arca = stand_in(
            dir.path(),
            r#"read -r _ && printf '{"version":1,"response":{"status":"cancelled"}}'"#,
        );
        let response = invoke_executable(&arca, &open_request("Open".into()), None);
        assert!(
            matches!(response, Ok(FileChooserResponse::Cancelled)),
            "{response:?}"
        );
    }

    #[test]
    fn a_descendant_holding_the_pipes_cannot_park_the_worker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("descendant.pid");
        // Exit at once, leaving a background process that inherited all three
        // pipes and records its pid so the test can reap it.
        let arca = stand_in(
            dir.path(),
            &format!("sleep 30 & echo $! > '{}'; exit 0", pid_file.display()),
        );
        let started = Instant::now();
        let response = invoke_executable(&arca, &open_request("Open".into()), None);
        let elapsed = started.elapsed();
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
        {
            // SAFETY: plain signal delivery to the stand-in's background
            // `sleep`, which this test started; no memory is involved.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            matches!(&response, Err(InvokeError::Failed(message)) if message.contains("still holds its pipes")),
            "{response:?}"
        );
        assert!(
            elapsed < DRAIN_GRACE + Duration::from_secs(2),
            "the drain must be bounded, took {elapsed:?}"
        );
    }

    #[test]
    fn cancellation_is_observed_while_a_large_request_is_unread() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Never reads stdin; the request is far larger than a pipe buffer.
        let arca = stand_in(dir.path(), "exec sleep 30");
        let request = open_request("x".repeat(512 * 1024));
        let started = Instant::now();
        let cancel_at = started + Duration::from_millis(100);
        let cancelled = || Instant::now() >= cancel_at;
        let response = invoke_executable(&arca, &request, Some(&cancelled));
        assert!(
            matches!(response, Err(InvokeError::Cancelled)),
            "{response:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a blocked request write must not delay cancellation"
        );
    }

    #[test]
    fn v1_envelope_round_trips_and_rejects_retired_v6_reply() {
        let req = open_request("Open".into());
        let encoded = serde_json::to_value(ChooserEnvelope {
            version: ARCA_CONTRACT_VERSION,
            request: &req,
        })
        .expect("serialize request envelope");
        assert_eq!(encoded["version"], 1);
        assert!(encoded["request"].is_object());
        assert!(encoded.get("prompt").is_none());

        let v1_reply = br#"{"version":1,"response":{"status":"cancelled"}}"#;
        let decoded: ChooserReply = serde_json::from_slice(v1_reply).expect("decode v1 reply");
        assert_eq!(decoded.version, ARCA_CONTRACT_VERSION);
        assert_eq!(decoded.response, FileChooserResponse::Cancelled);

        // A retired v6 multi-prompt reply must fail strict decoding.
        let legacy_v6 =
            br#"{"version":6,"result":{"kind":"file_chooser","response":{"status":"cancelled"}}}"#;
        assert!(serde_json::from_slice::<ChooserReply>(legacy_v6).is_err());
    }

    /// Spawn `/bin/sh -c script` and wait until it prints `ready`, so its
    /// signal disposition is armed before the test signals it.
    fn spawn_armed(script: &str) -> Child {
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn shell child");
        let mut ready = [0u8; 5];
        child
            .stdout
            .take()
            .expect("stdout was piped")
            .read_exact(&mut ready)
            .expect("child armed its trap");
        child
    }

    #[test]
    fn terminate_delivers_sigterm_before_escalating() {
        use std::os::unix::process::ExitStatusExt as _;

        // A child that handles SIGTERM exits on its own terms with status 0.
        let mut graceful =
            spawn_armed("trap 'exit 0' TERM; printf ready; while :; do sleep 0.05; done");
        terminate(&mut graceful);
        let status = graceful
            .try_wait()
            .expect("query status")
            .expect("child was reaped");
        assert!(
            status.success(),
            "SIGTERM must reach the child first: {status:?}"
        );

        // A child that ignores SIGTERM is killed once the grace window ends.
        let mut stubborn = spawn_armed("trap '' TERM; printf ready; while :; do sleep 0.05; done");
        let started = std::time::Instant::now();
        terminate(&mut stubborn);
        let status = stubborn
            .try_wait()
            .expect("query status")
            .expect("child was reaped");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "escalation must follow the 250 ms grace window"
        );
    }
}
