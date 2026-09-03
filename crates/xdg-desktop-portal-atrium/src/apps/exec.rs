//! Desktop Entry `Exec` handling: shell-style word splitting, the spec's
//! field-code expansion, and the detached launch itself.

use std::io;
use std::process::{Command, Stdio};

use super::{MAX_EXEC_BYTES, MAX_LAUNCH_URIS};

/// and reaping belongs to the session's init (or a future SIGCHLD reaper).
/// Launch `exec` with `uris`, expanding the Desktop Entry field codes, and
/// return without waiting. The child is deliberately unreaped: the portal
/// is a long-lived daemon whose launched applications outlive the request,
/// so reaping here would either block the request on the child's lifetime
/// or produce zombies. The kernel reparents the orphan when this daemon
/// exits; `SIGCHLD` is ignored process-wide for the same reason.
pub(crate) fn launch(exec: &str, uris: &[String]) -> io::Result<()> {
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
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
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
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16)
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}
