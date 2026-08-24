//! At-rest vault crypto for the secret store.
//!
//! The vault file is `serde_json` serialized [`VaultData`] encrypted with
//! XChaCha20-Poly1305 under the 32-byte master key; on disk it is a 24-byte
//! nonce prefix followed by the ciphertext. The format is byte-compatible
//! with the wssp vault so an existing `vault.enc` keeps working.
//!
//! Startup prefers keyfile mode (`vault.key` holding the master key as hex).
//! Password mode derives the master key with Argon2id: the `vault.kdf`
//! sidecar (JSON) persists the exact KDF parameters plus salt and is
//! authoritative when present, while a bare `vault.salt` marks a legacy
//! vault that implies the crate-default parameters. A successful legacy
//! unlock backfills `vault.kdf` and keeps `vault.salt` as a downgrade
//! mirror for older daemons, so an argon2-crate default change can never
//! silently invalidate existing vaults.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use argon2::password_hash::SaltString;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::SecretError;

const MAX_VAULT_BYTES: u64 = 64 * 1024 * 1024;

/// The only `vault.kdf` schema version this build reads and writes.
const KDF_FILE_VERSION: u32 = 1;
/// The only password KDF this build supports; every `Argon2` instance
/// constructed here is Argon2id v1.3.
const KDF_NAME: &str = "argon2id";

/// The persisted `vault.kdf` sidecar: the exact Argon2id parameters and
/// salt a password-mode vault is keyed with, recorded on disk so the
/// derivation stays reproducible across argon2-crate default changes.
#[derive(Serialize, Deserialize)]
struct KdfFile {
    version: u32,
    kdf: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
}

/// The decrypted vault commitment.
///
/// For the native Secret portal, `vault.enc` acts as an authenticated
/// commitment (Poly1305 AEAD validation) confirming that a candidate key
/// is correct before exposing the master key for HKDF expansion.
/// The payload retains 100% backward compatibility with existing on-disk
/// `{"collections": [...]}` files.
#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct VaultData {
    #[serde(default)]
    pub collections: Vec<CollectionEntry>,
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct CollectionEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    #[zeroize(skip)]
    pub items: Vec<serde_json::Value>,
}

/// An open vault: its file location plus the master key in memory.
pub struct Vault {
    path: PathBuf,
    // Heap-boxed so the mlock'd address stays valid when the Vault moves.
    master_key: Box<[u8; 32]>,
    mlocked: bool,
}

impl Drop for Vault {
    fn drop(&mut self) {
        self.master_key.zeroize();
        if self.mlocked {
            // SAFETY: the boxed key was successfully mlock'd in `new` and its
            // heap address has not changed since; it is zeroized above before
            // the pages are released.
            unsafe {
                libc::munlock(
                    self.master_key.as_ptr().cast::<libc::c_void>(),
                    self.master_key.len(),
                );
            }
        }
    }
}

impl Vault {
    pub fn new(path: PathBuf, master_key: [u8; 32]) -> Self {
        let mut vault = Self {
            path,
            master_key: Box::new(master_key),
            mlocked: false,
        };
        // SAFETY: the boxed key is a valid readable 32-byte region owned by
        // this process; mlock only pins its pages against swapping. Failure
        // (for example RLIMIT_MEMLOCK) is non-fatal and reported below —
        // an unlock must never fail over it.
        let result = unsafe {
            libc::mlock(
                vault.master_key.as_ptr().cast::<libc::c_void>(),
                vault.master_key.len(),
            )
        };
        if result == 0 {
            vault.mlocked = true;
        } else {
            log::warn!(
                "portal: could not mlock the vault master key: {}",
                io::Error::last_os_error()
            );
        }
        // Independent of the mlock outcome: keep the key's pages out of any
        // core dump image, including a piped core handler, which the
        // process-wide RLIMIT_CORE=0 alone cannot guarantee.
        crate::mark_dontdump(
            "the vault master key",
            vault.master_key.as_ptr(),
            vault.master_key.len(),
        );
        vault
    }

    /// The in-memory master key. Callers must derive purpose-separated keys
    /// from it (HKDF) instead of handing it out directly.
    pub fn get_master_key(&self) -> &[u8; 32] {
        &self.master_key
    }

    /// Argon2id password KDF under explicit parameters, as loaded from
    /// `vault.kdf` (or chosen for a fresh vault). Legacy `vault.salt`-only
    /// vaults and freshly keyed ones use `Params::default()`, the exact
    /// equivalent of `Argon2::default()`; every keyed vault then records
    /// its parameters in `vault.kdf`.
    pub fn derive_key_with(
        params: &Params,
        password: &str,
        salt_str: &str,
    ) -> Result<[u8; 32], SecretError> {
        let salt = SaltString::from_b64(salt_str)
            .map_err(|e| SecretError::Crypto(format!("invalid salt: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone());

        let mut key = [0u8; 32];
        if let Err(error) =
            argon2.hash_password_into(password.as_bytes(), salt.as_str().as_bytes(), &mut key)
        {
            key.zeroize();
            return Err(SecretError::Crypto(format!("hash failed: {error}")));
        }

        Ok(key)
    }

    /// Fresh Argon2id salt for keying or re-keying a password-mode vault.
    pub fn generate_salt() -> String {
        SaltString::generate(&mut OsRng).as_str().to_string()
    }

    /// Fresh random master key for keyfile mode.
    pub fn generate_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        key
    }

    pub fn key_to_hex(key: &[u8; 32]) -> String {
        key.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn key_from_hex(hex: &str) -> Result<[u8; 32], SecretError> {
        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(SecretError::Crypto(
                "vault.key must be 64 hex chars (32 bytes)".into(),
            ));
        }
        let mut arr = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let s = match std::str::from_utf8(chunk) {
                Ok(value) => value,
                Err(_) => {
                    arr.zeroize();
                    return Err(SecretError::Crypto("invalid hex in vault.key".into()));
                }
            };
            arr[i] = match u8::from_str_radix(s, 16) {
                Ok(value) => value,
                Err(_) => {
                    arr.zeroize();
                    return Err(SecretError::Crypto(format!("invalid hex byte: {s}")));
                }
            };
        }
        Ok(arr)
    }

    /// Validate a locked vault's file boundary without needing its key.
    /// Authenticated decryption still happens before the vault is unlocked.
    pub(crate) fn validate_ciphertext(path: &Path) -> Result<(), SecretError> {
        let Some(file_data) = read_vault_file(path, false)? else {
            return Err(SecretError::Vault("vault file is missing".to_owned()));
        };
        if file_data.len() < 24 {
            return Err(SecretError::Vault("vault file corrupted".to_string()));
        }
        Ok(())
    }

    /// Encrypt and persist the vault contents (24-byte nonce prefix +
    /// ciphertext), replacing any previous version atomically.
    pub fn save(&self, data: &VaultData) -> Result<(), SecretError> {
        atomic_replace(&self.path, &self.seal(data)?).map_err(SecretError::Io)
    }

    /// Encrypt and persist a brand-new vault, refusing to replace an
    /// existing file (same first-start race discipline as `atomic_create`).
    pub(crate) fn save_new(&self, data: &VaultData) -> Result<(), SecretError> {
        atomic_create(&self.path, &self.seal(data)?).map_err(SecretError::Io)
    }

    /// Serialize and encrypt the vault contents: 24-byte nonce prefix +
    /// XChaCha20-Poly1305 ciphertext.
    fn seal(&self, data: &VaultData) -> Result<Vec<u8>, SecretError> {
        let serialized = Zeroizing::new(serde_json::to_vec(data)?);
        let cipher = XChaCha20Poly1305::new((&*self.master_key).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, serialized.as_ref())
            .map_err(|e| SecretError::Crypto(format!("encryption failure: {e}")))?;

        let mut final_data = nonce.to_vec();
        final_data.extend_from_slice(&ciphertext);
        Ok(final_data)
    }

    /// Load and decrypt the vault. A missing file reads as an empty vault; a
    /// truncated or undecryptable file is an error.
    pub fn load(&self) -> Result<VaultData, SecretError> {
        let Some(file_data) = read_vault_file(&self.path, true)? else {
            return Ok(VaultData {
                collections: vec![],
            });
        };
        if file_data.len() < 24 {
            return Err(SecretError::Vault("vault file corrupted".to_string()));
        }

        let (nonce_bytes, ciphertext) = file_data.split_at(24);
        let nonce = XNonce::from_slice(nonce_bytes);

        let cipher = XChaCha20Poly1305::new((&*self.master_key).into());
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| SecretError::Crypto(format!("decryption failure: {e}")))?,
        );

        let data: VaultData = serde_json::from_slice(plaintext.as_ref())?;
        Ok(data)
    }
}

/// Serialize the `vault.kdf` sidecar from the parameters actually in use —
/// never from hardcoded literals — plus the salt.
pub(crate) fn encode_kdf(params: &Params, salt: &str) -> Result<Vec<u8>, SecretError> {
    let kdf = KdfFile {
        version: KDF_FILE_VERSION,
        kdf: KDF_NAME.to_owned(),
        m_cost: params.m_cost(),
        t_cost: params.t_cost(),
        p_cost: params.p_cost(),
        salt: salt.to_owned(),
    };
    Ok(serde_json::to_vec(&kdf)?)
}

/// Parse a `vault.kdf` sidecar into the exact Argon2id parameters and salt
/// it describes. Anything unknown — schema version, KDF name, out-of-range
/// parameters, a malformed salt — fails closed.
pub(crate) fn decode_kdf(bytes: &[u8]) -> Result<(Params, String), SecretError> {
    let kdf: KdfFile = serde_json::from_slice(bytes)
        .map_err(|e| SecretError::Vault(format!("vault.kdf is malformed: {e}")))?;
    if kdf.version != KDF_FILE_VERSION {
        return Err(SecretError::Vault(format!(
            "unsupported vault.kdf version {}",
            kdf.version
        )));
    }
    if kdf.kdf != KDF_NAME {
        return Err(SecretError::Vault(format!(
            "unsupported vault.kdf algorithm {}",
            kdf.kdf
        )));
    }
    let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, None)
        .map_err(|e| SecretError::Vault(format!("invalid vault.kdf parameters: {e}")))?;
    SaltString::from_b64(&kdf.salt)
        .map_err(|e| SecretError::Vault(format!("invalid vault.kdf salt: {e}")))?;
    Ok((params, kdf.salt))
}

fn read_vault_file(path: &Path, missing_is_empty: bool) -> Result<Option<Vec<u8>>, SecretError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if missing_is_empty && error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(SecretError::Io(error)),
    };
    let metadata = file.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.len() > MAX_VAULT_BYTES
    {
        return Err(SecretError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "vault.enc must be a user-owned regular file, mode 0600, at most 64 MiB",
        )));
    }
    let mut file_data = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut file_data)?;
    Ok(Some(file_data))
}

/// Durably replace one file without ever truncating the previous version.
/// The temporary file lives beside the destination so rename is atomic.
pub(crate) fn atomic_replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_replace_with(path, contents, || Ok(()))
}

/// Durably create a private file without replacing an existing path. A hard
/// link publishes a fully written same-directory temporary atomically; this
/// closes the first-start race between two activated backend processes.
pub(crate) fn atomic_create(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "vault path must have a parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "vault path must name a file")
    })?;
    let temporary_path = temporary_path(parent, file_name);
    let result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)?;
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        fs::hard_link(&temporary_path, path)?;
        fs::remove_file(&temporary_path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn atomic_replace_with<F>(path: &Path, contents: &[u8], before_rename: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "vault path must have a parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "vault path must name a file")
    })?;

    let temporary_path = temporary_path(parent, file_name);

    let result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)?;
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        before_rename()?;
        fs::rename(&temporary_path, path)?;

        // Persist the directory entry replacement, not just the file data.
        File::open(parent)?.sync_all()
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn temporary_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let mut random = [0u8; 16];
    OsRng.fill_bytes(&mut random);
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".{suffix}.tmp"));
    parent.join(temporary_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn vault_crypto_roundtrip() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let vault_path = std::env::temp_dir().join(format!("aegis-vault-test-{suffix}.enc"));

        let password = "super-secret-password";
        let salt = Vault::generate_salt();
        let key = Vault::derive_key_with(&Params::default(), password, &salt)
            .expect("key derivation failed");

        let vault = Vault::new(vault_path.clone(), key);
        let data = VaultData {
            collections: vec![CollectionEntry {
                id: "login".into(),
                label: "Login".into(),
                items: vec![serde_json::json!({
                    "id": "i1",
                    "label": "Item",
                    "attributes": {"app": "aegis"},
                    "secret": [104, 117, 110, 116, 101, 114, 50]
                })],
            }],
        };

        vault.save(&data).expect("save failed");
        let loaded = vault.load().expect("load failed");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].id, "login");

        let _ = std::fs::remove_file(vault_path);
    }

    #[test]
    fn kdf_sidecar_roundtrip_with_non_default_params() {
        let params = Params::new(64, 3, 2, None).expect("valid custom parameters");
        let salt = Vault::generate_salt();

        let encoded = encode_kdf(&params, &salt).expect("encode vault.kdf");
        let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["kdf"], "argon2id");
        assert_eq!(json["m_cost"], 64);
        assert_eq!(json["t_cost"], 3);
        assert_eq!(json["p_cost"], 2);
        assert_eq!(json["salt"].as_str().unwrap(), salt);

        let (decoded_params, decoded_salt) = decode_kdf(&encoded).expect("decode vault.kdf");
        assert_eq!(decoded_params, params);
        assert_eq!(decoded_salt, salt);

        let mut custom_key = Vault::derive_key_with(&params, "password", &salt).unwrap();
        let mut decoded_key =
            Vault::derive_key_with(&decoded_params, "password", &decoded_salt).unwrap();
        assert_eq!(
            custom_key, decoded_key,
            "a decoded vault.kdf must derive the same key"
        );
        let mut default_key =
            Vault::derive_key_with(&Params::default(), "password", &salt).unwrap();
        assert_ne!(
            custom_key, default_key,
            "non-default parameters must actually change the derived key"
        );
        custom_key.zeroize();
        decoded_key.zeroize();
        default_key.zeroize();
    }

    #[test]
    fn malformed_kdf_sidecars_fail_closed() {
        let salt = Vault::generate_salt();
        let valid = encode_kdf(&Params::default(), &salt).unwrap();
        let (decoded_params, decoded_salt) =
            decode_kdf(&valid).expect("a well-formed sidecar decodes");
        assert_eq!(decoded_params, Params::default());
        assert_eq!(decoded_salt, salt);

        let cases: Vec<&[u8]> = vec![
            // Unsupported schema version.
            br#"{"version":2,"kdf":"argon2id","m_cost":19456,"t_cost":2,"p_cost":1,"salt":"c29tZXNhbHQ"}"#,
            // Unknown KDF name.
            br#"{"version":1,"kdf":"scrypt","m_cost":19456,"t_cost":2,"p_cost":1,"salt":"c29tZXNhbHQ"}"#,
            // Out-of-range parameters (m_cost below the Argon2 minimum).
            br#"{"version":1,"kdf":"argon2id","m_cost":1,"t_cost":2,"p_cost":1,"salt":"c29tZXNhbHQ"}"#,
            // A salt that is not a valid SaltString body.
            br#"{"version":1,"kdf":"argon2id","m_cost":19456,"t_cost":2,"p_cost":1,"salt":"!!!"}"#,
            // Truncated JSON.
            br#"{"version":1,"kdf":"arg"#,
            // Empty file.
            b"",
        ];
        for case in cases {
            assert!(
                decode_kdf(case).is_err(),
                "malformed vault.kdf must fail closed: {case:?}"
            );
        }
    }

    #[test]
    fn atomic_replace_preserves_previous_file_before_rename() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-atomic-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.enc");
        fs::write(&path, b"previous valid vault").unwrap();

        let error = atomic_replace_with(&path, b"replacement", || {
            Err(io::Error::other("injected failure before rename"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"previous valid vault");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_replace_creates_private_file() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-mode-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.enc");

        atomic_replace(&path, b"ciphertext").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"ciphertext");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_create_never_replaces_an_existing_file() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-create-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.key");
        atomic_create(&path, b"first").unwrap();
        assert_eq!(
            atomic_create(&path, b"second").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
