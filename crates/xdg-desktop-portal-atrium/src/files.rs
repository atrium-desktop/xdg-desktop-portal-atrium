//! Portal capture persistence: cache-directory resolution, atomic PNG
//! writes, and `file://` URI rendering.
//!
//! Portal screenshots are not user photo-library material — the frontend may
//! copy them wherever the application asked — so they live under
//! `$XDG_CACHE_HOME/xdg-desktop-portal-atrium` (falling back to
//! `$XDG_RUNTIME_DIR/xdg-desktop-portal-atrium` per the same spec carve-out
//! that portals themselves use). Files are
//! written mode `0600` via create-temp-then-rename so a consumer can never
//! observe a partial PNG.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rand::RngCore;

/// The portal capture cache directory:
/// `$XDG_CACHE_HOME/xdg-desktop-portal-atrium`, else
/// `$XDG_RUNTIME_DIR/xdg-desktop-portal-atrium`, else `None` (the request
/// fails with response code 2).
pub(crate) fn cache_dir() -> Option<PathBuf> {
    cache_dir_from(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

/// Split out for tests: environment variables are process-global.
fn cache_dir_from(
    cache: Option<std::ffi::OsString>,
    runtime: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    cache
        .filter(|dir| !dir.is_empty())
        .or_else(|| runtime.filter(|dir| !dir.is_empty()))
        .map(|base| PathBuf::from(base).join("xdg-desktop-portal-atrium"))
}

/// Write `png` as `screenshot-<millis>-<token>.png` under `dir`, atomically,
/// returning the final path. `token` is already sanitized to `[A-Za-z0-9_]`
/// by the option parser, so the filename cannot escape `dir`.
pub(crate) fn write_capture(dir: &Path, token: &str, png: &[u8]) -> io::Result<PathBuf> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let nonce = random_suffix();
    write_atomic(
        dir,
        &format!("screenshot-{millis}-{token}-{nonce}.png"),
        png,
    )
}

/// Create-temp-then-rename under `dir`, mode 0600, so no consumer observes a
/// partial payload. Shared by the screenshot cache and the wallpaper staging
/// directory.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // Open without following the final component and validate before chmod.
    // Otherwise an attacker-controlled cache symlink could make the portal
    // change permissions on an unrelated directory.
    let directory = open_owned_dir(dir)?;
    // The directory contains names and pixels from private screenshots. Do
    // not rely on the process umask to keep it private.
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    if directory.metadata()?.permissions().mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "portal data directory mode must be 0700",
        ));
    }

    let final_path = dir.join(name);
    let temporary = dir.join(format!(".{name}.{}.tmp", random_suffix()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &final_path)?;
        // Make the directory entry durable too. Without this, a successful
        // response can still point at a file lost after sudden power loss.
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map(|()| final_path)
}

/// Open `dir` without following the final component and prove it is a
/// user-owned real directory (not a symlink to one).
fn open_owned_dir(dir: &Path) -> io::Result<std::fs::File> {
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "portal data directory must be a user-owned real directory",
        ));
    }
    Ok(directory)
}

/// Remove `dir` and its contents, after proving it is a user-owned real
/// directory with the same O_DIRECTORY|O_NOFOLLOW discipline as
/// [`write_atomic`]. A missing directory is not an error.
pub(crate) fn remove_owned_dir(dir: &Path) -> io::Result<()> {
    match open_owned_dir(dir) {
        Ok(_) => std::fs::remove_dir_all(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

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

/// Render an absolute path as a `file://` URI, percent-encoding every byte
/// outside the RFC 3986 unreserved set plus `/`.
pub(crate) fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// Resolve a local `file://` URI to a filesystem path. A remote authority is
/// rejected: portal backends must not reinterpret a network URI as a local
/// path. Percent escapes are decoded byte-for-byte so non-UTF-8 Unix paths
/// round-trip correctly.
pub(crate) fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri
        .strip_prefix("file://localhost/")
        .map(|path| format!("/{path}"))
        .or_else(|| uri.strip_prefix("file://").map(str::to_string))?;
    if !rest.starts_with('/') {
        return None;
    }

    let mut bytes = Vec::with_capacity(rest.len());
    let mut chars = rest.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let hex = |value: u8| -> Option<u8> {
                match value {
                    b'0'..=b'9' => Some(value - b'0'),
                    b'a'..=b'f' => Some(value - b'a' + 10),
                    b'A'..=b'F' => Some(value - b'A' + 10),
                    _ => None,
                }
            };
            bytes.push(hex(hi)? * 16 + hex(lo)?);
        } else {
            bytes.push(byte);
        }
    }
    if bytes.contains(&0) {
        return None;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_prefers_cache_home_and_falls_back_to_runtime_dir() {
        let cache = std::ffi::OsString::from("/cache");
        let runtime = std::ffi::OsString::from("/run/user/1000");
        assert_eq!(
            cache_dir_from(Some(cache.clone()), Some(runtime.clone())),
            Some(PathBuf::from("/cache/xdg-desktop-portal-atrium"))
        );
        assert_eq!(
            cache_dir_from(None, Some(runtime.clone())),
            Some(PathBuf::from("/run/user/1000/xdg-desktop-portal-atrium"))
        );
        assert_eq!(
            cache_dir_from(Some("".into()), Some(runtime)),
            Some(PathBuf::from("/run/user/1000/xdg-desktop-portal-atrium"))
        );
        assert_eq!(cache_dir_from(None, None), None);
    }

    #[test]
    fn write_capture_persists_bytes_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!(
            "xdg-desktop-portal-atrium-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = write_capture(&dir, "tok1", b"png-bytes").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"png-bytes");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("screenshot-"));
        assert!(name.contains("-tok1-"));
        assert!(name.ends_with(".png"));
        // No temp file lingers next to the result.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // Mode 0600: portal payloads are screen pixels.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_capture_rejects_symlink_directory_without_chmodding_target() {
        let root = std::env::temp_dir().join(format!(
            "xdg-desktop-portal-atrium-symlink-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = root.join("target");
        let link = root.join("cache");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(write_capture(&link, "tok1", b"private pixels").is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remove_owned_dir_wipes_contents_and_refuses_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "xdg-desktop-portal-atrium-remove-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let staging = root.join("wallpaper");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("current.png"), b"stale").unwrap();
        std::fs::write(staging.join(".current.png.ab.tmp"), b"partial").unwrap();

        remove_owned_dir(&staging).unwrap();
        assert!(!staging.exists());
        // A missing directory is not an error.
        remove_owned_dir(&staging).unwrap();

        // A symlink is refused and its target keeps its contents.
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.png"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &staging).unwrap();
        assert!(remove_owned_dir(&staging).is_err());
        assert_eq!(std::fs::read(target.join("keep.png")).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_uri_encodes_only_what_it_must() {
        assert_eq!(file_uri(Path::new("/a/b-c_d.png")), "file:///a/b-c_d.png");
        assert_eq!(
            file_uri(Path::new("/a b/ç.png")),
            "file:///a%20b/%C3%A7.png"
        );
    }

    #[test]
    fn file_uri_path_decodes_local_paths_only() {
        assert_eq!(
            path_from_file_uri("file:///home/user/My%20Files/a%20b.bin"),
            Some(PathBuf::from("/home/user/My Files/a b.bin"))
        );
        assert_eq!(
            path_from_file_uri("file://localhost/tmp/x.bin"),
            Some(PathBuf::from("/tmp/x.bin"))
        );
        assert_eq!(path_from_file_uri("file://server/share/x.bin"), None);
        assert_eq!(path_from_file_uri("https://example.com/x.bin"), None);
        assert_eq!(path_from_file_uri("file:///bad/%zz.bin"), None);
    }
}
