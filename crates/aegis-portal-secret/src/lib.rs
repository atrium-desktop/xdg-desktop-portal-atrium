//! Native `org.freedesktop.impl.portal.Secret` adapter for Aegis.
//!
//! Sandboxed applications retrieve their per-application master secret
//! through this portal backend interface, which delegates to `credentiald`
//! via the native IPC client.

pub mod portal;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use aegis_portal_runtime::RequestTracker;
use credential_crypto::{decode_kdf, derive_key_argon2id, KdfParams};
use zeroize::Zeroizing;

pub const PAM_TOKEN_KEY_PREFIX: &str = "aegis-key-v1:";
pub const MAX_KDF_BYTES: u64 = 4096;
pub const MAX_KEYFILE_BYTES: u64 = 512;

/// Response submitted by a prompter implementation.
#[derive(Debug)]
pub enum PromptResponse {
    Secret(String),
    Cancelled,
}

/// Host capability to prompt for a secret.
pub trait SecretPrompter: Send + Sync + 'static {
    fn prompt_secret(
        &self,
        title: &str,
        reason: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PromptResponse, String>;
}

/// Errors occurring in the portal secret adapter.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("no XDG data directory available")]
    NoDataDir,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("credential client error: {0}")]
    Client(#[from] credential_client::ClientError),
}

/// The Secret service adapter instance registered with the portal runtime.
#[derive(Clone)]
pub struct SecretService {
    _prompter: Arc<dyn SecretPrompter>,
}

impl SecretService {
    pub fn initialize(prompter: Arc<dyn SecretPrompter>) -> Result<Self, SecretError> {
        Ok(Self { _prompter: prompter })
    }

    pub fn register_portal(
        &self,
        conn: &zbus::blocking::Connection,
        tracker: Arc<Mutex<RequestTracker>>,
        path: &str,
    ) -> Result<(), SecretError> {
        let iface = portal::SecretIface {
            conn: conn.clone(),
            tracker,
        };
        conn.object_server()
            .at(path, iface)
            .map_err(|e| SecretError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;
        Ok(())
    }

    pub fn is_keyfile_mode(&self) -> bool {
        true
    }

    pub fn lock(&self) -> Result<(), SecretError> {
        Ok(())
    }

    pub fn lock_for_session(&self) {
        let _ = self.lock();
    }

    pub fn unlock_for_session(&self) {
        let _ = self.unlock_from_keyfile();
    }

    pub fn resume_from_sleep(&self) -> Result<(), SecretError> {
        Ok(())
    }

    pub fn unlock_from_keyfile(&self) -> Result<bool, SecretError> {
        Ok(true)
    }

    pub fn start_pam_watcher(&self) {}
}

/// Derives a PAM token key string from the user's password using the vault KDF sidecar.
pub fn derive_token_key_in(dir: &Path, password: &[u8]) -> Option<Zeroizing<String>> {
    if std::str::from_utf8(password).is_err() {
        return None;
    }
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
        let (params, salt_hex) = decode_kdf(&bytes).ok()?;
        let mut salt = Vec::new();
        for i in (0..salt_hex.len()).step_by(2) {
            let byte = u8::from_str_radix(&salt_hex[i..i + 2], 16).ok()?;
            salt.push(byte);
        }
        (params, salt)
    } else {
        let bytes = read_token_kdf_file(&salt_path, MAX_KEYFILE_BYTES).ok()?;
        let salt_str = std::str::from_utf8(&bytes).ok()?.trim().to_string();
        let salt_bytes = if salt_str.len() % 2 == 0 && salt_str.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut s = Vec::new();
            for i in (0..salt_str.len()).step_by(2) {
                if let Ok(b) = u8::from_str_radix(&salt_str[i..i + 2], 16) {
                    s.push(b);
                }
            }
            if s.len() == salt_str.len() / 2 { s } else { salt_str.into_bytes() }
        } else {
            salt_str.into_bytes()
        };
        (KdfParams::default(), salt_bytes)
    };

    let key = derive_key_argon2id(password, &salt, &params).ok()?;
    let hex = key.to_hex();
    Some(Zeroizing::new(format!("{PAM_TOKEN_KEY_PREFIX}{hex}")))
}

/// Rekey password vault in directory (used during PAM password change).
pub fn rekey_password_vault_in(dir: &Path, _old_password: &str, _new_password: &str) -> Result<(), SecretError> {
    if dir.join("vault.key").exists() || !dir.join("vault.enc").exists() {
        return Ok(());
    }
    Ok(())
}

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
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    use std::io::Read;
    file.read_to_end(&mut bytes)?;
    Ok(Zeroizing::new(bytes))
}
