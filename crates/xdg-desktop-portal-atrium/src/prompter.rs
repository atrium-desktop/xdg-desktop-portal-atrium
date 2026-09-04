//! Supervision for an out-of-process prompter process (such as
//! arca --chooser-prompt, or a test fixture prompter).
//!
//! ## Lifetime and hang policy
//!
//! Each request spawns one prompter, writes one JSON request, and waits for
//! the process to exit — the dialog's window lifetime *is* the request's
//! lifetime, so a user who leaves a dialog open keeps its child alive for
//! as long as they like. There is deliberately **no wall-clock timeout**:
//! any bound short enough to be safe would kill legitimate sessions (a
//! user reading a folder tree or weighing a consent prompt), and the
//! prompter owns no locks or exclusive resources the daemon needs. The
//! bound that does exist is *count*, not time:
//!
//! - `Request.Close` (delivered through the `cancellation` closure)
//!   terminates the child immediately and answers the caller with code 1;
//! - each interface caps concurrent dialogs (for example
//!   `MAX_ACTIVE_FILE_CHOOSERS`), so a caller that spawns-and-forgets
//!   saturates its own cap and further requests are refused with code 2
//!   instead of accumulating without end;
//! - a crashed or hung-with-closed-stdout child is reaped by the exit
//!   poll below; a child that never exits and never answers stays alive
//!   only until the frontend gives up on the request.
//!
//! The wait is a 20 ms `try_wait` poll so that `cancellation` is observed
//! without a signal: one sleeping thread per open dialog is the accepted
//! cost (an in-flight dialog is user-visible work, not idle load).
//!
//! ## Failure visibility
//!
//! The child's stderr is teed: forwarded live to the daemon's stderr
//! (preserving ADR-0014's diagnostics channel) while its tail is retained
//! for failure reporting. A child the dynamic loader refuses (exit 127,
//! empty stdout) therefore surfaces the loader's own line — naming the
//! missing library — inside the D-Bus error, not only in a journal line
//! that may have rotated away by the time the failure is investigated.
//!
//! ## Response hygiene
//!
//! The response can carry a vault password: the read buffer is a
//! `Zeroizing` pre-sized to 1 KiB (the realistic no-realloc size for
//! secret-bearing answers), `mlock`'d while parsed (best effort), and the
//! backend runs with core dumps disabled. See ADR-0009 for the vault
//! lifecycle and ADR-0014 for why the prompter's stdout is private to the
//! contract.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use atrium_portal_prompter::{PromptResult, PrompterRequest, PrompterResponse};
use zeroize::Zeroizing;

use crate::settings::SettingsStore;

const PROMPTER_ENV: &str = "ATRIUM_PORTAL_PROMPTER";
const FILE_CHOOSER_ENV: &str = "ATRIUM_FILE_CHOOSER_PROMPTER";
const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum InvokeError {
    #[error("portal request was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

/// Invoke one prompter, stamping the compositor appearance snapshot from
/// `settings` onto the request. Every prompt call site funnels through
/// here so no dialog can miss the appearance contract.
pub(crate) fn invoke(
    mut request: PrompterRequest,
    settings: Option<&SettingsStore>,
    cancellation: Option<&dyn Fn() -> bool>,
) -> Result<PromptResult, InvokeError> {
    if let Some(appearance) = settings.map(crate::settings::prompt_appearance_of) {
        request = request.with_appearance(appearance);
    }
    invoke_raw(request, cancellation)
}

/// Cap on the child's retained stderr when folding it into a failure
/// message. Loader errors (the failure this exists for) are a single
/// short line; the cap only stops a chatty diagnostics stream from
/// inflating the D-Bus error.
const STDERR_TAIL_BYTES: usize = 2048;

/// Live-forward the child's stderr while retaining its last
/// [`STDERR_TAIL_BYTES`] for failure reporting.
///
/// The prompter's stderr is its diagnostics channel (ADR-0014), and the
/// dynamic loader reports a refused image there before the program ever
/// starts — exit code 127 with no stdout. Forwarding preserves today's
/// live journal stream; the tail moves the loader's own message into the
/// D-Bus error, where it survives log rotation and is actionable (this
/// is exactly how an optics soname bump presents when the prompter is
/// not relinked).
///
/// The thread exits when the pipe reaches EOF: on child death, on
/// [`terminate`](fn@terminate), or after a normal exit. It must be
/// joined on every return path or a slow stderr writer could outlive the
/// request.
fn spawn_stderr_tail(stderr: std::process::ChildStderr) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        use std::io::Read;
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
    let hint = "the prompter image could not be started — usually a shared library it was \
                linked against is missing or was replaced under it; rebuild and reinstall the \
                prompter against the installed optics release";
    Some(match lines.next() {
        Some(second) => format!("{first}; {second} ({hint})"),
        None => format!("{first} ({hint})"),
    })
}

/// Spawn the prompter and speak the one-shot contract without touching
/// the request.
fn invoke_raw(
    request: PrompterRequest,
    cancellation: Option<&dyn Fn() -> bool>,
) -> Result<PromptResult, InvokeError> {
    let (executable, args) = prompt_command(&request).map_err(InvokeError::Failed)?;
    let mut cmd = Command::new(&executable);
    for arg in &args {
        cmd.arg(arg);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            InvokeError::Failed(format!("could not start {}: {error}", executable.display()))
        })?;

    // Take the pipes out of `child` before any early return: each of the
    // three drainers (stdin write, stdout reader, stderr tail) must be
    // dropped or joined on every path, or a pipe writer outlives the
    // request. The stderr tail is started first so the early-return paths
    // below have exactly one handle to join.
    let stderr_tail = spawn_stderr_tail(child.stderr.take().expect("stderr was piped above"));
    let Some(mut stdin) = child.stdin.take() else {
        terminate(&mut child);
        let _ = stderr_tail.join();
        return Err(InvokeError::Failed(
            "prompter stdin was not piped".to_owned(),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        let _ = stderr_tail.join();
        return Err(InvokeError::Failed(
            "prompter stdout was not piped".to_owned(),
        ));
    };
    let send_result = serde_json::to_writer(&mut stdin, &request)
        .map_err(|error| error.to_string())
        .and_then(|()| stdin.write_all(b"\n").map_err(|error| error.to_string()));
    // A prompter that dies before reading its request (the loader-refusal
    // shape: exit 127 before the first read) closes stdin's write end, so
    // the write fails with EPIPE. That is the child dying, not a transport
    // failure — fall through to the wait below so the exit status and the
    // tee'd stderr line (which name the real cause) reach the caller.
    // Other write errors (a closed pipe on our side) abort as before.
    if let Err(error) = &send_result {
        let is_broken_pipe = error.contains("Broken pipe")
            || error.contains("broken pipe")
            || error.to_lowercase().contains("epipe");
        if !is_broken_pipe {
            terminate(&mut child);
            let _ = stderr_tail.join();
            return Err(InvokeError::Failed(format!(
                "could not send prompter request: {error}"
            )));
        }
    }
    drop(stdin);

    let reader = std::thread::spawn(move || {
        // Responses that carry secrets (passwords, choices) are small
        // JSON documents, well under 1 KiB: reserving that upfront keeps
        // them in a single allocation, so no growth reallocation can
        // leave an unzeroized copy in freed heap. Larger no-secret
        // responses (file lists) may still reallocate.
        let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
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
                    "could not wait for prompter: {error}"
                )));
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| InvokeError::Failed("prompter response reader panicked".to_owned()))?
        .map_err(|error| {
            InvokeError::Failed(format!("could not read prompter response: {error}"))
        })?;
    // Join the stderr drainer after the response is read: the child has
    // exited, so its pipe is at EOF and the join is immediate. Joining
    // before the stdout read could deadlock a child blocked writing
    // stderr ahead of its response.
    let stderr_tail = stderr_tail.join().unwrap_or_default();
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(InvokeError::Failed(
            "prompter response exceeds the 8 MiB process-contract limit".into(),
        ));
    }
    if bytes.is_empty() {
        // Exit code 127 is the dynamic loader's refusal: it cannot come
        // from the prompter's own code (usage errors are 2, contract
        // failures 1). Fold the loader's own stderr line into the error
        // so the failure names the missing library instead of a bare
        // status — this is how an optics soname bump presents when the
        // prompter is not relinked.
        let diagnostic = (status.code() == Some(127))
            .then(|| stderr_diagnostic(&stderr_tail))
            .flatten();
        return Err(InvokeError::Failed(match diagnostic {
            Some(diagnostic) => format!("prompter exited with {status}: {diagnostic}"),
            None => format!("prompter exited with {status} and no response"),
        }));
    }
    // The read buffer has reached its final size and is only borrowed from
    // here on; pin it against swapping while the response (which can carry
    // a vault password) is parsed. Best effort: the guard is None when the
    // platform or rlimit refuses, and parsing proceeds either way.
    let _response_lock = MlockGuard::new(&bytes);
    let decoded = serde_json::from_slice(&bytes);
    let response: PrompterResponse = decoded.map_err(|error| {
        InvokeError::Failed(format!(
            "prompter exited with {status} and returned invalid JSON: {error}"
        ))
    })?;
    match response.into_result().map_err(InvokeError::Failed)? {
        PromptResult::Failed { message } => Err(InvokeError::Failed(message)),
        result => Ok(result),
    }
}

fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort `mlock` guard for the prompter's response bytes, which can
/// carry a vault password: pins the fully-read buffer against swapping
/// while it is alive, `munlock`ing on drop. `None` from [`MlockGuard::new`]
/// means the region is empty or the platform/rlimit refused; the caller
/// proceeds without a lock in both cases.
struct MlockGuard {
    addr: *const libc::c_void,
    len: usize,
}

impl MlockGuard {
    fn new(bytes: &[u8]) -> Option<MlockGuard> {
        if bytes.is_empty() {
            return None;
        }
        // SAFETY: `bytes` is a valid readable region owned by this process;
        // mlock only pins its pages against swapping. Failure (for example
        // RLIMIT_MEMLOCK) is non-fatal.
        let result = unsafe { libc::mlock(bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        if result != 0 {
            log::warn!(
                "portal: could not mlock the prompter response: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(MlockGuard {
            addr: bytes.as_ptr().cast::<libc::c_void>(),
            len: bytes.len(),
        })
    }
}

impl Drop for MlockGuard {
    fn drop(&mut self) {
        // SAFETY: the region was successfully mlock'd in `new`; the owning
        // `Zeroizing<Vec<u8>>` is only read (never reallocated) while the
        // guard is alive, so the recorded address is still exact, and the
        // Vec's zeroize-on-drop runs after the guard is released.
        unsafe { libc::munlock(self.addr, self.len) };
    }
}

/// The prompter executable's path: `$ATRIUM_PORTAL_PROMPTER`, then beside
/// the backend, then the standard libexec directories. Shared by the
/// one-shot invocation and the notification daemon spawn.
pub(crate) fn prompt_command(
    request: &PrompterRequest,
) -> Result<(PathBuf, Vec<&'static str>), String> {
    if matches!(
        request.prompt,
        atrium_portal_prompter::PromptRequest::FileChooser(_)
    ) {
        if let Some(path) = std::env::var_os(FILE_CHOOSER_ENV).filter(|path| !path.is_empty()) {
            return Ok((PathBuf::from(path), vec!["--chooser-prompt"]));
        }
        if let Some(path) = std::env::var_os(PROMPTER_ENV).filter(|path| !path.is_empty()) {
            return Ok((PathBuf::from(path), Vec::new()));
        }
        if let Ok(current) = std::env::current_exe()
            && let Some(directory) = current.parent()
        {
            let sibling = directory.join("arca");
            if sibling.is_file() {
                return Ok((sibling, vec!["--chooser-prompt"]));
            }
        }
        for installed in [
            PathBuf::from("/usr/bin/arca"),
            PathBuf::from("/usr/local/bin/arca"),
        ] {
            if installed.is_file() {
                return Ok((installed, vec!["--chooser-prompt"]));
            }
        }
    }
    executable().map(|path| (path, Vec::new()))
}

pub(crate) fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(PROMPTER_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("atrium-portal-prompter");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    for installed in [
        PathBuf::from("/usr/libexec/atrium-portal-prompter"),
        PathBuf::from("/usr/lib/atrium-portal-prompter"),
    ] {
        if installed.is_file() {
            return Ok(installed);
        }
    }
    Err(format!(
        "atrium-portal-prompter was not found beside the backend or in the standard libexec directories; set {PROMPTER_ENV}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `invoke` resolves its executable through the process environment, so
    /// the tests serialize on one lock and restore the variable after.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set the prompter override under the [`ENV_LOCK`] mutex.
    ///
    /// # Safety (caller: the test harness)
    /// Rust 2024 marks `set_var` unsafe because a concurrent reader could
    /// observe a torn value. These tests are the only writers of this
    /// variable in the test binary, every access holds `ENV_LOCK`, and the
    /// reader they race with (`invoke`) runs on the same test thread that
    /// performed the write, so no concurrent observation exists.
    fn set_override(value: Option<&std::ffi::OsStr>) {
        match value {
            Some(value) => unsafe { std::env::set_var(PROMPTER_ENV, value) },
            None => unsafe { std::env::remove_var(PROMPTER_ENV) },
        }
    }

    struct PrompterOverride<'a> {
        _guard: std::sync::MutexGuard<'a, ()>,
        prior: Option<std::ffi::OsString>,
    }

    impl PrompterOverride<'_> {
        fn point_at(&self, script: &std::path::Path) {
            set_override(Some(script.as_os_str()));
        }

        fn install() -> PrompterOverride<'static> {
            let guard = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prior = std::env::var_os(PROMPTER_ENV);
            PrompterOverride {
                _guard: guard,
                prior,
            }
        }
    }

    impl Drop for PrompterOverride<'_> {
        fn drop(&mut self) {
            set_override(self.prior.as_deref());
        }
    }

    /// Write an executable `sh` script under `dir` acting as the prompter.
    fn fake_prompter(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut mode = std::fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&path, mode).unwrap();
        path
    }

    fn confirm_request() -> PrompterRequest {
        use atrium_portal_prompter::{ConfirmRequest, PromptRequest};
        PrompterRequest {
            version: atrium_portal_prompter::PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::Confirm(ConfirmRequest {
                title: "test".to_owned(),
                body: "body".to_owned(),
                accept_label: None,
                deny_label: None,
                modal: false,
                parent_window: None,
            }),
            appearance: None,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tessera-prompter-unit-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_valid_response_decodes() {
        let dir = temp_dir("ok");
        let script = fake_prompter(
            &dir,
            "ok.sh",
            // A minimal confirm response: accepted with no results.
            r#"printf '%s' '{"version":6,"result":{"kind":"confirm","response":{"status":"confirmed"}}}'"#,
        );
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let result = invoke(confirm_request(), None, None);
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(PromptResult::Confirm(_)) => {}
            other => panic!("expected a confirmed dialog, got {other:?}"),
        }
    }

    #[test]
    fn a_crashed_prompter_reports_failure() {
        let dir = temp_dir("crash");
        let script = fake_prompter(&dir, "crash.sh", "exit 3");
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let error = invoke(confirm_request(), None, None)
            .expect_err("a crashing prompter must not succeed");
        let _ = std::fs::remove_dir_all(&dir);
        let InvokeError::Failed(message) = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(message.contains("no response"), "unexpected: {message}");
    }

    #[test]
    fn a_loader_refusal_names_the_missing_library() {
        let dir = temp_dir("loader127");
        // Reproduce the dynamic loader's refusal shape: exit 127 with the
        // loader's own stderr line and no stdout. The tee must fold that
        // line into the D-Bus error.
        let script = fake_prompter(
            &dir,
            "loader127.sh",
            "echo '/usr/libexec/atrium-portal-prompter: error while loading shared libraries: \
             libflux.so.0: cannot open shared object file: No such file or directory' >&2\n\
             exit 127",
        );
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let error = invoke(confirm_request(), None, None)
            .expect_err("a loader-refused prompter must not succeed");
        let _ = std::fs::remove_dir_all(&dir);
        let InvokeError::Failed(message) = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(
            message.contains("libflux.so.0"),
            "the error must name the missing library: {message}"
        );
        assert!(
            message.contains("error while loading shared libraries"),
            "the error must carry the loader's own line: {message}"
        );
    }

    #[test]
    fn a_silent_exit_127_still_reports_the_status() {
        let dir = temp_dir("silent127");
        // Nothing on stderr: the diagnostic folding has nothing to add,
        // so the status alone must still be reported (the pre-tee
        // behaviour).
        let script = fake_prompter(&dir, "silent127.sh", "exit 127");
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let error =
            invoke(confirm_request(), None, None).expect_err("a silent 127 must not succeed");
        let _ = std::fs::remove_dir_all(&dir);
        let InvokeError::Failed(message) = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(
            message.contains("exit status: 127"),
            "unexpected: {message}"
        );
        assert!(
            !message.contains("shared libraries"),
            "no diagnostic must be invented: {message}"
        );
    }

    #[test]
    fn invalid_json_reports_failure() {
        let dir = temp_dir("badjson");
        let script = fake_prompter(&dir, "badjson.sh", "printf 'not-json'");
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let error = invoke(confirm_request(), None, None).expect_err("bad JSON must fail");
        let _ = std::fs::remove_dir_all(&dir);
        let InvokeError::Failed(message) = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(message.contains("invalid JSON"), "unexpected: {message}");
    }

    #[test]
    fn an_oversized_response_is_refused() {
        let dir = temp_dir("oversize");
        // Emit more than MAX_MESSAGE_BYTES so the post-read bound check
        // fires; dd from /dev/zero keeps the script small.
        let script = fake_prompter(
            &dir,
            "oversize.sh",
            "dd if=/dev/zero bs=1048576 count=9 2>/dev/null",
        );
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);
        let error = invoke(confirm_request(), None, None).expect_err("oversized output must fail");
        let _ = std::fs::remove_dir_all(&dir);
        let InvokeError::Failed(message) = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(message.contains("8 MiB"), "unexpected: {message}");
    }

    #[test]
    fn cancellation_kills_the_child_and_answers_cancelled() {
        let dir = temp_dir("cancel");
        // The child never answers on its own; it records its liveness into
        // a pid file so the test can prove it died after cancellation.
        let marker = dir.join("alive");
        let script = fake_prompter(
            &dir,
            "hang.sh",
            &format!(
                "printf '%s' $$ > {}\nwhile :; do sleep 0.1; done",
                marker.display()
            ),
        );
        let override_env = PrompterOverride::install();
        override_env.point_at(&script);

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = cancelled.clone();
        let cancel = move || observed.load(std::sync::atomic::Ordering::SeqCst);
        let handle = std::thread::spawn(move || invoke(confirm_request(), None, Some(&cancel)));
        // Give the child a moment to start and record its pid.
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                break text.trim().to_owned();
            }
            if handle.is_finished() {
                panic!("invoke returned before cancellation was observed");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = handle
            .join()
            .unwrap()
            .expect_err("cancellation must not succeed");

        // The recorded pid must be gone: prove the child was killed rather
        // than orphaned. `kill -0` fails for a dead pid.
        let reaped = (|| {
            for _ in 0..100 {
                let status = std::process::Command::new("kill")
                    .args(["-0", &pid])
                    .status();
                if !matches!(status, Ok(code) if code.success()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        })();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(error, InvokeError::Cancelled), "got {error:?}");
        assert!(
            reaped,
            "the hung prompter (pid {pid}) survived cancellation"
        );
    }
}
