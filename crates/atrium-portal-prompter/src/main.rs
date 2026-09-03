//! One-shot optics (iris/lens) host for portal prompt requests.
//!
//! The backend writes one versioned JSON request to stdin; this process shows
//! the matching native dialog (file chooser, confirmation, secret password,
//! application chooser, or screen-source chooser) and writes one versioned
//! JSON response to stdout.
//! The wire contract lives in `atrium_portal_prompter`; this binary only
//! renders it.
//!
//! With `--notification-daemon` the process instead runs the long-lived
//! notification daemon (stream protocol in `atrium_portal_prompter::notify`,
//! UI in `ui::notify`).

use std::io::{Read, Write};
use std::process::ExitCode;

use atrium_portal_prompter::{
    PromptRequest, PromptResult, PrompterRequest, PrompterResponse, SecretResponse,
};

mod ui;
mod wire;

use wire::Wire;

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

// This crate does not depend on libc (unlike the daemon crate); declare
// the one syscall entry point needed to opt out of core dumps instead of
// growing the manifest for a single call. Linux-only, like the iris/lens
// stack this binary links.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn setrlimit(resource: std::ffi::c_int, rlim: *const Rlimit) -> std::ffi::c_int;
}

/// Linux `RLIMIT_CORE`: the core dump size resource.
#[cfg(target_os = "linux")]
const RLIMIT_CORE: std::ffi::c_int = 4;

/// Mirrors Linux `struct rlimit` (`rlim_t` is `unsigned long`).
#[cfg(target_os = "linux")]
#[repr(C)]
struct Rlimit {
    rlim_cur: std::ffi::c_ulong,
    rlim_max: std::ffi::c_ulong,
}

fn main() -> ExitCode {
    // Claim the protocol wire before anything else can touch stdout: the
    // optics C stack prints process-lifetime diagnostics, and a buffered
    // one flushed at exit corrupts a shared wire (see wire.rs).
    let mut wire = Wire::acquire();
    // Prompt requests can carry vault passwords through this process's
    // memory; keep core dumps from being written for it. The secret buffer
    // additionally opts its own pages out of any dump image with
    // MADV_DONTDUMP (see ui/secret_buffer.rs). Do NOT switch this to
    // PR_SET_DUMPABLE=0: a non-dumpable process's /proc/<pid>/exe is
    // unreadable, which blinds the compositor's kernel-verified identity
    // check for built-in IPC scope claims (ADR-0128). Best effort: a
    // setrlimit failure (e.g. under a restrictive seccomp filter) warns and
    // never aborts startup. The logger is not initialized yet, so the
    // warning goes to stderr directly.
    // SAFETY: setrlimit takes a valid pointer to an initialized rlimit.
    #[cfg(target_os = "linux")]
    {
        let no_core = Rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { setrlimit(RLIMIT_CORE, &no_core) } != 0 {
            let error = std::io::Error::last_os_error();
            eprintln!("atrium-portal-prompter: could not disable core dumps: {error}");
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if std::env::args().nth(1).as_deref() == Some("--notification-daemon") {
        return ui::notify::run_daemon(wire);
    }
    let response =
        match read_request().and_then(|(request, appearance)| run_dialog(request, appearance)) {
            Ok(response) => response,
            Err(message) => {
                log::error!("prompter: {message}");
                PrompterResponse::failed(message)
            }
        };
    match write_response(&mut wire, &response) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("prompter: could not write response: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read_request() -> Result<
    (
        PromptRequest,
        Option<atrium_portal_prompter::PromptAppearance>,
    ),
    String,
> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read request: {error}"))?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err("request exceeds the 8 MiB process-contract limit".into());
    }
    let request: PrompterRequest =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid request JSON: {error}"))?;
    let appearance = request.appearance;
    let prompt = request.into_prompt()?;
    Ok((prompt, appearance))
}

fn write_response(wire: &mut Wire, response: &PrompterResponse) -> Result<(), String> {
    // Pin a secret response's password bytes against swapping for their
    // short serialization lifetime. Best effort: the guard is None when the
    // value is empty, the platform has no mlock, or the rlimit refuses, and
    // serialization proceeds either way. serde reads the String in place,
    // so its heap address is stable while the guard is held.
    let _secret_lock = match &response.result {
        PromptResult::Secret(SecretResponse::Secret { value }) => {
            ui::secret_buffer::PageLock::new(value.as_bytes())
        }
        _ => None,
    };
    serde_json::to_writer(&mut *wire, response)
        .map_err(|error| format!("could not encode response: {error}"))?;
    wire.write_all(b"\n")
        .and_then(|()| wire.flush())
        .map_err(|error| error.to_string())
}

fn run_dialog(
    request: PromptRequest,
    appearance: Option<atrium_portal_prompter::PromptAppearance>,
) -> Result<PrompterResponse, String> {
    let result = match request {
        PromptRequest::FileChooser(request) => ui::file_chooser::run(request, appearance.as_ref())?,
        PromptRequest::Confirm(request) => ui::confirm::run(request, appearance.as_ref())?,
        PromptRequest::Secret(request) => ui::secret::run(request, appearance.as_ref())?,
        PromptRequest::ChooseApp(request) => ui::choose_app::run(request, appearance.as_ref())?,
        PromptRequest::ChooseSource(request) => {
            ui::choose_source::run(request, appearance.as_ref())?
        }
        PromptRequest::LauncherEdit(request) => {
            ui::launcher_edit::run(request, appearance.as_ref())?
        }
    };
    Ok(PrompterResponse {
        version: atrium_portal_prompter::PROCESS_CONTRACT_VERSION,
        result,
    })
}
