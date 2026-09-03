//! ScreenCast persist/restore tokens.
//!
//! The ScreenCast contract lets a client skip the source selection UI on
//! later sessions by presenting a `restore_token` the backend issued
//! earlier. Two persistence modes exist:
//!
//! - **Mode 1 (until revoked)** tokens live in
//!   `$XDG_DATA_HOME/atrium-portal/screencast-restore.json` (directory 0700,
//!   file 0600, atomic temp+rename through [`crate::files::write_atomic`]).
//! - **Mode 2 (until the app exits)** tokens live in memory only, keyed by
//!   the caller's D-Bus unique name; the daemon's NameOwnerChanged watcher
//!   drops them when that name vanishes.
//!
//! Only monitor selections (whole desktop or one connector) are
//! persistable: a window id is not stable across sessions, so window
//! captures never get a token. Tokens are 128-bit random hex, opaque to
//! clients. Validation is fail-closed: an unknown token, an app_id
//! mismatch, a corrupt store file, or a selection the compositor can no
//! longer serve (a vanished connector, or any connector target against a
//! pre-29 compositor) degrades to the normal interactive selection.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rand::RngCore as _;

/// The on-disk store file inside the data directory.
const STORE_FILE: &str = "screencast-restore.json";
/// Refuse to read a store larger than this; it is corrupt or hostile.
const MAX_STORE_BYTES: u64 = 1024 * 1024;
/// Bounds on retained tokens; the oldest are evicted first.
const MAX_DISK_TOKENS: usize = 64;
const MAX_MEMORY_TOKENS_PER_OWNER: usize = 16;
/// The store file schema this daemon reads and writes.
const STORE_VERSION: u32 = 1;

/// A persistable capture selection. Window captures are deliberately
/// absent: they can never be restored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StoredSource {
    /// The whole desktop.
    Desktop,
    /// One connector-named output.
    Output { connector: String },
}

/// The selection a token restores, bound to the application it was issued
/// for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredSelection {
    pub(crate) app_id: String,
    pub(crate) source: StoredSource,
    pub(crate) cursor_mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    token: String,
    #[serde(flatten)]
    selection: StoredSelection,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskStore {
    version: u32,
    tokens: Vec<StoredEntry>,
}

/// The restore-token store: mode-1 tokens on disk, mode-2 tokens in
/// memory keyed by the caller's D-Bus unique name.
pub(crate) struct RestoreStore {
    data_dir: Option<PathBuf>,
    disk: Vec<StoredEntry>,
    memory: HashMap<String, Vec<StoredEntry>>,
}

impl RestoreStore {
    /// Load the store rooted at `data_dir` (`$XDG_DATA_HOME/atrium-portal`,
    /// `None` when no data directory exists). A missing file is an empty
    /// store; a corrupt or unreadable one fails closed to empty and is
    /// replaced by the next write.
    pub(crate) fn load(data_dir: Option<PathBuf>) -> Self {
        let disk = data_dir
            .as_ref()
            .and_then(|dir| read_store(&dir.join(STORE_FILE)))
            .unwrap_or_default();
        Self {
            data_dir,
            disk,
            memory: HashMap::new(),
        }
    }

    /// Issue a fresh token for `selection`. Mode 1 appends to the disk
    /// store; mode 2 records the token under `owner` (the caller's D-Bus
    /// unique name). Returns `None` when the grant cannot be honored (a
    /// mode-1 request without a writable data directory, or a mode-2
    /// request without a sender name) — the caller then reports the
    /// reduction to `persist_mode` 0.
    pub(crate) fn issue(
        &mut self,
        mode: u32,
        owner: &str,
        selection: StoredSelection,
    ) -> Option<String> {
        let token = random_token();
        let entry = StoredEntry {
            token: token.clone(),
            selection,
        };
        match mode {
            1 => {
                self.data_dir.as_ref()?;
                self.disk.push(entry);
                if self.disk.len() > MAX_DISK_TOKENS {
                    let overflow = self.disk.len() - MAX_DISK_TOKENS;
                    self.disk.drain(..overflow);
                }
                if let Err(error) = self.write_disk() {
                    log::warn!("portal: could not persist the screencast restore store: {error}");
                }
            }
            2 => {
                if owner.is_empty() {
                    return None;
                }
                let entries = self.memory.entry(owner.to_string()).or_default();
                entries.push(entry);
                if entries.len() > MAX_MEMORY_TOKENS_PER_OWNER {
                    let overflow = entries.len() - MAX_MEMORY_TOKENS_PER_OWNER;
                    entries.drain(..overflow);
                }
            }
            _ => return None,
        }
        Some(token)
    }

    /// Resolve a token presented by `app_id`. `servable` reports whether
    /// the compositor can still capture the stored source (a whole-desktop
    /// selection is always servable; a connector selection needs protocol
    /// 29 and the connector still present). Any failure — unknown token,
    /// wrong application, unservable selection — returns `None`, which the
    /// caller treats as "no usable restore data" per the contract.
    pub(crate) fn validate(
        &mut self,
        token: &str,
        app_id: &str,
        servable: &dyn Fn(&StoredSource) -> bool,
    ) -> Option<(u32, StoredSelection)> {
        for entry in self.memory.values().flatten() {
            if entry.token == token {
                return (entry.selection.app_id == app_id && servable(&entry.selection.source))
                    .then(|| (2, entry.selection.clone()));
            }
        }
        if let Some(entry) = self.disk.iter().find(|entry| entry.token == token) {
            return (entry.selection.app_id == app_id && servable(&entry.selection.source))
                .then(|| (1, entry.selection.clone()));
        }
        None
    }

    /// Drop every mode-2 token of a vanished D-Bus unique name.
    pub(crate) fn drop_owner(&mut self, owner: &str) {
        self.memory.remove(owner);
    }

    /// Lazily prune disk tokens whose selections can no longer be served,
    /// writing the store back only when something was removed.
    pub(crate) fn prune(&mut self, servable: &dyn Fn(&StoredSource) -> bool) {
        let before = self.disk.len();
        self.disk.retain(|entry| servable(&entry.selection.source));
        if self.disk.len() != before
            && let Err(error) = self.write_disk()
        {
            log::warn!("portal: could not rewrite the pruned restore store: {error}");
        }
    }

    fn write_disk(&self) -> io::Result<PathBuf> {
        let Some(dir) = self.data_dir.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no XDG data directory for the restore store",
            ));
        };
        let store = DiskStore {
            version: STORE_VERSION,
            tokens: self.disk.clone(),
        };
        let bytes = serde_json::to_vec(&store)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        crate::files::write_atomic(dir, STORE_FILE, &bytes)
    }
}

/// The restore store's data directory: `$XDG_DATA_HOME/atrium-portal`, with
/// the freedesktop fallback `~/.local/share/atrium-portal`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("atrium-portal"))
}

/// Read and parse the store file; any failure is an empty store.
fn read_store(path: &Path) -> Option<Vec<StoredEntry>> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_STORE_BYTES {
        log::warn!("portal: refusing an oversized or non-regular restore store");
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let store: DiskStore = serde_json::from_slice(&bytes)
        .map_err(|error| {
            log::warn!("portal: ignoring a corrupt screencast restore store: {error}");
            error
        })
        .ok()?;
    if store.version != STORE_VERSION {
        log::warn!(
            "portal: ignoring a restore store with unsupported version {}",
            store.version
        );
        return None;
    }
    Some(store.tokens)
}

/// A fresh 128-bit token as lowercase hex. Tokens are opaque to clients.
fn random_token() -> String {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "atrium-portal-restore-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn selection(app_id: &str) -> StoredSelection {
        StoredSelection {
            app_id: app_id.to_string(),
            source: StoredSource::Desktop,
            cursor_mode: 1,
        }
    }

    fn always(_: &StoredSource) -> bool {
        true
    }

    #[test]
    fn issued_tokens_validate_and_survive_a_reload() {
        let dir = fixture_dir("roundtrip");
        let mut store = RestoreStore::load(Some(dir.clone()));
        let token = store
            .issue(1, ":1.42", selection("org.example.App"))
            .expect("mode-1 token");
        assert_eq!(token.len(), 32);
        let (mode, restored) = store
            .validate(&token, "org.example.App", &always)
            .expect("token validates");
        assert_eq!(mode, 1);
        assert_eq!(restored, selection("org.example.App"));

        // The file is private and reloadable.
        use std::os::unix::fs::PermissionsExt;
        let file = dir.join(STORE_FILE);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut reloaded = RestoreStore::load(Some(dir.clone()));
        assert_eq!(
            reloaded.validate(&token, "org.example.App", &always),
            Some((1, selection("org.example.App")))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn validation_rejects_the_wrong_application_and_unknown_tokens() {
        let dir = fixture_dir("wrong-app");
        let mut store = RestoreStore::load(Some(dir.clone()));
        let token = store
            .issue(1, ":1.42", selection("org.example.App"))
            .unwrap();
        assert!(
            store
                .validate(&token, "org.example.Other", &always)
                .is_none()
        );
        assert!(
            store
                .validate(
                    "00000000000000000000000000000000",
                    "org.example.App",
                    &always
                )
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mode_2_tokens_vanish_with_the_owner() {
        let mut store = RestoreStore::load(None);
        let token = store
            .issue(2, ":1.42", selection("org.example.App"))
            .expect("mode-2 token");
        assert_eq!(
            store.validate(&token, "org.example.App", &always),
            Some((2, selection("org.example.App")))
        );
        store.drop_owner(":1.42");
        assert!(store.validate(&token, "org.example.App", &always).is_none());
        // Another connection of the same app never sees the token.
        assert!(store.issue(2, "", selection("org.example.App")).is_none());
    }

    #[test]
    fn a_corrupt_store_fails_closed_and_is_replaced() {
        let dir = fixture_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STORE_FILE), b"not json {").unwrap();
        let mut store = RestoreStore::load(Some(dir.clone()));
        assert!(
            store
                .validate(
                    "00000000000000000000000000000000",
                    "org.example.App",
                    &always
                )
                .is_none()
        );
        // Issuing rewrites a valid store in place of the corrupt one.
        let token = store
            .issue(1, ":1.42", selection("org.example.App"))
            .unwrap();
        let mut reloaded = RestoreStore::load(Some(dir.clone()));
        assert!(
            reloaded
                .validate(&token, "org.example.App", &always)
                .is_some()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unservable_selections_do_not_restore_and_are_pruned() {
        let dir = fixture_dir("prune");
        let mut store = RestoreStore::load(Some(dir.clone()));
        let connector = StoredSelection {
            app_id: "org.example.App".into(),
            source: StoredSource::Output {
                connector: "HDMI-A-1".into(),
            },
            cursor_mode: 1,
        };
        let token = store.issue(1, ":1.42", connector).unwrap();
        let nothing_servable = |_: &StoredSource| false;
        assert!(
            store
                .validate(&token, "org.example.App", &nothing_servable)
                .is_none()
        );
        store.prune(&nothing_servable);
        assert!(
            store.validate(&token, "org.example.App", &always).is_none(),
            "a pruned token stays invalid even when it becomes servable again"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
