//! `file://` URI rendering and parsing for portal results.
//!
//! The portal's result dictionaries carry `file://` URIs, and its argument
//! dictionaries accept them. Both directions work on native Unix bytes so a
//! filename that is not valid UTF-8 survives the round trip.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
            bytes.push(hex(hi)? << 4 | hex(lo)?);
        } else {
            bytes.push(byte);
        }
    }
    use std::os::unix::ffi::OsStringExt as _;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_percent_encode_and_decode_native_bytes() {
        let path = PathBuf::from("/tmp/a b/ünïcode.txt");
        let uri = file_uri(&path);
        assert!(uri.starts_with("file:///tmp/a%20b/"));
        assert_eq!(path_from_file_uri(&uri), Some(path));
    }

    #[test]
    fn a_non_utf8_path_round_trips() {
        use std::os::unix::ffi::OsStringExt as _;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', 0xff, b'.', b'x',
        ]));
        let uri = file_uri(&path);
        assert!(uri.contains("%FF"));
        assert_eq!(path_from_file_uri(&uri), Some(path));
    }

    #[test]
    fn remote_authorities_and_relative_paths_are_refused() {
        assert!(path_from_file_uri("file://remote.host/tmp/x").is_none());
        assert!(path_from_file_uri("file://relative").is_none());
        assert!(path_from_file_uri("https://example.test/x").is_none());
        assert!(path_from_file_uri("file:///tmp/bad%2").is_none());
    }

    #[test]
    fn localhost_is_the_local_host() {
        assert_eq!(
            path_from_file_uri("file://localhost/etc/hostname"),
            Some(PathBuf::from("/etc/hostname"))
        );
    }
}
