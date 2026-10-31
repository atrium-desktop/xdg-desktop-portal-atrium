//! Atomic file writes under a caller-chosen, owner-verified directory.
//!
//! Every payload the portal persists (a screenshot, a staged wallpaper, the
//! ScreenCast restore store) must be either absent or complete: a consumer
//! that reads while the portal writes must never observe a torn file. The
//! discipline is create-temp-then-rename inside a directory the portal has
//! proved it owns, with mode `0600` throughout.

use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rand::RngCore;

/// Create-temp-then-rename under `dir`, mode 0600, so no consumer observes a
/// partial payload. The directory is created when missing and is always
/// tightened to mode 0700: a portal data directory holds names and pixels from
/// private screenshots, so the process umask must not decide its privacy.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // Open without following the final component and validate before chmod:
    // otherwise an attacker-controlled symlink could make the portal change
    // permissions on an unrelated directory.
    let directory = open_owned_dir(dir)?;
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    if directory.metadata()?.permissions().mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "portal data directory mode must be 0700",
        ));
    }

    let path = dir.join(name);
    let temp = dir.join(format!(".{name}.{}.tmp", random_suffix()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        // Make the directory entry durable too. Without this, a successful
        // response can still point at a file lost after sudden power loss.
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map(|()| path)
}

/// Read `name` from `dir` under the same discipline [`write_atomic`] uses.
///
/// The read side needs the mirror of the write side's protection: the store
/// lives in the user's data directory, so another process running as the same
/// user can replace it with a symlink. Following that link would let the
/// reader be steered at a file the writer never produced — and the ScreenCast
/// restore store *grants capture consent*, so a substituted file is not a
/// cosmetic problem. `O_NOFOLLOW` on the final component plus a regular-file
/// and owner check keeps the read on files this daemon could have written.
///
/// A missing file, a symlink, a non-regular file, a file owned by another
/// user, or one above `max_bytes` all yield `Ok(None)`: each means "no usable
/// stored payload", which every caller already handles as the empty case.
pub(crate) fn read_atomic(dir: &Path, name: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;

    let path = dir.join(name);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        // Absent, or a symlink we refuse to follow: both are "nothing stored".
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: `geteuid` reads the process's effective uid and touches no
    // caller memory.
    let euid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != euid {
        return Ok(None);
    }
    if metadata.len() > max_bytes {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    // Bound the read as well: the file could grow between the metadata check
    // and the read.
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Open `dir` as a real, user-owned directory without following a symlink.
fn open_owned_dir(dir: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = file.metadata()?;
    // SAFETY: `geteuid` reads the process's effective uid and touches no
    // caller memory.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "portal data directory must be a user-owned real directory",
        ));
    }
    Ok(file)
}

/// Remove `dir` and its contents, after proving it is a user-owned real
/// directory with the same `O_DIRECTORY|O_NOFOLLOW` discipline as
/// [`write_atomic`]. A missing directory is not an error.
pub(crate) fn remove_owned_dir(dir: &Path) -> io::Result<()> {
    match open_owned_dir(dir) {
        Ok(_) => std::fs::remove_dir_all(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// A random 128-bit hex suffix that keeps concurrent writers apart.
fn random_suffix() -> String {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atrium-fsx-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn writes_are_private_and_replace_atomically() {
        let dir = scratch("atomic");
        let first = write_atomic(&dir, "payload.bin", b"one").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        let mode = std::fs::metadata(&first).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "payload must be owner-only");

        // Replacing leaves no temp file behind.
        write_atomic(&dir, "payload.bin", b"two").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"two");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["payload.bin"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let root = scratch("symlink");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            write_atomic(&link, "x", b"x").is_err(),
            "a symlinked target must be refused"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_returns_what_the_writer_stored() {
        let dir = scratch("read-roundtrip");
        write_atomic(&dir, "store.json", b"{\"version\":1}").unwrap();
        let bytes = read_atomic(&dir, "store.json", 1024)
            .unwrap()
            .expect("a written payload must read back");
        assert_eq!(bytes, b"{\"version\":1}");

        // An absent payload is the empty case, not an error.
        assert!(read_atomic(&dir, "absent.json", 1024).unwrap().is_none());
        // An oversized payload is refused rather than partially returned.
        assert!(read_atomic(&dir, "store.json", 4).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The store grants capture consent, so a symlink planted in the data
    /// directory must never be followed — it would let another process of the
    /// same user substitute a token file the daemon never wrote.
    #[test]
    fn a_symlinked_payload_is_not_followed() {
        let dir = scratch("read-symlink");
        let secret = dir.join("elsewhere.json");
        std::fs::write(&secret, b"{\"version\":1,\"tokens\":[]}").unwrap();
        let link = dir.join("store.json");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        assert!(
            read_atomic(&dir, "store.json", 1024).unwrap().is_none(),
            "a symlinked payload must read as absent, not be followed"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory (or any non-regular file) is not a stored payload.
    #[test]
    fn a_non_regular_payload_is_refused() {
        let dir = scratch("read-nonregular");
        std::fs::create_dir_all(dir.join("store.json")).unwrap();
        assert!(read_atomic(&dir, "store.json", 1024).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removing_an_absent_directory_is_not_an_error() {
        let root = scratch("absent");
        let missing = root.join("missing");
        assert!(remove_owned_dir(&missing).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
