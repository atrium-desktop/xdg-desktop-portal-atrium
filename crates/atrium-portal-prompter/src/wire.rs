//! The process's protocol output channel: a private duplicate of the
//! caller's stdout, claimed before any library code runs.
//!
//! The process contract makes stdout a one-way JSON wire, but the optics
//! C stack shares this process's stdio: anything a C library prints to
//! stdout is fully buffered on pipes and flushes at exit, landing *after*
//! the response and corrupting the wire (the backend's strict parse then
//! fails the request — this exact bug broke the portal's save dialogs).
//! [`Wire::acquire`] duplicates fd 1 for protocol use and aliases fd 1
//! onto stderr for the rest of the process lifetime, so a stray C
//! `printf` — or a Rust `println!` — becomes a journal-visible diagnostic
//! instead of protocol corruption.

use std::io::{self, Write};

// This crate does not depend on libc (see the setrlimit declaration in
// main.rs); declare the two entry points the wire guard needs. POSIX,
// but the prompter is Linux-only, like the iris/lens stack it links.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dup(oldfd: std::ffi::c_int) -> std::ffi::c_int;
    fn dup2(oldfd: std::ffi::c_int, newfd: std::ffi::c_int) -> std::ffi::c_int;
}

/// The private protocol channel; see the module docs. Implements
/// [`io::Write`], so serialization code treats it like the stdout it
/// replaces.
pub enum Wire {
    /// The duplicated original stdout — the only remaining writer on the
    /// protocol pipe once fd 1 is aliased to stderr.
    Private(std::fs::File),
    /// Plain stdout: the fallback when the fd dance was unavailable
    /// (e.g. descriptor exhaustion), which keeps the pre-guard behavior.
    Stdout(io::Stdout),
}

impl Wire {
    /// Claim the wire. Best effort: a `dup`/`dup2` failure falls back to
    /// plain stdout with a stderr note, never aborts startup.
    pub fn acquire() -> Wire {
        match acquire_privatized() {
            Some(wire) => wire,
            None => {
                eprintln!(
                    "atrium-portal-prompter: could not privatize stdout \
                     ({}); the protocol wire stays shared with library diagnostics",
                    io::Error::last_os_error()
                );
                Wire::Stdout(io::stdout())
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn acquire_privatized() -> Option<Wire> {
    use std::os::unix::io::FromRawFd as _;
    // SAFETY: both calls pass valid, open descriptors (1 = stdout,
    // 2 = stderr) and only duplicate or re-alias descriptor numbers; no
    // memory is shared. `copy` is immediately owned by a File, which
    // closes exactly that descriptor on drop.
    unsafe {
        let copy = dup(1);
        if copy < 0 {
            return None;
        }
        if dup2(2, 1) < 0 {
            drop(std::fs::File::from_raw_fd(copy));
            return None;
        }
        Some(Wire::Private(std::fs::File::from_raw_fd(copy)))
    }
}

#[cfg(not(target_os = "linux"))]
fn acquire_privatized() -> Option<Wire> {
    None
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Private(file) => file.write(buf),
            Wire::Stdout(stdout) => stdout.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Private(file) => file.flush(),
            Wire::Stdout(stdout) => stdout.flush(),
        }
    }
}
