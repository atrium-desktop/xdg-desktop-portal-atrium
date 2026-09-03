//! `xdg-desktop-portal-atrium` entry point: D-Bus-activated portal backend.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Vault passwords and prompt contents pass through this process's
    // memory; keep core dumps from being written for it. The secret buffers
    // additionally opt their own pages out of any dump image with
    // MADV_DONTDUMP, which is the layer that actually keeps key material out
    // of a piped core handler. Do NOT switch this to PR_SET_DUMPABLE=0: a
    // non-dumpable process's /proc/<pid>/exe is unreadable, which blinds the
    // compositor's kernel-verified identity check for built-in IPC scope
    // claims (ADR-0128) and severs every compositor-mediated portal function.
    // Best effort: a setrlimit failure (e.g. under a restrictive seccomp
    // filter) warns and never aborts startup. The logger is not initialized
    // yet, so the warning goes to stderr directly — the early-error channel
    // this file already uses.
    // SAFETY: setrlimit takes a valid pointer to an initialized rlimit.
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0 {
        let error = std::io::Error::last_os_error();
        eprintln!("xdg-desktop-portal-atrium: could not disable core dumps: {error}");
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match atrium_portal::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("xdg-desktop-portal-atrium: {error}");
            eprintln!("xdg-desktop-portal-atrium: {error}");
            ExitCode::FAILURE
        }
    }
}
