//! Native `org.freedesktop.impl.portal.Secret` storage for Aegis.
//!
//! The public portal secret is derived from a private, encrypted vault key.
//! Password-mode vaults persist their Argon2id parameters in `vault.kdf`,
//! unlock from a one-shot PAM token or a Portal-owned masked prompt, and
//! support create/re-key through [`SecretService::create_password_vault`],
//! [`SecretService::change_password`], and the prompter-free
//! [`rekey_password_vault_in`] entry used by the PAM chauthtok hook. The PAM
//! token carries the derived vault master key ([`PAM_TOKEN_KEY_PREFIX`] +
//! hex) when a password-mode vault exists at login, falling back to the raw
//! login password otherwise; re-keying is a two-phase protocol whose
//! interrupted states self-heal on the next successful unlock. This
//! crate deliberately does not
//! implement `org.freedesktop.secrets`; claiming that separate API without
//! its full locking, alias, prompt, and collection semantics would be unsafe.

mod portal;
mod vault;

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aegis_portal_runtime::sync;
use argon2::Params;
use zeroize::{Zeroize, Zeroizing};

use vault::{Vault, VaultData};

const PAM_TOKEN_NAME: &str = "aegis-pam-token";
const MAX_PAM_TOKEN_BYTES: u64 = 64 * 1024;
const MAX_KEYFILE_BYTES: u64 = 1024;
const MAX_KDF_BYTES: u64 = 4096;
const MAX_PENDING_UNLOCKS: usize = 64;

/// Prefix of the v2 PAM token: the derived vault master key as 64 lowercase
/// hex chars, planted by the PAM module instead of the raw login password
/// whenever a password-mode vault exists (see [`derive_token_key_in`]).
/// Anything else in the token file parses as a legacy raw password token.
pub const PAM_TOKEN_KEY_PREFIX: &str = "aegis-key-v1:";

/// Mark the pages covering a secret region `MADV_DONTDUMP` so its bytes
/// stay out of any core dump image — including a piped core handler, which
/// the process-wide `RLIMIT_CORE` cap alone cannot guarantee. Best effort
/// like the mlock policy: a failure is logged, never fatal. The flag dies
/// with the mapping, so it needs no undo.
fn mark_dontdump(what: &str, ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    // madvise requires a page-aligned address, so round the range out to
    // its covering pages; over-excluding a neighbor's bytes on a shared
    // page is harmless. SAFETY: the rounded range covers the region the
    // caller guarantees is valid readable memory owned by this process.
    let page = page_size();
    let start = ptr as usize & !(page - 1);
    let end = (ptr as usize + len + page - 1) & !(page - 1);
    if unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTDUMP) } != 0 {
        log::warn!(
            "portal: could not mark {what} MADV_DONTDUMP: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// The host page size, defaulting to 4 KiB if sysconf refuses.
fn page_size() -> usize {
    // SAFETY: sysconf is always safe to call.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 { size as usize } else { 4096 }
}

/// A best-effort mlock'd byte buffer for hot secret material. Construction
/// pins the buffer's pages against swapping and marks them MADV_DONTDUMP so
/// they stay out of any core dump image. Drop zeroizes the contents before
/// releasing them. A pinning or advice failure (RLIMIT_MEMLOCK and friends)
/// is logged and never fails the surrounding operation — the buffer stays
/// fully usable. The Vec is never grown after construction, so the pinned
/// address stays valid for the buffer's lifetime, the same reasoning as the
/// boxed master key in `Vault`.
struct LockedBytes {
    bytes: Vec<u8>,
    mlocked: bool,
}

impl LockedBytes {
    fn new(bytes: Vec<u8>) -> Self {
        let mut locked = Self {
            bytes,
            mlocked: false,
        };
        if locked.bytes.is_empty() {
            return locked;
        }
        // SAFETY: the Vec's heap region is valid, readable, and owned by
        // this process, and its address cannot change because nothing grows
        // or mutates the Vec after this point. mlock only pins the pages
        // against swapping; failure is reported below without failing the
        // operation.
        let result = unsafe {
            libc::mlock(
                locked.bytes.as_ptr().cast::<libc::c_void>(),
                locked.bytes.len(),
            )
        };
        if result == 0 {
            locked.mlocked = true;
        } else {
            log::warn!(
                "portal: could not mlock a secret buffer: {}",
                std::io::Error::last_os_error()
            );
        }
        // Independent of the mlock outcome: exclude the pages from any
        // core dump image.
        mark_dontdump("a secret buffer", locked.bytes.as_ptr(), locked.bytes.len());
        locked
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }
}

impl Drop for LockedBytes {
    fn drop(&mut self) {
        self.bytes.zeroize();
        if self.mlocked {
            // SAFETY: the region was successfully mlock'd in `new`, its
            // address has not changed since (the Vec is never grown), and it
            // is zeroized above before the pages are released.
            unsafe {
                libc::munlock(self.bytes.as_ptr().cast::<libc::c_void>(), self.bytes.len());
            }
        }
    }
}

/// Result of asking the Portal's UI adapter for the vault password.
pub enum PromptResponse {
    /// The user confirmed a password. This crate zeroizes it immediately
    /// after it crosses the boundary.
    Secret(String),
    /// The user dismissed the prompt without submitting a password.
    Cancelled,
}

/// Narrow host capability required to unlock a password-protected vault.
/// The host must stop its prompt when every waiting request is cancelled.
pub trait SecretPrompter: Send + Sync + 'static {
    fn prompt_secret(
        &self,
        title: &str,
        reason: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PromptResponse, String>;
}

/// Native Secret portal service and its shared unlock coordinator.
pub struct SecretService {
    state: Arc<Mutex<SecretState>>,
    prompter: Arc<dyn SecretPrompter>,
}

impl SecretService {
    /// Open or create the user's vault and bind its unlock path to `prompter`.
    pub fn initialize(prompter: Arc<dyn SecretPrompter>) -> Result<Self, SecretError> {
        let state = init()?;
        Ok(Self { state, prompter })
    }

    /// Create a fresh password-mode vault in the user's XDG data directory
    /// and return its unlocked service. Writes `vault.kdf` (authoritative
    /// Argon2id parameters), `vault.salt` (downgrade mirror), and an empty
    /// `vault.enc`; refuses to overwrite any existing vault.
    pub fn create_password_vault(
        prompter: Arc<dyn SecretPrompter>,
        password: &str,
    ) -> Result<Self, SecretError> {
        let dir = dirs::data_dir()
            .ok_or(SecretError::NoDataDir)?
            .join("aegis")
            .join("secrets");
        let state = create_password_vault_in(&dir, password)?;
        Ok(Self { state, prompter })
    }

    /// Change a password-mode vault's password. The current password is
    /// proven by authenticated decryption before any file is modified; on
    /// success the vault is re-keyed under a fresh salt with crate-default
    /// Argon2id parameters persisted through a two-phase protocol
    /// (`vault.kdf.next`/`vault.salt.next` staged, `vault.enc` swapped, the
    /// pair renamed into `vault.kdf`/`vault.salt`) whose interrupted states
    /// self-heal on the next unlock, and stays unlocked under the new
    /// password.
    pub fn change_password(&self, current: &str, new: &str) -> Result<(), SecretError> {
        sync::lock(&self.state, "secret state").change_password(current, new)
    }

    /// Register the native Secret backend interface.
    pub fn register_portal(
        &self,
        conn: &zbus::blocking::Connection,
        tracker: Arc<Mutex<aegis_portal_runtime::RequestTracker>>,
        path: &str,
    ) -> zbus::Result<()> {
        conn.object_server().at(
            path,
            portal::SecretIface {
                conn: conn.clone(),
                tracker,
                state: Arc::clone(&self.state),
                prompter: Arc::clone(&self.prompter),
            },
        )?;
        Ok(())
    }

    /// Check if the vault is currently unlocked in memory.
    pub fn is_unlocked(&self) -> bool {
        sync::lock(&self.state, "secret state").is_unlocked()
    }

    /// Explicitly lock the vault and purge the master key from memory.
    /// Dropping the `Vault` zeroizes its master key and munlocks its pages.
    pub fn lock(&self) {
        let mut state = sync::lock(&self.state, "secret state");
        if state.vault.take().is_some() {
            log::info!("portal: secret vault locked; master key zeroized in memory");
        }
    }

    /// Observe a session lock boundary (logind `Lock` signal or `PrepareForSleep(true)`).
    ///
    /// Following modern desktop security architecture (GNOME/KDE best practices),
    /// the vault master key is bound to the user's login session and remains
    /// secured in memory (mlock'd and MADV_DONTDUMP) across screen locks and suspend
    /// so that background sync, notification daemons, and running applications
    /// (e.g. Chrome, email clients) continue to function without credential loss.
    pub fn lock_for_session(&self) {
        log::info!("portal: session lock observed; master key remains secured in memory");
    }

    /// The session returned (logind `Unlock` signal or `PrepareForSleep(false)`).
    /// If the vault was not yet unlocked (e.g. keyfile vault that started locked),
    /// re-unlocks from keyfile.
    pub fn unlock_for_session(&self) {
        let mut state = sync::lock(&self.state, "secret state");
        if state.is_unlocked() {
            log::info!("portal: session unlock observed; secret vault is active");
            return;
        }
        if state.is_keyfile_mode() {
            match state.unlock_with_keyfile() {
                Ok(()) => {
                    log::info!("portal: secret vault re-unlocked from keyfile with the session")
                }
                Err(error) => {
                    log::warn!("portal: keyfile re-unlock after session unlock failed: {error}")
                }
            }
        }
    }

    /// Whether the vault is in keyfile mode (unlockable without a
    /// credential). Exposed for the session-lock watcher to log the
    /// effective policy once at startup.
    pub fn is_keyfile_mode(&self) -> bool {
        sync::lock(&self.state, "secret state").is_keyfile_mode()
    }

    /// Watch for a PAM token planted by a committing PAM hook (login, or
    /// a screen locker that establishes credentials — see ADR-0010).
    /// The watcher runs for the daemon's lifetime using Linux inotify for
    /// instant, event-driven token consumption and unlocking without polling latency.
    pub fn start_pam_watcher(&self) {
        let state = Arc::clone(&self.state);
        let spawned = std::thread::Builder::new()
            .name("aegis-pam-token-watcher".to_owned())
            .spawn(move || {
                let uid = unsafe { libc::getuid() };
                let run_dir = PathBuf::from(format!("/run/user/{uid}"));

                let try_consume = || {
                    if let Some(token) = consume_pam_token() {
                        let mut guard = sync::lock(&state, "secret state");
                        match guard.unlock_with_pam_token(&token) {
                            Ok(()) => {
                                log::info!("portal: secret vault unlocked by a PAM token");
                            }
                            Err(error) => {
                                log::warn!("portal: PAM-token unlock failed: {error}");
                                return;
                            }
                        }
                        // Wake anything queued behind the unlock prompt.
                        let requests = std::mem::take(&mut guard.pending_unlocks);
                        drop(guard);
                        if !requests.is_empty() {
                            complete_unlock_requests(&state, requests, true);
                        }
                    }
                };

                // Initial check in case the token was already planted before the watcher thread started
                try_consume();

                let inotify_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
                if inotify_fd < 0 {
                    log::warn!("portal: inotify_init1 failed; falling back to periodic polling");
                    loop {
                        try_consume();
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }

                let c_path = match std::ffi::CString::new(run_dir.as_os_str().as_encoded_bytes()) {
                    Ok(c) => c,
                    Err(_) => {
                        unsafe { libc::close(inotify_fd) };
                        loop {
                            try_consume();
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                };

                let watch_mask =
                    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB;
                let mut wd =
                    unsafe { libc::inotify_add_watch(inotify_fd, c_path.as_ptr(), watch_mask) };

                let mut buffer = [0u8; 4096];
                loop {
                    if wd < 0 {
                        wd = unsafe {
                            libc::inotify_add_watch(inotify_fd, c_path.as_ptr(), watch_mask)
                        };
                        if wd < 0 {
                            try_consume();
                            std::thread::sleep(Duration::from_millis(200));
                            continue;
                        }
                    }

                    let mut pfd = libc::pollfd {
                        fd: inotify_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ret = unsafe { libc::poll(&mut pfd, 1, 2000) };
                    if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
                        let bytes_read = unsafe {
                            libc::read(
                                inotify_fd,
                                buffer.as_mut_ptr().cast::<libc::c_void>(),
                                buffer.len(),
                            )
                        };
                        if bytes_read > 0 {
                            try_consume();
                        }
                    } else {
                        // Timeout tick or signal interruption: check token
                        try_consume();
                    }
                }
            });
        if let Err(error) = spawned {
            log::error!("portal: could not start PAM-token watcher: {error}");
        }
    }
}

/// Errors that keep secret support from coming up at all.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// No XDG data directory is available.
    #[error("no XDG data directory available")]
    NoDataDir,
    /// Filesystem failure around the vault directory or files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Key derivation, encryption, or decryption failed.
    #[error("crypto error: {0}")]
    Crypto(String),
    /// The vault file is truncated or otherwise malformed.
    #[error("vault error: {0}")]
    Vault(String),
    /// (De)serialization of the vault contents failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

struct SecretState {
    pub(crate) vault: Option<Vault>,
    pub(crate) pending_unlocks: Vec<PendingUnlock>,
    pub(crate) unlock_worker_active: bool,
    pub(crate) vault_path: PathBuf,
    pub(crate) salt_path: PathBuf,
    pub(crate) kdf_path: PathBuf,
    /// Present when the vault directory holds a `vault.key` (keyfile
    /// mode). A keyfile vault is always unlockable without a prompt, so a
    /// lock is reversible: the master key is re-read from disk on the
    /// next unlock path instead of demanding a password the vault never
    /// had. `None` marks password mode, whose only re-unlock paths are
    /// the PAM token and the masked prompt.
    pub(crate) keyfile_path: Option<PathBuf>,
}

impl SecretState {
    pub(crate) fn is_unlocked(&self) -> bool {
        self.vault.is_some()
    }

    /// Is this a keyfile-mode vault, unlockable without a credential?
    pub(crate) fn is_keyfile_mode(&self) -> bool {
        self.keyfile_path.is_some()
    }

    /// Re-unlock a keyfile-mode vault by re-reading `vault.key` from disk
    /// and proving it through authenticated decryption. The read carries
    /// the same `O_NOFOLLOW`, ownership, mode-0600, and size validation as
    /// startup ([`read_regular_file`]), so a swapped or tampered keyfile
    /// fails closed exactly like a corrupted startup state. This is the
    /// only post-startup keyfile path: the vault file itself is never
    /// created here (startup owns first-run initialization).
    pub(crate) fn unlock_with_keyfile(&mut self) -> Result<(), SecretError> {
        let Some(key_path) = self.keyfile_path.clone() else {
            return Err(SecretError::Vault(
                "vault is not in keyfile mode".to_owned(),
            ));
        };
        if !self.vault_path.exists() {
            return Err(SecretError::Vault(
                "keyfile-mode vault is missing vault.enc".to_owned(),
            ));
        }
        let hex = read_regular_file(&key_path, MAX_KEYFILE_BYTES, true)?;
        let hex = std::str::from_utf8(&hex)
            .map_err(|_| SecretError::Crypto("vault.key is not UTF-8".to_owned()))?;
        let mut key = Vault::key_from_hex(hex)?;
        let vault = Vault::new(self.vault_path.clone(), key);
        key.zeroize();
        // Authenticated decryption is the validity check: a stale or
        // tampered keyfile fails closed without touching state.
        let _validated = vault.load()?;
        self.vault = Some(vault);
        Ok(())
    }

    /// The KDF candidates present on disk, in unlock-attempt order.
    /// `vault.kdf` is authoritative; `vault.kdf.next` is the pending pair
    /// of an interrupted re-key; a bare `vault.salt` or `vault.salt.next`
    /// implies the legacy crate-default Argon2id parameters.
    fn kdf_candidates(&self) -> Vec<KdfSource> {
        let mut sources = Vec::new();
        if self.kdf_path.exists() {
            sources.push(KdfSource::Kdf);
        }
        if next_path(&self.kdf_path).exists() {
            sources.push(KdfSource::KdfNext);
        }
        if self.salt_path.exists() {
            sources.push(KdfSource::SaltLegacy);
        }
        if next_path(&self.salt_path).exists() {
            sources.push(KdfSource::SaltNextLegacy);
        }
        sources
    }

    /// Load one candidate's parameters. The base files (`vault.kdf`,
    /// `vault.salt`) fail closed on any read or parse error, exactly as
    /// before; a pending `.next` file is skipped instead, because an
    /// interrupted re-key must not wedge an otherwise unlockable vault.
    fn load_kdf_candidate(&self, source: KdfSource) -> Result<Option<VaultKdf>, SecretError> {
        let path = match source {
            KdfSource::Kdf => self.kdf_path.clone(),
            KdfSource::KdfNext => next_path(&self.kdf_path),
            KdfSource::SaltLegacy => self.salt_path.clone(),
            KdfSource::SaltNextLegacy => next_path(&self.salt_path),
        };
        let loaded = match source {
            KdfSource::Kdf | KdfSource::KdfNext => read_regular_file(&path, MAX_KDF_BYTES, false)
                .and_then(|bytes| vault::decode_kdf(&bytes)),
            KdfSource::SaltLegacy | KdfSource::SaltNextLegacy => {
                read_regular_file(&path, MAX_KEYFILE_BYTES, false).and_then(|bytes| {
                    let salt = std::str::from_utf8(&bytes)
                        .map_err(|_| SecretError::Crypto("vault.salt is not UTF-8".to_owned()))?;
                    let salt = salt.trim().to_owned();
                    argon2::password_hash::SaltString::from_b64(&salt)
                        .map_err(|error| SecretError::Crypto(format!("invalid salt: {error}")))?;
                    Ok((Params::default(), salt))
                })
            }
        };
        match loaded {
            Ok((params, salt)) => Ok(Some(VaultKdf { params, salt })),
            Err(error) if source.strict() => Err(error),
            Err(error) => {
                log::warn!("portal: skipping unusable {source:?} parameters: {error}");
                Ok(None)
            }
        }
    }

    /// Prove `password` by authenticated decryption, trying the KDF
    /// candidates in order until one decrypts the vault. On success returns
    /// the open vault, its contents, and the winning candidate — the caller
    /// keeps the vault live (unlock) or re-keys it (password change). A
    /// wrong password fails closed with every candidate exhausted and no
    /// file modified.
    fn open_with_password(
        &self,
        password: &str,
    ) -> Result<(Vault, VaultData, KdfSource, VaultKdf), SecretError> {
        if !self.vault_path.exists() {
            return Err(SecretError::Vault(
                "password-mode vault is missing vault.enc".to_owned(),
            ));
        }
        let mut failure = None;
        for source in self.kdf_candidates() {
            let Some(kdf) = self.load_kdf_candidate(source)? else {
                continue;
            };
            let mut key = Vault::derive_key_with(&kdf.params, password, &kdf.salt)?;
            let vault = Vault::new(self.vault_path.clone(), key);
            key.zeroize();
            match vault.load() {
                // Authenticated decryption validates the password before the
                // master key becomes live.
                Ok(data) => return Ok((vault, data, source, kdf)),
                Err(error) => failure = Some(error),
            }
        }
        Err(failure.unwrap_or_else(|| {
            SecretError::Vault("password-mode vault has no KDF material".to_owned())
        }))
    }

    fn unlock_with_password(&mut self, password: &str) -> Result<(), SecretError> {
        let (vault, _validated, winner, kdf) = self.open_with_password(password)?;
        self.vault = Some(vault);
        self.reconcile_kdf(winner, &kdf);
        Ok(())
    }

    /// Unlock by direct master-key decryption — the v2 PAM token path.
    /// Authenticated decryption is the validity check: a wrong or garbage
    /// key fails closed. No KDF reconciliation happens here: without the
    /// password there is no way to tell a stale pending pair from the live
    /// one, so `.next` files are left untouched for the next password
    /// unlock.
    fn unlock_with_master_key(&mut self, key: &[u8; 32]) -> Result<(), SecretError> {
        if !self.vault_path.exists() {
            return Err(SecretError::Vault(
                "password-mode vault is missing vault.enc".to_owned(),
            ));
        }
        let vault = Vault::new(self.vault_path.clone(), *key);
        let _validated = vault.load()?;
        self.vault = Some(vault);
        Ok(())
    }

    /// Unlock from a consumed PAM token, dispatching on its format: a v2
    /// derived-key token goes straight to master-key decryption, a legacy
    /// token takes the password path.
    fn unlock_with_pam_token(&mut self, token: &PamToken) -> Result<(), SecretError> {
        match token {
            PamToken::Password(password) => {
                let password = password.as_str().ok_or_else(|| {
                    SecretError::Crypto("PAM token password is not UTF-8".to_owned())
                })?;
                self.unlock_with_password(password)
            }
            PamToken::MasterKey(bytes) => {
                let key: &[u8; 32] = bytes.as_bytes().try_into().map_err(|_| {
                    SecretError::Crypto("v2 PAM token key must be 32 bytes".to_owned())
                })?;
                self.unlock_with_master_key(key)
            }
        }
    }

    /// Reconcile on-disk KDF state after a successful password unlock: a
    /// winning pending `.next` file is adopted into its final position
    /// (together with its mirror pair), a legacy salt winner backfills
    /// `vault.kdf`, and leftover pending files of an interrupted re-key are
    /// removed. A pending file whose adoption rename failed is kept — it
    /// may be the only record of the live key's parameters. Every step is
    /// best-effort: reconciliation must never fail a successful unlock.
    fn reconcile_kdf(&self, winner: KdfSource, kdf: &VaultKdf) {
        let kdf_next = next_path(&self.kdf_path);
        let salt_next = next_path(&self.salt_path);
        let mut protected: Vec<PathBuf> = Vec::new();
        match winner {
            KdfSource::Kdf => {}
            KdfSource::KdfNext => match std::fs::rename(&kdf_next, &self.kdf_path) {
                Ok(()) => {
                    // Adopt the mirror pair as well so vault.salt keeps
                    // describing the live derivation.
                    if salt_next.exists()
                        && let Err(error) = std::fs::rename(&salt_next, &self.salt_path)
                    {
                        log::warn!("portal: could not adopt vault.salt.next: {error}");
                        protected.push(salt_next.clone());
                    }
                    self.sync_vault_dir();
                }
                Err(error) => {
                    log::warn!(
                        "portal: unlocked via vault.kdf.next but could not adopt it: {error}"
                    );
                    // Keep the pending pair together for the next attempt.
                    protected.push(kdf_next.clone());
                    protected.push(salt_next.clone());
                }
            },
            KdfSource::SaltLegacy => {
                // Backfill vault.kdf with the parameters actually used,
                // keeping vault.salt as the downgrade mirror.
                if let Err(error) = migrate_kdf(&self.kdf_path, &kdf.params, &kdf.salt) {
                    log::warn!(
                        "portal: legacy vault unlocked but vault.kdf migration failed: {error}"
                    );
                }
            }
            KdfSource::SaltNextLegacy => match std::fs::rename(&salt_next, &self.salt_path) {
                Ok(()) => {
                    if let Err(error) = migrate_kdf(&self.kdf_path, &kdf.params, &kdf.salt) {
                        log::warn!(
                            "portal: vault.salt.next adopted but vault.kdf backfill failed: {error}"
                        );
                    }
                    self.sync_vault_dir();
                }
                Err(error) => {
                    log::warn!(
                        "portal: unlocked via vault.salt.next but could not adopt it: {error}"
                    );
                    protected.push(salt_next.clone());
                }
            },
        }
        // Leftover pending files of an interrupted re-key never win again.
        for stale in [&kdf_next, &salt_next] {
            if protected.iter().any(|kept| kept == stale) || !stale.exists() {
                continue;
            }
            if let Err(error) = std::fs::remove_file(stale) {
                log::warn!(
                    "portal: could not remove stale {}: {error}",
                    stale.display()
                );
            }
        }
    }

    /// Persist directory-entry changes from reconciliation, best-effort.
    fn sync_vault_dir(&self) {
        let Some(parent) = self.kdf_path.parent() else {
            return;
        };
        if let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            log::warn!("portal: could not fsync the vault directory: {error}");
        }
    }

    /// Re-key the password-mode vault: prove `current` by authenticated
    /// decryption, rotate the salt, and persist the vault under crate-default
    /// Argon2id parameters. A wrong password is a clean error before any
    /// file is touched. On success the vault is unlocked under the new
    /// password.
    ///
    /// The write protocol is two-phase and self-healing:
    /// 1. the new parameters are staged as `vault.kdf.next` +
    ///    `vault.salt.next` (atomic replaces),
    /// 2. `vault.enc` is atomically replaced — the point of no return,
    /// 3. the pending pair is renamed into `vault.kdf` / `vault.salt`
    ///    (atomic renames + directory fsync).
    ///
    /// A crash before step 2 leaves the old trio plus a stray pending pair,
    /// which the next successful unlock cleans up; a crash between steps 2
    /// and 3 leaves the new ciphertext beside the pending pair, which the
    /// next unlock adopts through the KDF candidate order. The invariant:
    /// every reachable on-disk state either decrypts under one of
    /// [vault.kdf, vault.kdf.next, vault.salt, vault.salt.next] or the
    /// password is simply wrong — an interrupted re-key never desyncs the
    /// vault.
    fn change_password(&mut self, current: &str, new: &str) -> Result<(), SecretError> {
        // Pin the passwords against swapping for the derivation stretch.
        // The callers' own copies stay their responsibility; these are the
        // working copies this function derives from.
        let current_locked = LockedBytes::new(current.as_bytes().to_vec());
        let new_locked = LockedBytes::new(new.as_bytes().to_vec());
        let current = current_locked
            .as_str()
            .ok_or_else(|| SecretError::Crypto("password is not UTF-8".to_owned()))?;
        let new = new_locked
            .as_str()
            .ok_or_else(|| SecretError::Crypto("password is not UTF-8".to_owned()))?;

        let (_old_vault, data, _winner, _kdf) = self.open_with_password(current)?;

        let new_salt = Vault::generate_salt();
        let new_params = Params::default();
        let mut new_key = Vault::derive_key_with(&new_params, new, &new_salt)?;
        let new_vault = Vault::new(self.vault_path.clone(), new_key);
        new_key.zeroize();
        let encoded = vault::encode_kdf(&new_params, &new_salt)?;

        let kdf_next = next_path(&self.kdf_path);
        let salt_next = next_path(&self.salt_path);
        // Phase 1: stage the pending parameter pair.
        vault::atomic_replace(&kdf_next, &encoded)?;
        vault::atomic_replace(&salt_next, new_salt.as_bytes())?;
        // Phase 2: point of no return — the ciphertext moves to the new key.
        new_vault.save(&data)?;
        // Phase 3: adopt the pending pair, then persist the renames.
        std::fs::rename(&kdf_next, &self.kdf_path)?;
        std::fs::rename(&salt_next, &self.salt_path)?;
        let parent = self
            .kdf_path
            .parent()
            .ok_or_else(|| SecretError::Vault("vault.kdf must name a file".to_owned()))?;
        std::fs::File::open(parent)?.sync_all()?;

        self.vault = Some(new_vault);
        Ok(())
    }
}

/// A KDF candidate source, in unlock-attempt order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KdfSource {
    /// `vault.kdf`: authoritative parameters.
    Kdf,
    /// `vault.kdf.next`: staged parameters of an interrupted re-key.
    KdfNext,
    /// `vault.salt`: legacy crate-default parameters.
    SaltLegacy,
    /// `vault.salt.next`: staged legacy parameters of an interrupted re-key.
    SaltNextLegacy,
}

impl KdfSource {
    /// The base files keep the fail-closed behavior on read/parse errors;
    /// pending `.next` files are speculative and only ever skipped.
    fn strict(self) -> bool {
        matches!(self, Self::Kdf | Self::SaltLegacy)
    }
}

/// Password-mode KDF configuration resolved from the vault directory.
struct VaultKdf {
    params: Params,
    salt: String,
}

/// The pending-sidecar path of the two-phase re-key: `<name>.next` beside
/// the final file.
fn next_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".next");
    PathBuf::from(name)
}

/// Write vault.kdf without ever replacing an existing file. A concurrent
/// daemon that already migrated is indistinguishable from success.
fn migrate_kdf(kdf_path: &Path, params: &Params, salt: &str) -> Result<(), SecretError> {
    let encoded = vault::encode_kdf(params, salt)?;
    match vault::atomic_create(kdf_path, &encoded) {
        Ok(()) => {
            log::info!("portal: recorded Argon2id parameters in vault.kdf");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(SecretError::Io(error)),
    }
}

/// Create a fresh password-mode vault in `dir` and return it unlocked.
/// vault.kdf (authoritative parameters) and vault.salt (downgrade mirror)
/// are created before the empty vault.enc; nothing existing is ever
/// replaced, and files this call created are removed again if a later step
/// fails so a partial vault cannot wedge startup.
fn create_password_vault_in(
    dir: &Path,
    password: &str,
) -> Result<Arc<Mutex<SecretState>>, SecretError> {
    prepare_private_dir(dir)?;

    let key_path = dir.join("vault.key");
    let vault_path = dir.join("vault.enc");
    let salt_path = dir.join("vault.salt");
    let kdf_path = dir.join("vault.kdf");
    if key_path.exists() || vault_path.exists() || salt_path.exists() || kdf_path.exists() {
        return Err(SecretError::Vault(
            "refusing to overwrite an existing vault".to_owned(),
        ));
    }

    let salt = Vault::generate_salt();
    let params = Params::default();
    let mut key = Vault::derive_key_with(&params, password, &salt)?;
    let vault = Vault::new(vault_path.clone(), key);
    key.zeroize();

    let mut created: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), SecretError> {
        let encoded = vault::encode_kdf(&params, &salt)?;
        vault::atomic_create(&kdf_path, &encoded)?;
        created.push(kdf_path.clone());
        vault::atomic_create(&salt_path, salt.as_bytes())?;
        created.push(salt_path.clone());
        vault.save_new(&VaultData {
            collections: vec![],
        })?;
        created.push(vault_path.clone());
        Ok(())
    })();
    if result.is_err() {
        // Every path in `created` was provably absent before this call
        // (atomic_create), so removing it cannot touch foreign data.
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    }
    result?;

    Ok(Arc::new(Mutex::new(SecretState {
        vault: Some(vault),
        pending_unlocks: Vec::new(),
        unlock_worker_active: false,
        vault_path,
        salt_path,
        kdf_path,
        // A password-mode vault is never keyfile-unlockable.
        keyfile_path: None,
    })))
}

/// Re-key a password-mode vault in `dir` from `current` to `new` without a
/// prompter, service state, or D-Bus — the entry point for the `pam_aegis`
/// chauthtok hook, which runs inside arbitrary PAM client processes.
///
/// The directory must already exist and pass the same ownership, type, and
/// mode validation as daemon startup; nothing is created for a missing
/// vault. `current` is proven by authenticated decryption under the KDF
/// resolved with the usual precedence (`vault.kdf` authoritative, bare
/// `vault.salt` = legacy default parameters) before any file is touched, so
/// a wrong password leaves the vault byte-identical. On success the vault
/// is re-keyed under a fresh salt with crate-default Argon2id parameters
/// persisted to `vault.kdf` (+ the `vault.salt` downgrade mirror) through
/// the same two-phase protocol as [`SecretService::change_password`], whose
/// interrupted states self-heal on the next unlock.
pub fn rekey_password_vault_in(dir: &Path, current: &str, new: &str) -> Result<(), SecretError> {
    validate_private_dir(dir)?;
    let mut state = SecretState {
        vault: None,
        pending_unlocks: Vec::new(),
        unlock_worker_active: false,
        vault_path: dir.join("vault.enc"),
        salt_path: dir.join("vault.salt"),
        kdf_path: dir.join("vault.kdf"),
        keyfile_path: None,
    };
    state.change_password(current, new)
}

/// Derive the v2 PAM token for a password-mode vault in `dir`: the vault
/// master key under the authoritative Argon2id parameters, formatted as
/// [`PAM_TOKEN_KEY_PREFIX`] + 64 lowercase hex chars. Returns `None` —
/// letting the caller fall back to planting the raw login password — when
/// `dir` holds no password-mode vault (same gating as the PAM crate's
/// re-key plan: a keyfile takes precedence, and `vault.kdf` or `vault.salt`
/// beside `vault.enc` marks password mode), when the password is not UTF-8
/// (the vault password domain is UTF-8), or when the KDF parameters cannot
/// be loaded. All intermediate material is zeroized.
///
/// The KDF files are read without an ownership check against the process
/// uid: this entry exists for the PAM module, which legitimately runs as
/// root inside the login stack and resolves `dir` from the target account's
/// passwd entry. Reads still refuse symlinks, non-regular files,
/// group/world-writable files, and oversize content.
pub fn derive_token_key_in(dir: &Path, password: &[u8]) -> Option<Zeroizing<String>> {
    if dir.join("vault.key").exists() || !dir.join("vault.enc").exists() {
        return None;
    }
    let kdf_path = dir.join("vault.kdf");
    let salt_path = dir.join("vault.salt");
    if !kdf_path.exists() && !salt_path.exists() {
        return None;
    }
    let (params, salt) = if kdf_path.exists() {
        let bytes = read_token_kdf_file(&kdf_path, MAX_KDF_BYTES).ok()?;
        vault::decode_kdf(&bytes).ok()?
    } else {
        let bytes = read_token_kdf_file(&salt_path, MAX_KEYFILE_BYTES).ok()?;
        let salt = std::str::from_utf8(&bytes).ok()?;
        (Params::default(), salt.trim().to_owned())
    };
    let password = std::str::from_utf8(password).ok()?;
    let mut key = Vault::derive_key_with(&params, password, &salt).ok()?;
    let hex = Zeroizing::new(Vault::key_to_hex(&key));
    key.zeroize();
    Some(Zeroizing::new(format!("{PAM_TOKEN_KEY_PREFIX}{}", *hex)))
}

/// Read a vault KDF file for token derivation without an ownership check:
/// the PAM module legitimately reads another account's files as root after
/// resolving the directory from the account's passwd entry. Symlinks,
/// non-regular files, group/world-writable modes, and oversize content are
/// still refused.
fn read_token_kdf_file(path: &Path, limit: u64) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o022 != 0 || metadata.len() > limit {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("unsafe file type, mode, or size: {}", path.display()),
        )));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The result reported to a native RetrieveSecret request waiting for an
/// unlock.
pub(crate) enum PortalUnlockOutcome {
    Delivered,
    Dismissed,
    Failed,
}

pub(crate) struct PendingUnlock {
    pub(crate) fd: OwnedFd,
    pub(crate) outcome: async_channel::Sender<PortalUnlockOutcome>,
    pub(crate) tracker: Arc<Mutex<aegis_portal_runtime::RequestTracker>>,
    pub(crate) request_path: String,
    pub(crate) app_id: String,
}

pub(crate) fn write_secret_fd(fd: OwnedFd, secret: &[u8]) -> std::io::Result<()> {
    std::fs::File::from(fd).write_all(secret)
}

/// Queue one caller behind the shared unlock prompt. One worker services the
/// whole batch, while every request retains its own cancellation check.
pub(crate) fn enqueue_unlock_request(
    state: &Arc<Mutex<SecretState>>,
    prompter: &Arc<dyn SecretPrompter>,
    request: PendingUnlock,
) {
    let (spawn, rejected) = {
        let mut state = sync::lock(state, "secret state");
        if state.pending_unlocks.len() >= MAX_PENDING_UNLOCKS {
            (false, Some(request))
        } else {
            state.pending_unlocks.push(request);
            if state.unlock_worker_active {
                (false, None)
            } else {
                state.unlock_worker_active = true;
                (true, None)
            }
        }
    };
    if let Some(request) = rejected {
        log::warn!("portal: refusing RetrieveSecret request: unlock queue limit reached");
        let _ = request.outcome.send_blocking(PortalUnlockOutcome::Failed);
        return;
    }
    if !spawn {
        return;
    }

    let worker_state = Arc::clone(state);
    let worker_prompter = Arc::clone(prompter);
    let spawned = std::thread::Builder::new()
        .name("aegis-portal-unlock".to_owned())
        .spawn(move || unlock_worker(worker_state, worker_prompter));
    if let Err(error) = spawned {
        log::error!("portal: could not spawn unlock worker: {error}");
        let requests = {
            let mut state = sync::lock(state, "secret state");
            state.unlock_worker_active = false;
            std::mem::take(&mut state.pending_unlocks)
        };
        complete_unlock_requests(state, requests, false);
    }
}

fn unlock_worker(state: Arc<Mutex<SecretState>>, prompter: Arc<dyn SecretPrompter>) {
    loop {
        let requests = {
            let mut state = sync::lock(&state, "secret state");
            let requests = std::mem::take(&mut state.pending_unlocks);
            if requests.is_empty() {
                state.unlock_worker_active = false;
            }
            requests
        };
        if requests.is_empty() {
            return;
        }

        let unlocked = if sync::lock(&state, "secret state").is_unlocked() {
            true
        } else {
            // A keyfile-mode vault has no password: its prompt can never
            // succeed. Re-read the keyfile instead of demanding a
            // credential the vault never had.
            let keyfile_unlock = {
                let mut state = sync::lock(&state, "secret state");
                if state.is_keyfile_mode() {
                    match state.unlock_with_keyfile() {
                        Ok(()) => {
                            log::info!(
                                "portal: secret vault re-unlocked from keyfile for a waiting request"
                            );
                            true
                        }
                        Err(error) => {
                            log::warn!(
                                "portal: keyfile re-unlock failed: {error}; falling back to the prompt"
                            );
                            false
                        }
                    }
                } else {
                    false
                }
            };
            if keyfile_unlock {
                true
            } else {
                match prompt_and_unlock(&state, prompter.as_ref(), &requests) {
                    Ok(()) => {
                        log::info!("portal: secret vault unlocked via password prompt");
                        true
                    }
                    Err(reason) => {
                        log::warn!("portal: vault unlock did not complete: {reason}");
                        false
                    }
                }
            }
        };
        complete_unlock_requests(&state, requests, unlocked);
    }
}

fn prompt_and_unlock(
    state: &Arc<Mutex<SecretState>>,
    prompter: &dyn SecretPrompter,
    requests: &[PendingUnlock],
) -> Result<(), String> {
    let cancelled = || {
        requests.iter().all(|request| {
            sync::lock(&request.tracker, "secret tracker").was_closed(&request.request_path)
        })
    };
    let password = match prompter.prompt_secret(
        "Unlock Keyring",
        Some("The secret vault is locked. Enter its password to unlock it."),
        &cancelled,
    ) {
        // The String's allocation moves into the pinned buffer unchanged.
        Ok(PromptResponse::Secret(value)) => LockedBytes::new(value.into_bytes()),
        Ok(PromptResponse::Cancelled) => return Err("prompt dismissed".into()),
        Err(error) => return Err(format!("secret prompt failed: {error}")),
    };
    let password = password
        .as_str()
        .ok_or_else(|| "prompt returned a non-UTF-8 password".to_owned())?;
    sync::lock(state, "secret state")
        .unlock_with_password(password)
        .map_err(|error| format!("wrong password or unreadable vault: {error}"))
}

fn complete_unlock_requests(
    state: &Arc<Mutex<SecretState>>,
    requests: Vec<PendingUnlock>,
    unlocked: bool,
) {
    for request in requests {
        let cancelled =
            sync::lock(&request.tracker, "secret tracker").was_closed(&request.request_path);
        let result = if cancelled || !unlocked {
            PortalUnlockOutcome::Dismissed
        } else {
            deliver_portal_secret(state, request.fd, &request.app_id)
        };
        if request.outcome.send_blocking(result).is_err() {
            log::warn!("portal: RetrieveSecret caller went away before unlock completed");
        }
    }
}

fn deliver_portal_secret(
    state: &Arc<Mutex<SecretState>>,
    fd: OwnedFd,
    app_id: &str,
) -> PortalUnlockOutcome {
    let mut secret = {
        let state = sync::lock(state, "secret state");
        match state.vault.as_ref() {
            Some(vault) => portal::derive_portal_secret(vault.get_master_key(), app_id),
            None => return PortalUnlockOutcome::Failed,
        }
    };
    let written = write_secret_fd(fd, &secret);
    secret.zeroize();
    match written {
        Ok(()) => PortalUnlockOutcome::Delivered,
        Err(error) => {
            log::warn!("portal: could not write RetrieveSecret fd after unlock: {error}");
            PortalUnlockOutcome::Failed
        }
    }
}

/// PAM and the backend use the same root-owned session runtime location.
/// Never trust an inherited XDG_RUNTIME_DIR for a login password token.
fn pam_token_path() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/{PAM_TOKEN_NAME}"))
}

/// A consumed PAM token. The v2 format carries the derived vault master
/// key; the legacy format is the raw login password. Both payloads are
/// pinned and zeroized through [`LockedBytes`] for their whole lifetime.
enum PamToken {
    Password(LockedBytes),
    /// Exactly 32 bytes, guaranteed by `parse_pam_token`.
    MasterKey(LockedBytes),
}

/// Interpret consumed token content. The v2 prefix commits to the
/// master-key format: a key that fails to decode makes the whole token
/// invalid (failing closed) rather than falling back to treating the
/// content as a password.
fn parse_pam_token(content: &LockedBytes) -> Option<PamToken> {
    let text = content.as_str()?;
    let text = text.trim_end_matches(['\n', '\r']);
    if let Some(hex) = text.strip_prefix(PAM_TOKEN_KEY_PREFIX) {
        let mut key = Vault::key_from_hex(hex).ok()?;
        let token = LockedBytes::new(key.to_vec());
        key.zeroize();
        Some(PamToken::MasterKey(token))
    } else if text.is_empty() {
        None
    } else {
        Some(PamToken::Password(LockedBytes::new(
            text.as_bytes().to_vec(),
        )))
    }
}

fn consume_pam_token() -> Option<PamToken> {
    consume_pam_token_at(&pam_token_path())
}

/// Open a one-shot token without following links, validate the opened file,
/// unlink the name before reading, and cap its size. The bytes are pinned
/// and zeroized through [`LockedBytes`].
fn consume_pam_token_at(path: &Path) -> Option<PamToken> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.len() > MAX_PAM_TOKEN_BYTES
    {
        log::warn!("portal: refusing unsafe PAM token at {}", path.display());
        let _ = std::fs::remove_file(path);
        return None;
    }
    // Refuse reuse even if parsing or password validation later fails.
    std::fs::remove_file(path).ok()?;

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).ok()?;
    let content = LockedBytes::new(bytes);
    parse_pam_token(&content)
}

fn init() -> Result<Arc<Mutex<SecretState>>, SecretError> {
    let dir = dirs::data_dir()
        .ok_or(SecretError::NoDataDir)?
        .join("aegis")
        .join("secrets");
    init_in(&dir)
}

fn init_in(dir: &Path) -> Result<Arc<Mutex<SecretState>>, SecretError> {
    prepare_private_dir(dir)?;

    let key_path = dir.join("vault.key");
    let keyfile_present = key_path.exists();
    let mut state = SecretState {
        vault: None,
        pending_unlocks: Vec::new(),
        unlock_worker_active: false,
        vault_path: dir.join("vault.enc"),
        salt_path: dir.join("vault.salt"),
        kdf_path: dir.join("vault.kdf"),
        keyfile_path: keyfile_present.then_some(key_path.clone()),
    };

    if key_path.exists() {
        let hex = read_regular_file(&key_path, MAX_KEYFILE_BYTES, true)?;
        let hex = std::str::from_utf8(&hex)
            .map_err(|_| SecretError::Crypto("vault.key is not UTF-8".to_owned()))?;
        let mut key = Vault::key_from_hex(hex)?;
        let vault = Vault::new(state.vault_path.clone(), key);
        key.zeroize();
        let missing = !state.vault_path.exists();
        let _validated = vault.load()?;
        if missing {
            vault.save(&VaultData {
                collections: vec![],
            })?;
        }
        state.vault = Some(vault);
        log::info!("portal: secret vault unlocked via keyfile");
    } else if state.salt_path.exists() || state.kdf_path.exists() {
        if !state.vault_path.exists() {
            return Err(SecretError::Vault(
                "password-mode vault has vault.kdf/vault.salt but no vault.enc".to_owned(),
            ));
        }
        // A locked vault still advertises Secret, so validate every backing
        // path now rather than deferring symlink/mode/size failures until a
        // caller is already waiting on an unlock prompt. vault.kdf, when
        // present, is additionally decoded: malformed parameters fail closed.
        if state.salt_path.exists() {
            let _salt = read_regular_file(&state.salt_path, MAX_KEYFILE_BYTES, false)?;
        }
        if state.kdf_path.exists() {
            let kdf = read_regular_file(&state.kdf_path, MAX_KDF_BYTES, false)?;
            let _validated = vault::decode_kdf(&kdf)?;
        }
        Vault::validate_ciphertext(&state.vault_path)?;
        match consume_pam_token() {
            Some(token) => match state.unlock_with_pam_token(&token) {
                Ok(()) => log::info!("portal: secret vault unlocked via PAM token"),
                Err(error) => log::warn!("portal: PAM-token unlock failed: {error}"),
            },
            None => log::info!("portal: password-protected secret vault starts locked"),
        }
    } else {
        if state.vault_path.exists() {
            return Err(SecretError::Vault(
                "refusing to overwrite vault.enc without vault.key or vault.salt".to_owned(),
            ));
        }
        let mut key = Vault::generate_key();
        let encoded = Zeroizing::new(Vault::key_to_hex(&key));
        match vault::atomic_create(&key_path, encoded.as_bytes()) {
            Ok(()) => {
                let vault = Vault::new(state.vault_path.clone(), key);
                key.zeroize();
                vault.save(&VaultData {
                    collections: vec![],
                })?;
                state.vault = Some(vault);
                state.keyfile_path = Some(key_path.clone());
                log::info!("portal: secret vault initialized at {}", dir.display());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another concurrently activated backend won initialization.
                // Use only its fully published key.
                key.zeroize();
                let hex = read_regular_file(&key_path, MAX_KEYFILE_BYTES, true)?;
                let hex = std::str::from_utf8(&hex)
                    .map_err(|_| SecretError::Crypto("vault.key is not UTF-8".to_owned()))?;
                let mut key = Vault::key_from_hex(hex)?;
                let vault = Vault::new(state.vault_path.clone(), key);
                key.zeroize();
                if !state.vault_path.exists() {
                    vault.save(&VaultData {
                        collections: vec![],
                    })?;
                } else {
                    let _validated = vault.load()?;
                }
                state.vault = Some(vault);
                state.keyfile_path = Some(key_path);
            }
            Err(error) => return Err(SecretError::Io(error)),
        }
    }

    Ok(Arc::new(Mutex::new(state)))
}

/// Create the vault directory when absent, then validate it. The create is
/// only for first-run setup; the re-key entry point uses
/// [`validate_private_dir`] directly so it never materializes a directory
/// for a missing vault.
fn prepare_private_dir(dir: &Path) -> Result<(), SecretError> {
    std::fs::create_dir_all(dir)?;
    validate_private_dir(dir)
}

/// Open the final vault directory without following a symlink. Tighten its
/// mode through the descriptor only after owner/type validation, so a
/// rejected path cannot chmod a link target as a side effect.
fn validate_private_dir(dir: &Path) -> Result<(), SecretError> {
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret directory must be a user-owned real directory",
        )));
    }
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    let metadata = directory.metadata()?;
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret directory mode must be 0700",
        )));
    }
    Ok(())
}

/// Read a bounded regular file through an O_NOFOLLOW descriptor. `private`
/// additionally requires exact mode 0600.
fn read_regular_file(
    path: &Path,
    limit: u64,
    private: bool,
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || (private && metadata.permissions().mode() & 0o7777 != 0o600)
        || (!private && metadata.permissions().mode() & 0o022 != 0)
        || metadata.len() > limit
    {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "unsafe file ownership, mode, type, or size: {}",
                path.display()
            ),
        )));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut suffix = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        std::env::temp_dir().join(format!("aegis-secret-test-{tag}-{suffix}"))
    }

    /// Write a legacy (salt-only) password vault fixture with one stored
    /// collection, the way the wssp-era daemons left it on disk.
    fn write_legacy_password_vault(dir: &Path, password: &str, salt: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("vault.salt"), salt).unwrap();
        let mut key =
            Vault::derive_key_with(&Params::default(), password, salt).expect("derive fixture key");
        let vault = Vault::new(dir.join("vault.enc"), key);
        key.zeroize();
        vault
            .save(&VaultData {
                collections: vec![vault::CollectionEntry {
                    id: "login".into(),
                    label: "Login".into(),
                    items: vec![],
                }],
            })
            .expect("save fixture vault");
    }

    /// A locked password-mode state over `dir`, bypassing startup's PAM
    /// token consumption so tests never touch the real session token.
    fn locked_password_state(dir: &Path) -> Arc<Mutex<SecretState>> {
        prepare_private_dir(dir).expect("prepare vault dir");
        Arc::new(Mutex::new(SecretState {
            vault: None,
            pending_unlocks: Vec::new(),
            unlock_worker_active: false,
            vault_path: dir.join("vault.enc"),
            salt_path: dir.join("vault.salt"),
            kdf_path: dir.join("vault.kdf"),
            keyfile_path: None,
        }))
    }

    #[test]
    fn first_run_creates_private_keyfile_and_reopens() {
        let dir = temp_dir("first-run");
        let state = init_in(&dir).expect("first-run init");
        assert!(state.lock().unwrap().is_unlocked());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir.join("vault.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(dir.join("vault.enc").exists());
        assert!(
            init_in(&dir)
                .expect("second init")
                .lock()
                .unwrap()
                .is_unlocked()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn salt_without_ciphertext_is_rejected() {
        let dir = temp_dir("salt-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vault.salt"), "c29tZXNhbHQ").unwrap();
        assert!(init_in(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_ciphertext_is_never_overwritten() {
        let dir = temp_dir("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        let vault_path = dir.join("vault.enc");
        std::fs::write(&vault_path, b"irreplaceable ciphertext").unwrap();
        let before = std::fs::read(&vault_path).unwrap();

        assert!(init_in(&dir).is_err());
        assert_eq!(std::fs::read(&vault_path).unwrap(), before);
        assert!(!dir.join("vault.key").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_directory_is_rejected_without_chmodding_target() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("directory-symlink");
        let target = root.join("target");
        let link = root.join("link");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(init_in(&link).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locked_vault_rejects_a_symlink_salt_at_startup() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("salt-symlink");
        let dir = root.join("secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let target = root.join("salt-target");
        std::fs::write(&target, b"c29tZXNhbHQ").unwrap();
        symlink(&target, dir.join("vault.salt")).unwrap();
        std::fs::write(dir.join("vault.enc"), [0_u8; 24]).unwrap();
        std::fs::set_permissions(
            dir.join("vault.enc"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        assert!(init_in(&dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pam_token_must_be_regular_private_and_is_one_shot() {
        let dir = temp_dir("pam-token");
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token, b"password\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let consumed = consume_pam_token_at(&token);
        match consumed {
            Some(PamToken::Password(password)) => {
                assert_eq!(password.as_str(), Some("password"))
            }
            _ => panic!("a raw token must parse as a legacy password"),
        }
        assert!(!token.exists());

        std::fs::write(&token, b"leaked").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(consume_pam_token_at(&token).is_none());
        assert!(!token.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_vault_unlock_backfills_vault_kdf() {
        let dir = temp_dir("legacy-migration");
        write_legacy_password_vault(&dir, "hunter2", "c29tZXNhbHQ");
        let salt_before = std::fs::read(dir.join("vault.salt")).unwrap();
        assert!(!dir.join("vault.kdf").exists());

        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .unlock_with_password("hunter2")
            .expect("legacy unlock");

        let kdf_path = dir.join("vault.kdf");
        assert!(kdf_path.exists(), "legacy unlock must backfill vault.kdf");
        assert_eq!(
            std::fs::read(dir.join("vault.salt")).unwrap(),
            salt_before,
            "vault.salt stays as the downgrade mirror"
        );
        assert_eq!(
            std::fs::metadata(&kdf_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "vault.kdf must be private"
        );
        let kdf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&kdf_path).unwrap()).unwrap();
        assert_eq!(kdf["version"], 1);
        assert_eq!(kdf["kdf"], "argon2id");
        assert_eq!(kdf["m_cost"], Params::DEFAULT_M_COST);
        assert_eq!(kdf["t_cost"], Params::DEFAULT_T_COST);
        assert_eq!(kdf["p_cost"], Params::DEFAULT_P_COST);
        assert_eq!(kdf["salt"], "c29tZXNhbHQ");

        // The migrated vault keeps decrypting through vault.kdf, the wrong
        // password still fails, and migration is idempotent.
        let kdf_bytes = std::fs::read(&kdf_path).unwrap();
        let wrong = locked_password_state(&dir);
        assert!(
            wrong
                .lock()
                .unwrap()
                .unlock_with_password("wrong-password")
                .is_err(),
            "a wrong password must not unlock a migrated vault"
        );
        let migrated = locked_password_state(&dir);
        migrated
            .lock()
            .unwrap()
            .unlock_with_password("hunter2")
            .expect("migrated unlock");
        {
            let guard = migrated.lock().unwrap();
            let loaded = guard.vault.as_ref().unwrap().load().unwrap();
            assert_eq!(loaded.collections[0].id, "login");
        }
        assert_eq!(
            std::fs::read(&kdf_path).unwrap(),
            kdf_bytes,
            "migration must be idempotent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vault_kdf_with_custom_params_is_used_on_load() {
        let dir = temp_dir("custom-kdf");
        std::fs::create_dir_all(&dir).unwrap();
        let params = Params::new(64, 3, 1, None).expect("valid custom parameters");
        let salt = Vault::generate_salt();
        std::fs::write(dir.join("vault.salt"), &salt).unwrap();
        std::fs::write(
            dir.join("vault.kdf"),
            vault::encode_kdf(&params, &salt).unwrap(),
        )
        .unwrap();
        let mut key = Vault::derive_key_with(&params, "hunter2", &salt).unwrap();
        let vault = Vault::new(dir.join("vault.enc"), key);
        key.zeroize();
        vault
            .save(&VaultData {
                collections: vec![],
            })
            .unwrap();

        // Startup validates vault.kdf, and unlock must derive with its
        // parameters: under crate defaults the key would not decrypt.
        let state = init_in(&dir).expect("init custom-kdf vault");
        state
            .lock()
            .unwrap()
            .unlock_with_password("hunter2")
            .expect("vault.kdf parameters must be used on load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_vault_kdf_fails_closed() {
        for (tag, contents) in [
            (
                "bad-version",
                br#"{"version":2,"kdf":"argon2id","m_cost":19456,"t_cost":2,"p_cost":1,"salt":"c29tZXNhbHQ"}"#
                    .as_slice(),
            ),
            (
                "unknown-kdf",
                br#"{"version":1,"kdf":"scrypt","m_cost":19456,"t_cost":2,"p_cost":1,"salt":"c29tZXNhbHQ"}"#,
            ),
            ("truncated", br#"{"version":1,"kdf":"arg"#),
        ] {
            let dir = temp_dir(&format!("bad-kdf-{tag}"));
            write_legacy_password_vault(&dir, "hunter2", "c29tZXNhbHQ");
            std::fs::write(dir.join("vault.kdf"), contents).unwrap();

            assert!(
                init_in(&dir).is_err(),
                "{tag}: startup must fail closed on a malformed vault.kdf"
            );
            let state = locked_password_state(&dir);
            assert!(
                state
                    .lock()
                    .unwrap()
                    .unlock_with_password("hunter2")
                    .is_err(),
                "{tag}: unlock must fail closed on a malformed vault.kdf"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn create_password_vault_writes_private_files() {
        let dir = temp_dir("create-password");
        let state = create_password_vault_in(&dir, "hunter2").expect("create password vault");
        assert!(state.lock().unwrap().is_unlocked());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "vault directory must stay 0700"
        );
        for name in ["vault.enc", "vault.kdf", "vault.salt"] {
            let metadata = std::fs::metadata(dir.join(name)).unwrap();
            assert!(metadata.is_file(), "{name} must be a regular file");
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "{name} must be private"
            );
        }
        let kdf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("vault.kdf")).unwrap()).unwrap();
        assert_eq!(kdf["version"], 1);
        assert_eq!(kdf["kdf"], "argon2id");
        assert_eq!(kdf["m_cost"], Params::DEFAULT_M_COST);
        assert_eq!(kdf["t_cost"], Params::DEFAULT_T_COST);
        assert_eq!(kdf["p_cost"], Params::DEFAULT_P_COST);
        assert_eq!(
            kdf["salt"].as_str().unwrap(),
            std::fs::read_to_string(dir.join("vault.salt")).unwrap(),
            "vault.kdf and vault.salt must agree"
        );

        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("hunter2")
            .expect("created vault unlocks with its password");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_password_vault_never_clobbers_an_existing_vault() {
        // Over a password-mode vault.
        let dir = temp_dir("clobber-password");
        create_password_vault_in(&dir, "hunter2").expect("create password vault");
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        assert!(create_password_vault_in(&dir, "other").is_err());
        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        let _ = std::fs::remove_dir_all(&dir);

        // Over a legacy salt-only vault.
        let dir = temp_dir("clobber-legacy");
        write_legacy_password_vault(&dir, "hunter2", "c29tZXNhbHQ");
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        assert!(create_password_vault_in(&dir, "other").is_err());
        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        assert!(
            !dir.join("vault.kdf").exists(),
            "a refused create must not leave vault.kdf behind"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // Over a keyfile vault.
        let dir = temp_dir("clobber-keyfile");
        let _keyfile = init_in(&dir).expect("keyfile init");
        assert!(create_password_vault_in(&dir, "hunter2").is_err());
        assert!(dir.join("vault.key").exists());
        assert!(!dir.join("vault.salt").exists());
        assert!(!dir.join("vault.kdf").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_password_rotates_salt_and_rekeys_the_vault() {
        let dir = temp_dir("change-password");
        let state = create_password_vault_in(&dir, "old-password").expect("create password vault");
        state
            .lock()
            .unwrap()
            .vault
            .as_ref()
            .unwrap()
            .save(&VaultData {
                collections: vec![vault::CollectionEntry {
                    id: "login".into(),
                    label: "Login".into(),
                    items: vec![],
                }],
            })
            .expect("store a collection");
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        let kdf_before = std::fs::read(dir.join("vault.kdf")).unwrap();
        let salt_before = std::fs::read(dir.join("vault.salt")).unwrap();

        // A wrong current password is a clean error and touches no file.
        assert!(
            state
                .lock()
                .unwrap()
                .change_password("wrong-password", "new-password")
                .is_err()
        );
        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        assert_eq!(std::fs::read(dir.join("vault.kdf")).unwrap(), kdf_before);
        assert_eq!(std::fs::read(dir.join("vault.salt")).unwrap(), salt_before);

        state
            .lock()
            .unwrap()
            .change_password("old-password", "new-password")
            .expect("re-key the vault");
        assert!(state.lock().unwrap().is_unlocked());

        let salt_after = std::fs::read(dir.join("vault.salt")).unwrap();
        assert_ne!(salt_after, salt_before, "the salt must rotate");
        let kdf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("vault.kdf")).unwrap()).unwrap();
        assert_eq!(
            kdf["salt"].as_str().unwrap(),
            std::str::from_utf8(&salt_after).unwrap(),
            "vault.kdf and vault.salt must both update"
        );
        for name in ["vault.enc", "vault.kdf", "vault.salt"] {
            assert_eq!(
                std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{name} must stay private"
            );
        }

        // The old password is dead, the new one unlocks, contents survive.
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("new password unlocks");
        {
            let guard = reopened.lock().unwrap();
            let loaded = guard.vault.as_ref().unwrap().load().unwrap();
            assert_eq!(loaded.collections[0].id, "login");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_password_on_a_legacy_vault_records_vault_kdf() {
        let dir = temp_dir("change-legacy");
        write_legacy_password_vault(&dir, "old-password", "c29tZXNhbHQ");
        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .change_password("old-password", "new-password")
            .expect("re-key a legacy vault");

        assert!(dir.join("vault.kdf").exists());
        let kdf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("vault.kdf")).unwrap()).unwrap();
        let salt = std::fs::read_to_string(dir.join("vault.salt")).unwrap();
        assert_eq!(kdf["salt"].as_str().unwrap(), salt);
        assert_ne!(salt, "c29tZXNhbHQ", "the salt must rotate");
        assert!(
            !dir.join("vault.kdf.next").exists() && !dir.join("vault.salt.next").exists(),
            "the two-phase re-key leaves no pending files behind"
        );

        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("new password unlocks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_password_vault_in_rekeys_without_service_state() {
        let dir = temp_dir("rekey-entry");
        let state = create_password_vault_in(&dir, "old-password").expect("create password vault");
        state
            .lock()
            .unwrap()
            .vault
            .as_ref()
            .unwrap()
            .save(&VaultData {
                collections: vec![vault::CollectionEntry {
                    id: "login".into(),
                    label: "Login".into(),
                    items: vec![],
                }],
            })
            .expect("store a collection");
        drop(state);

        rekey_password_vault_in(&dir, "old-password", "new-password").expect("re-key the vault");

        // The old password is dead, the new one unlocks, contents survive,
        // and the persisted files stay private.
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("new password unlocks");
        {
            let guard = reopened.lock().unwrap();
            let loaded = guard.vault.as_ref().unwrap().load().unwrap();
            assert_eq!(loaded.collections[0].id, "login");
        }
        for name in ["vault.enc", "vault.kdf", "vault.salt"] {
            assert_eq!(
                std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{name} must stay private"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_password_vault_in_with_a_wrong_current_password_touches_nothing() {
        let dir = temp_dir("rekey-wrong");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        let kdf_before = std::fs::read(dir.join("vault.kdf")).unwrap();
        let salt_before = std::fs::read(dir.join("vault.salt")).unwrap();

        assert!(rekey_password_vault_in(&dir, "wrong-password", "new-password").is_err());

        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        assert_eq!(std::fs::read(dir.join("vault.kdf")).unwrap(), kdf_before);
        assert_eq!(std::fs::read(dir.join("vault.salt")).unwrap(), salt_before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_password_vault_in_on_a_legacy_vault_uses_the_legacy_kdf() {
        let dir = temp_dir("rekey-legacy");
        write_legacy_password_vault(&dir, "old-password", "c29tZXNhbHQ");
        assert!(!dir.join("vault.kdf").exists());

        rekey_password_vault_in(&dir, "old-password", "new-password")
            .expect("re-key a legacy vault");

        let kdf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("vault.kdf")).unwrap()).unwrap();
        assert_eq!(kdf["m_cost"], Params::DEFAULT_M_COST);
        assert_eq!(
            kdf["salt"].as_str().unwrap(),
            std::fs::read_to_string(dir.join("vault.salt")).unwrap()
        );
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("new password unlocks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_password_vault_in_never_creates_a_missing_vault() {
        let dir = temp_dir("rekey-missing");
        assert!(rekey_password_vault_in(&dir, "old-password", "new-password").is_err());
        assert!(
            !dir.exists(),
            "the re-key entry point must not materialize a vault directory"
        );

        // An existing directory without a vault is left without one as well.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(rekey_password_vault_in(&dir, "old-password", "new-password").is_err());
        assert!(!dir.join("vault.enc").exists());
        assert!(!dir.join("vault.kdf").exists());
        assert!(!dir.join("vault.salt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derived_key_token_unlocks_a_password_vault() {
        let dir = temp_dir("key-token");
        let state = create_password_vault_in(&dir, "hunter2").expect("create password vault");
        state
            .lock()
            .unwrap()
            .vault
            .as_ref()
            .unwrap()
            .save(&VaultData {
                collections: vec![vault::CollectionEntry {
                    id: "login".into(),
                    label: "Login".into(),
                    items: vec![],
                }],
            })
            .expect("store a collection");
        drop(state);

        let token = derive_token_key_in(&dir, b"hunter2").expect("derive a v2 token");
        assert!(token.starts_with(PAM_TOKEN_KEY_PREFIX));
        assert_eq!(token.len(), PAM_TOKEN_KEY_PREFIX.len() + 64);
        assert!(
            token.len() as u64 <= MAX_PAM_TOKEN_BYTES,
            "the v2 token must fit the consume cap"
        );

        // Plant it like the PAM module would and consume it like the daemon.
        let token_path = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token_path, token.as_bytes()).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let consumed = consume_pam_token_at(&token_path).expect("consume the v2 token");
        assert!(!token_path.exists(), "the token stays single-shot");

        let locked = locked_password_state(&dir);
        locked
            .lock()
            .unwrap()
            .unlock_with_pam_token(&consumed)
            .expect("the v2 token unlocks the vault");
        let guard = locked.lock().unwrap();
        let loaded = guard.vault.as_ref().unwrap().load().unwrap();
        assert_eq!(loaded.collections[0].id, "login");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derived_key_token_uses_legacy_default_params_for_salt_only_vaults() {
        let dir = temp_dir("key-token-legacy");
        write_legacy_password_vault(&dir, "hunter2", "c29tZXNhbHQ");

        let token = derive_token_key_in(&dir, b"hunter2").expect("derive for a legacy vault");
        let token_path = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token_path, token.as_bytes()).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let consumed = consume_pam_token_at(&token_path).expect("consume the v2 token");

        let locked = locked_password_state(&dir);
        locked
            .lock()
            .unwrap()
            .unlock_with_pam_token(&consumed)
            .expect("the v2 token unlocks a legacy vault");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derived_key_token_uses_authoritative_kdf_params() {
        let dir = temp_dir("key-token-custom-kdf");
        std::fs::create_dir_all(&dir).unwrap();
        let params = Params::new(64, 3, 1, None).expect("valid custom parameters");
        let salt = Vault::generate_salt();
        std::fs::write(dir.join("vault.salt"), &salt).unwrap();
        std::fs::write(
            dir.join("vault.kdf"),
            vault::encode_kdf(&params, &salt).unwrap(),
        )
        .unwrap();
        let mut key = Vault::derive_key_with(&params, "hunter2", &salt).unwrap();
        let vault = Vault::new(dir.join("vault.enc"), key);
        key.zeroize();
        vault
            .save(&VaultData {
                collections: vec![],
            })
            .unwrap();

        // The token must derive through vault.kdf's custom parameters:
        // crate defaults would produce a key that does not decrypt.
        let token = derive_token_key_in(&dir, b"hunter2").expect("derive with custom params");
        let token_path = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token_path, token.as_bytes()).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let consumed = consume_pam_token_at(&token_path).expect("consume the v2 token");
        let locked = locked_password_state(&dir);
        locked
            .lock()
            .unwrap()
            .unlock_with_pam_token(&consumed)
            .expect("a custom-params token unlocks the vault");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_key_tokens_fail_closed() {
        let dir = temp_dir("key-token-garbage");
        create_password_vault_in(&dir, "hunter2").expect("create password vault");
        let token_path = dir.join(PAM_TOKEN_NAME);
        let cases = [
            format!("{PAM_TOKEN_KEY_PREFIX}zz-not-hex"),
            format!("{PAM_TOKEN_KEY_PREFIX}abcd"),
            format!("{PAM_TOKEN_KEY_PREFIX}{}", "0".repeat(63)),
            format!("{PAM_TOKEN_KEY_PREFIX}{}", "0".repeat(65)),
            PAM_TOKEN_KEY_PREFIX.to_owned(),
        ];
        for bad in cases {
            std::fs::write(&token_path, bad.as_bytes()).unwrap();
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(
                consume_pam_token_at(&token_path).is_none(),
                "a malformed key token must fail closed: {bad:?}"
            );
            assert!(!token_path.exists(), "the token is still deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_key_token_fails_closed_and_stays_single_shot() {
        let dir = temp_dir("key-token-wrong");
        create_password_vault_in(&dir, "hunter2").expect("create password vault");
        let mut wrong_key = [0x5a_u8; 32];
        let token = format!("{PAM_TOKEN_KEY_PREFIX}{}", Vault::key_to_hex(&wrong_key));
        wrong_key.zeroize();
        let token_path = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token_path, token.as_bytes()).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let consumed = consume_pam_token_at(&token_path).expect("a well-formed key token parses");
        assert!(!token_path.exists());
        let locked = locked_password_state(&dir);
        assert!(
            locked
                .lock()
                .unwrap()
                .unlock_with_pam_token(&consumed)
                .is_err(),
            "authenticated decryption rejects the wrong key"
        );
        assert!(!locked.lock().unwrap().is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_password_tokens_still_unlock() {
        let dir = temp_dir("password-token");
        create_password_vault_in(&dir, "hunter2").expect("create password vault");
        let token_path = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token_path, b"hunter2\n").unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let consumed = consume_pam_token_at(&token_path).expect("consume a legacy token");
        let locked = locked_password_state(&dir);
        locked
            .lock()
            .unwrap()
            .unlock_with_pam_token(&consumed)
            .expect("a legacy password token still unlocks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_token_key_in_only_derives_for_password_vaults() {
        // A keyfile vault never gets a token: the daemon unlocks itself.
        let dir = temp_dir("derive-keyfile");
        let _keyfile = init_in(&dir).expect("keyfile init");
        assert!(derive_token_key_in(&dir, b"hunter2").is_none());
        let _ = std::fs::remove_dir_all(&dir);

        // A missing directory derives nothing.
        assert!(derive_token_key_in(&temp_dir("derive-missing"), b"hunter2").is_none());

        // An orphan ciphertext is not a password-mode vault.
        let dir = temp_dir("derive-orphan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vault.enc"), b"orphan").unwrap();
        assert!(derive_token_key_in(&dir, b"hunter2").is_none());
        let _ = std::fs::remove_dir_all(&dir);

        // KDF material without a ciphertext is not a password-mode vault.
        let dir = temp_dir("derive-salt-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vault.salt"), "c29tZXNhbHQ").unwrap();
        assert!(derive_token_key_in(&dir, b"hunter2").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_token_key_in_rejects_non_utf8_passwords() {
        let dir = temp_dir("derive-non-utf8");
        create_password_vault_in(&dir, "hunter2").expect("create password vault");
        assert!(
            derive_token_key_in(&dir, &[0xff, 0xfe]).is_none(),
            "a non-UTF-8 password falls back to the raw token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The point after which a simulated crash interrupts the two-phase
    /// re-key write protocol.
    enum CrashAfter {
        /// `vault.kdf.next` + `vault.salt.next` staged, `vault.enc` untouched.
        PendingWrites,
        /// `vault.enc` swapped to the new key, pending pair not adopted.
        CiphertextSwap,
        /// `vault.kdf.next` renamed into place, `vault.salt.next` pending.
        KdfRename,
        /// Both renames done: the fully consistent final state.
        SaltRename,
    }

    /// Drive the two-phase re-key write protocol of `change_password` by
    /// hand, stopping after `stop`. The vault in `dir` must currently unlock
    /// with `old_password`; the staged pair always describes `new_password`
    /// under crate-default parameters, exactly as the real re-key stages it.
    fn simulate_rekey_crash(dir: &Path, old_password: &str, new_password: &str, stop: CrashAfter) {
        let vault_path = dir.join("vault.enc");
        let kdf_path = dir.join("vault.kdf");
        let salt_path = dir.join("vault.salt");
        let kdf_next = next_path(&kdf_path);
        let salt_next = next_path(&salt_path);

        let state = locked_password_state(dir);
        let (_vault, data, _winner, _kdf) = state
            .lock()
            .unwrap()
            .open_with_password(old_password)
            .expect("prove the old password");

        let new_salt = Vault::generate_salt();
        let new_params = Params::default();
        let mut new_key = Vault::derive_key_with(&new_params, new_password, &new_salt).unwrap();
        let new_vault = Vault::new(vault_path, new_key);
        new_key.zeroize();
        let encoded = vault::encode_kdf(&new_params, &new_salt).unwrap();

        vault::atomic_replace(&kdf_next, &encoded).unwrap();
        vault::atomic_replace(&salt_next, new_salt.as_bytes()).unwrap();
        if matches!(stop, CrashAfter::PendingWrites) {
            return;
        }
        new_vault.save(&data).unwrap();
        if matches!(stop, CrashAfter::CiphertextSwap) {
            return;
        }
        std::fs::rename(&kdf_next, &kdf_path).unwrap();
        if matches!(stop, CrashAfter::KdfRename) {
            return;
        }
        std::fs::rename(&salt_next, &salt_path).unwrap();
    }

    #[test]
    fn crash_after_pending_writes_heals_by_cleanup() {
        let dir = temp_dir("crash-pending");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        simulate_rekey_crash(
            &dir,
            "old-password",
            "new-password",
            CrashAfter::PendingWrites,
        );
        assert!(dir.join("vault.kdf.next").exists());
        assert!(dir.join("vault.salt.next").exists());

        // The ciphertext never moved: the old password still unlocks, the
        // new one never took effect, and the stray pair is cleaned up.
        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .unlock_with_password("old-password")
            .expect("the old password still unlocks");
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("new-password")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_after_ciphertext_swap_heals_by_adoption() {
        let dir = temp_dir("crash-enc");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        simulate_rekey_crash(
            &dir,
            "old-password",
            "new-password",
            CrashAfter::CiphertextSwap,
        );
        let kdf_next = std::fs::read(dir.join("vault.kdf.next")).unwrap();
        let salt_next = std::fs::read(dir.join("vault.salt.next")).unwrap();

        // The new ciphertext decrypts under the pending pair, which is then
        // adopted into its final position — mirror included.
        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("the pending pair unlocks the new ciphertext");
        assert_eq!(std::fs::read(dir.join("vault.kdf")).unwrap(), kdf_next);
        assert_eq!(std::fs::read(dir.join("vault.salt")).unwrap(), salt_next);
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());

        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err(),
            "the old password is dead once the ciphertext moved"
        );
        let reopened = locked_password_state(&dir);
        reopened
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("the adopted vault keeps unlocking");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_after_kdf_rename_heals_by_finishing() {
        let dir = temp_dir("crash-kdf-rename");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        simulate_rekey_crash(&dir, "old-password", "new-password", CrashAfter::KdfRename);
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(dir.join("vault.salt.next").exists());

        // vault.kdf already describes the new key and wins immediately; the
        // pending mirror is cleaned up.
        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("the adopted vault.kdf unlocks");
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_after_salt_rename_is_already_consistent() {
        let dir = temp_dir("crash-salt-rename");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        simulate_rekey_crash(&dir, "old-password", "new-password", CrashAfter::SaltRename);
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());

        let state = locked_password_state(&dir);
        state
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("the fully renamed trio unlocks");
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());
        let stale = locked_password_state(&dir);
        assert!(
            stale
                .lock()
                .unwrap()
                .unlock_with_password("old-password")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_password_fails_closed_and_preserves_pending_files() {
        let dir = temp_dir("crash-wrong-password");
        create_password_vault_in(&dir, "old-password").expect("create password vault");
        simulate_rekey_crash(
            &dir,
            "old-password",
            "new-password",
            CrashAfter::CiphertextSwap,
        );

        let state = locked_password_state(&dir);
        assert!(
            state
                .lock()
                .unwrap()
                .unlock_with_password("wrong-password")
                .is_err(),
            "no candidate decrypts under a wrong password"
        );
        assert!(!state.lock().unwrap().is_unlocked());
        assert!(
            dir.join("vault.kdf.next").exists() && dir.join("vault.salt.next").exists(),
            "a failed unlock leaves the pending pair in place for diagnosis"
        );

        // The pending pair survives, so the right new password still heals.
        state
            .lock()
            .unwrap()
            .unlock_with_password("new-password")
            .expect("the pending pair still unlocks afterwards");
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_password_refuses_a_keyfile_vault() {
        let dir = temp_dir("change-keyfile");
        let state = init_in(&dir).expect("keyfile init");
        assert!(state.lock().unwrap().is_unlocked());
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        let key_before = std::fs::read(dir.join("vault.key")).unwrap();

        assert!(
            state
                .lock()
                .unwrap()
                .change_password("old-password", "new-password")
                .is_err(),
            "a keyfile vault has no password KDF material"
        );
        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        assert_eq!(std::fs::read(dir.join("vault.key")).unwrap(), key_before);
        assert!(!dir.join("vault.kdf.next").exists());
        assert!(!dir.join("vault.salt.next").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn locked_bytes_pin_zeroize_and_release() {
        // Smoke test only: mlock may legitimately be refused under
        // RLIMIT_MEMLOCK and must never fail construction.
        let locked = LockedBytes::new(b"hunter2".to_vec());
        assert_eq!(locked.as_str(), Some("hunter2"));
        assert_eq!(locked.as_bytes(), b"hunter2");
        drop(locked);
        let empty = LockedBytes::new(Vec::new());
        assert!(empty.as_bytes().is_empty());
        assert_eq!(empty.as_str(), Some(""));
    }

    #[test]
    fn secret_service_explicit_lock_purges_master_key() {
        struct DummyPrompter;
        impl SecretPrompter for DummyPrompter {
            fn prompt_secret(
                &self,
                _title: &str,
                _reason: Option<&str>,
                _cancelled: &dyn Fn() -> bool,
            ) -> Result<PromptResponse, String> {
                Ok(PromptResponse::Cancelled)
            }
        }

        let dir = temp_dir("explicit-lock");
        let service = SecretService {
            state: init_in(&dir).expect("init"),
            prompter: Arc::new(DummyPrompter),
        };
        assert!(service.is_unlocked());
        service.lock();
        assert!(!service.is_unlocked());
        // Idempotent
        service.lock();
        assert!(!service.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_lock_then_unlock_round_trips_a_keyfile_vault() {
        struct DummyPrompter;
        impl SecretPrompter for DummyPrompter {
            fn prompt_secret(
                &self,
                _title: &str,
                _reason: Option<&str>,
                _cancelled: &dyn Fn() -> bool,
            ) -> Result<PromptResponse, String> {
                Ok(PromptResponse::Cancelled)
            }
        }

        let dir = temp_dir("session-roundtrip");
        let service = SecretService {
            state: init_in(&dir).expect("init"),
            prompter: Arc::new(DummyPrompter),
        };
        assert!(service.is_keyfile_mode());
        assert!(service.is_unlocked());

        // The desktop locked the session: master key remains secured in memory for the login session.
        service.lock_for_session();
        assert!(service.is_unlocked());

        // Explicit lock purges memory:
        service.lock();
        assert!(!service.is_unlocked());

        // The session returned: a keyfile vault re-unlocks without a
        // credential — it never had one to prompt for.
        service.unlock_for_session();
        assert!(service.is_unlocked());

        // Idempotent in both directions; a second unlock is a no-op on an
        // already-unlocked vault.
        service.lock_for_session();
        assert!(service.is_unlocked());
        service.unlock_for_session();
        assert!(service.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_unlock_leaves_a_password_vault_locked() {
        struct DummyPrompter;
        impl SecretPrompter for DummyPrompter {
            fn prompt_secret(
                &self,
                _title: &str,
                _reason: Option<&str>,
                _cancelled: &dyn Fn() -> bool,
            ) -> Result<PromptResponse, String> {
                Ok(PromptResponse::Cancelled)
            }
        }

        let dir = temp_dir("password-session-unlock");
        write_legacy_password_vault(&dir, "hunter2", "c29tZXNhbHQ");
        let service = SecretService {
            state: locked_password_state(&dir),
            prompter: Arc::new(DummyPrompter),
        };
        assert!(!service.is_keyfile_mode());
        assert!(!service.is_unlocked());

        // The session returning never grants a locked password vault: its only
        // re-unlock paths are the PAM token and the masked prompt.
        service.unlock_for_session();
        assert!(!service.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyfile_reunlock_fails_closed_on_a_swapped_key() {
        struct DummyPrompter;
        impl SecretPrompter for DummyPrompter {
            fn prompt_secret(
                &self,
                _title: &str,
                _reason: Option<&str>,
                _cancelled: &dyn Fn() -> bool,
            ) -> Result<PromptResponse, String> {
                Ok(PromptResponse::Cancelled)
            }
        }

        let dir = temp_dir("swapped-key");
        let service = SecretService {
            state: init_in(&dir).expect("init"),
            prompter: Arc::new(DummyPrompter),
        };
        service.lock();
        assert!(!service.is_unlocked());

        // Swap in a syntactically valid key that does not decrypt the
        // vault: the re-unlock must fail closed and stay locked.
        let other = Vault::generate_key();
        let encoded = Zeroizing::new(Vault::key_to_hex(&other));
        std::fs::write(dir.join("vault.key"), encoded.as_bytes()).unwrap();
        service.unlock_for_session();
        assert!(!service.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
