//! Desktop Entry `Exec` handling: shell-style word splitting, the spec's
//! field-code expansion, and the detached launch itself.

use std::io;
use std::process::{Command, Stdio};

use super::{MAX_EXEC_BYTES, MAX_LAUNCH_URIS};

/// Launch `exec` with `uris`, expanding the Desktop Entry field codes, and
/// return without waiting for the application to exit.
///
/// The child must not outlive the daemon's accounting, but it must also not
/// be waited on: a launched application runs for as long as the user keeps it
/// open, and the portal request that started it returns in milliseconds.
/// Dropping the `Child` is therefore not an option either — it leaves a
/// zombie behind, and this process is a resident, D-Bus-activated daemon that
/// lives for the whole session.
///
/// The child is handed to a single process-wide reaper thread instead (the
/// same shape as the mailer reaper in the backend: one thread, non-blocking
/// `try_wait` sweeps, no signal handler and no global `SIGCHLD` disposition
/// to interfere with anything else this process runs).
pub fn launch(exec: &str, uris: &[String]) -> io::Result<()> {
    if exec.len() > MAX_EXEC_BYTES || uris.len() > MAX_LAUNCH_URIS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Exec line or URI list is oversized",
        ));
    }
    let argv = expand_exec(exec, uris);
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the Exec line expanded to nothing",
        ));
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    hand_to_reaper(child);
    Ok(())
}

/// Sweep interval for the reaper. Launched applications are long-lived, so
/// promptness does not matter; the interval only bounds how long a finished
/// child may linger before it is collected.
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Hand a spawned child to the shared reaper thread.
///
/// If the reaper cannot be installed (its thread will not start) or has
/// stopped, there is no one to collect the child, so it is killed and reaped
/// instead of being abandoned: a launch this process cannot account for would
/// be a zombie for the rest of the session. The launch still reports success —
/// the caller asked for the application to be started, and that already
/// happened — so the failure is surfaced in the log rather than as an error the
/// caller can no longer act on.
fn hand_to_reaper(child: std::process::Child) {
    use std::sync::OnceLock;

    static REAPER: OnceLock<Option<std::sync::mpsc::Sender<std::process::Child>>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<std::process::Child>();
        let spawned = std::thread::Builder::new()
            .name("atrium-portal-exec-reaper".to_owned())
            .spawn(move || {
                let mut children: Vec<std::process::Child> = Vec::new();
                loop {
                    // Drain whatever arrived since the last sweep, then poll.
                    loop {
                        match receiver.try_recv() {
                            Ok(child) => children.push(child),
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                // The process is tearing down: collect what
                                // can be collected and stop.
                                for mut child in children {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                }
                                return;
                            }
                        }
                    }
                    children.retain_mut(|child| match child.try_wait() {
                        // Still running: keep it for a later sweep.
                        Ok(None) => true,
                        // Exited and reaped by `try_wait`.
                        Ok(Some(_)) => false,
                        // `try_wait` failed; drop the handle rather than
                        // retry forever, and reap best-effort.
                        Err(error) => {
                            log::warn!("portal: could not reap launched process: {error}");
                            let _ = child.kill();
                            let _ = child.wait();
                            false
                        }
                    });
                    std::thread::sleep(REAP_INTERVAL);
                }
            });
        match spawned {
            Ok(_) => Some(sender),
            Err(error) => {
                log::error!("portal: could not start the exec reaper: {error}");
                None
            }
        }
    });

    match sender {
        Some(sender) => {
            if let Err(error) = sender.send(child) {
                let mut child = error.0;
                log::error!("portal: the exec reaper stopped; killing the launched process");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        None => {
            let mut child = child;
            log::error!("portal: no exec reaper; killing the launched process");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Split an `Exec` line into tokens honouring the spec's quoting: double
/// quotes group, and a backslash escapes the next character (inside quotes
/// only before `"`, `` ` ``, `$`, or `\`; elsewhere before anything).
pub(super) fn split_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut has_token = false;
    let mut chars = exec.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' => {
                quoted = !quoted;
                has_token = true;
            }
            '\\' => {
                let Some(&next) = chars.peek() else {
                    break;
                };
                if !quoted || matches!(next, '"' | '`' | '$' | '\\') {
                    token.push(next);
                    chars.next();
                } else {
                    token.push('\\');
                }
                has_token = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has_token {
                    tokens.push(std::mem::take(&mut token));
                    has_token = false;
                }
            }
            c => {
                token.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(token);
    }
    tokens
}

/// Expand one `Exec` line into an argv vector. Whole-token `%u`/`%U`/`%f`/
/// `%F` expand to one/many arguments; embedded codes expand inline (`%%`
/// to `%`, the metadata codes `%i`/`%c`/`%k` and unknown codes to
/// nothing). When no URI code appears the URIs are appended, matching the
/// spec's fallback. `%f`/`%F` keep only `file://` URIs, decoded to paths.
pub(super) fn expand_exec(exec: &str, uris: &[String]) -> Vec<String> {
    let mut argv = Vec::new();
    let mut used_uris = false;
    for token in split_exec(exec) {
        match token.as_str() {
            "%u" => {
                used_uris = true;
                if let Some(first) = uris.first() {
                    argv.push(first.clone());
                }
            }
            "%U" => {
                used_uris = true;
                argv.extend(uris.iter().cloned());
            }
            "%f" => {
                used_uris = true;
                if let Some(path) = uris.first().and_then(|uri| uri_to_path(uri)) {
                    argv.push(path);
                }
            }
            "%F" => {
                used_uris = true;
                argv.extend(uris.iter().filter_map(|uri| uri_to_path(uri)));
            }
            _ => {
                let mut expanded = String::new();
                let mut chars = token.chars().peekable();
                while let Some(character) = chars.next() {
                    if character != '%' {
                        expanded.push(character);
                        continue;
                    }
                    let Some(code) = chars.next() else {
                        break;
                    };
                    used_uris |= matches!(code, 'u' | 'U' | 'f' | 'F');
                    match code {
                        '%' => expanded.push('%'),
                        'u' => {
                            if let Some(first) = uris.first() {
                                expanded.push_str(first);
                            }
                        }
                        'U' => expanded.push_str(&uris.join(" ")),
                        'f' => {
                            if let Some(path) = uris.first().and_then(|uri| uri_to_path(uri)) {
                                expanded.push_str(&path);
                            }
                        }
                        'F' => expanded.push_str(
                            &uris
                                .iter()
                                .filter_map(|uri| uri_to_path(uri))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        // %i/%c/%k need entry metadata this surface does
                        // not take; unknown codes are dropped (spec).
                        _ => {}
                    }
                }
                if !expanded.is_empty() {
                    argv.push(expanded);
                }
            }
        }
    }
    if !used_uris {
        argv.extend(uris.iter().cloned());
    }
    argv
}

/// Decode a `file://` URI into a local path; anything else returns `None`.
fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let path = path.strip_prefix('/').map(|tail| format!("/{tail}"))?;
    percent_decode(&path)
}

/// Decode `%XX` escapes; malformed sequences pass through unchanged.
///
/// Works on bytes throughout. Slicing the `&str` at `index + 1..index + 3`
/// would panic whenever the byte after `%` begins a multi-byte character
/// (`%€`), because those offsets are not char boundaries — an application can
/// put arbitrary bytes in a URI, so this must not be reachable.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push((high << 4) | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// The value of one ASCII hex digit, or `None` for anything else.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A launched child must be collected once it exits, not abandoned as a
    /// zombie for the life of this resident daemon.
    #[test]
    fn a_launched_child_is_reaped() {
        // `true` exits immediately; without a reaper it would linger as a
        // zombie until the daemon exits.
        launch("/bin/true", &[]).expect("launch /bin/true");
        // The reaper sweeps on an interval; poll for the child to disappear
        // from the process table rather than assuming a fixed delay.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !has_zombie_child() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the launched child was never reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Whether this process currently has a zombie child.
    ///
    /// Reads `/proc/<this pid>/task/<tid>/children`… is not enough (it lists
    /// live children), so scan `/proc` for a `Z`-state process whose parent is
    /// this one.
    fn has_zombie_child() -> bool {
        let me = std::process::id().to_string();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // Format: pid (comm) state ppid ... ; comm may contain spaces and
            // parentheses, so split after the last ')'.
            let Some((_, rest)) = stat.rsplit_once(')') else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next().unwrap_or("");
            let ppid = fields.next().unwrap_or("");
            if state == "Z" && ppid == me {
                return true;
            }
        }
        false
    }

    /// `%` followed by a multi-byte character must not panic. `percent_decode`
    /// previously sliced at `index + 1..index + 3`, which is not a char
    /// boundary for a multi-byte character.
    #[test]
    fn percent_decode_tolerates_multibyte_after_percent() {
        // Used to panic on the non-boundary slice: `%` followed by the
        // multi-byte `€` leaves a malformed escape, which passes through.
        assert_eq!(
            percent_decode("file:///tmp/x%€").as_deref(),
            Some("file:///tmp/x%€")
        );
        assert_eq!(percent_decode("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(percent_decode("%zz").as_deref(), Some("%zz"));
        // A truncated escape at the very end is also unchanged, not a panic.
        assert_eq!(percent_decode("tail%").as_deref(), Some("tail%"));
        assert_eq!(percent_decode("tail%4").as_deref(), Some("tail%4"));
    }
}
