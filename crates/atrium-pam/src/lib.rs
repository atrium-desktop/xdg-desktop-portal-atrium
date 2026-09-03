//! `pam_atrium`: bridge the login password and the tessera secret vault.
//!
//! Installed as `auth/session/password optional pam_atrium.so`, the module
//! does two jobs and never grants or denies anything (`optional`):
//!
//! **Vault unlock token.** `pam_sm_authenticate` stashes the just-verified
//! authtok in PAM module data behind a zeroizing cleanup — no file is
//! written yet, because later modules in the auth stack can still fail the
//! login. `pam_sm_setcred` (credential commit) and `pam_sm_open_session`,
//! whichever fires first, write the token to
//! `/run/user/<uid>/atrium-pam-token` mode 0600 through the hardened
//! temp-then-rename path and zeroize the stash. A failed write keeps the
//! stash so the later hook retries: logind may create `/run/user/<uid>`
//! only when the session is registered. When the account holds a
//! password-mode vault, the planted content is the derived vault master key
//! (`aegis-key-v1:<hex>`, `atrium_portal_secret::derive_token_key_in`)
//! instead of the raw password, narrowing the at-rest leak from the login
//! password to the vault key; a keyfile-mode vault unlocks itself from
//! `vault.key`, so no token is planted for it at all.
//! `xdg-desktop-portal-atrium` consumes and deletes the token to unlock a
//! password-mode vault without prompting — the wssp-pam pattern. On a
//! failed login `pam_end` runs the stash cleanup and no token file ever
//! exists; pure-auth stacks that never commit credentials or open a session
//! (some screen lockers) therefore plant no token.
//!
//! **Vault password propagation.** The `pam_sm_chauthtok` update phase
//! re-keys the target user's password-mode vault from `PAM_OLDAUTHTOK` to
//! `PAM_AUTHTOK`, so the vault password tracks the login password. Every
//! failure is swallowed (a password change is never blocked by vault
//! propagation) and nothing sensitive is logged; admin-initiated resets
//! skip the vault, which then falls back to the Portal's own unlock prompt.
//!
//! Failure posture: at worst the vault stays locked or stale and the Portal
//! prompts for the vault password through its own UI.
//!
//! ## Why direct libpam FFI instead of pamsm
//!
//! pamsm 0.4.3 does expose `pam_set_data`/`pam_get_data` with cleanup
//! callbacks and a `chauthtok` hook. Its `pam_module!` macro, however,
//! types the libpam `flags` argument as the `PamFlag` enum, which has no
//! `PAM_PRELIM_CHECK` (0x4000) or `PAM_UPDATE_AUTHTOK` (0x2000) variants —
//! nor combined values such as `PAM_SILENT|PAM_ESTABLISH_CRED` — so every
//! chauthtok call would hand Rust an invalid enum discriminant (undefined
//! behavior) exactly where the phase must be read. The macro likewise types
//! `argc` as `usize` while libpam passes a 32-bit `int`, and the `Pam`
//! wrapper never exposes the raw handle for a manual escape hatch. The six
//! entry points are therefore defined against libpam's stable C ABI with
//! `flags`/`argc` as `c_int`, bit-tested explicitly.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// The token file name under the user's runtime directory.
pub const TOKEN_NAME: &str = "atrium-pam-token";

/// The PAM module-data name under which the stashed authtok lives between
/// `authenticate` and the first committing hook.
const STASH_NAME: &[u8] = b"pam_atrium_authtok\0";

// libpam status codes used below (<security/_pam_types.h>).
const PAM_SUCCESS: c_int = 0;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_SESSION_ERR: c_int = 14;
const PAM_IGNORE: c_int = 25;

// libpam flags (<security/_pam_types.h>, <security/pam_modules.h>).
const PAM_DELETE_CRED: c_int = 0x0004;
const PAM_UPDATE_AUTHTOK: c_int = 0x2000;
const PAM_PRELIM_CHECK: c_int = 0x4000;

// libpam item types (<security/_pam_types.h>).
const PAM_USER: c_int = 2;
const PAM_AUTHTOK: c_int = 6;
const PAM_OLDAUTHTOK: c_int = 7;

/// The opaque libpam handle (`pam_handle_t`); modules only ever pass it
/// back to libpam.
type PamHandle = *mut c_void;

/// libpam module-data cleanup callback (`pam_set_data(3)`).
type PamCleanup = unsafe extern "C" fn(PamHandle, *mut c_void, c_int);

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_get_item(pamh: PamHandle, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_getenv(pamh: PamHandle, name: *const c_char) -> *const c_char;
    fn pam_set_data(
        pamh: PamHandle,
        module_data_name: *const c_char,
        data: *mut c_void,
        cleanup: Option<PamCleanup>,
    ) -> c_int;
    fn pam_get_data(
        pamh: PamHandle,
        module_data_name: *const c_char,
        data: *mut *const c_void,
    ) -> c_int;
}

/// PAM `auth` entry point: capture the just-verified authtok and park it in
/// PAM module data. No token file is written here — later modules in the
/// stack can still fail the login, and a planted file would outlive it.
///
/// # Safety
/// Called by libpam with a valid handle and the module's argv; the handle
/// is only passed back to libpam and no borrowed pointer outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_authenticate(
    pamh: PamHandle,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    authenticate(pamh)
}

/// PAM credential entry point: credentials are committed only after a
/// successful authentication, so this is the earliest point the token may
/// be planted. The teardown call (`PAM_DELETE_CRED`) never plants.
///
/// # Safety
/// Called by libpam with a valid handle and the module's argv; the handle
/// is only passed back to libpam and no borrowed pointer outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_setcred(
    pamh: PamHandle,
    flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    if setcred_plants(flags) {
        return plant_stashed_token(pamh);
    }
    PAM_SUCCESS
}

/// PAM `session` entry point: plants the token when no setcred hook ran
/// (session-only stack), or retries after a setcred-time failure. Stacked
/// after logind's session module, `/run/user/<uid>` already exists here
/// even for a first login after boot.
///
/// # Safety
/// Called by libpam with a valid handle and the module's argv; the handle
/// is only passed back to libpam and no borrowed pointer outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_open_session(
    pamh: PamHandle,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    plant_stashed_token(pamh)
}

/// PAM session teardown: nothing to undo; the token is single-shot and
/// `/run/user/<uid>` is reclaimed by logind.
///
/// # Safety
/// Called by libpam with a valid handle; it is never dereferenced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_close_session(
    _pamh: PamHandle,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// PAM `account` entry point: intentionally unimplemented. `PAM_IGNORE` is
/// the one status that can never influence a stack, whatever control flag
/// an administrator chooses.
///
/// # Safety
/// Called by libpam with a valid handle; it is never dereferenced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_acct_mgmt(
    _pamh: PamHandle,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_IGNORE
}

/// PAM `password` entry point: on the update phase only, propagate the
/// password change to the target user's vault. The preliminary probe phase
/// changes nothing. Always succeeds — the module is optional.
///
/// # Safety
/// Called by libpam with a valid handle and the module's argv; the handle
/// is only passed back to libpam and no borrowed pointer outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_chauthtok(
    pamh: PamHandle,
    flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    if chauthtok_updates(flags) {
        rekey_vault_from_pam(pamh);
    }
    PAM_SUCCESS
}

/// setcred plants the token whenever credentials are being committed —
/// anything but the teardown call. A bare `pam_setcred(0)` from minimal
/// clients counts as a commit, mirroring libpam's own default.
fn setcred_plants(flags: c_int) -> bool {
    flags & PAM_DELETE_CRED == 0
}

/// Only an explicit chauthtok update phase may touch the vault;
/// `PAM_PRELIM_CHECK` probes must change nothing. The two phase flags are
/// mutually exclusive per `pam_modules.h`; an ambiguous or absent value
/// skips. `PAM_CHANGE_EXPIRED_AUTHTOK` may accompany the update flag and is
/// still an update.
fn chauthtok_updates(flags: c_int) -> bool {
    (flags & (PAM_UPDATE_AUTHTOK | PAM_PRELIM_CHECK)) == PAM_UPDATE_AUTHTOK
}

/// The authtok captured in `authenticate`, parked in PAM module data until
/// the login is confirmed (setcred/open_session) or the handle ends.
struct PendingToken {
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// The account's home directory, kept to resolve the vault directory at
    /// plant time (the same way the chauthtok hook resolves it).
    home: Vec<u8>,
    /// Set once the token has been handled — planted or deliberately
    /// suppressed; the authtok is zeroized at the same moment, so no later
    /// hook re-plants it.
    planted: bool,
    authtok: Vec<u8>,
}

impl PendingToken {
    /// Write the token file through `write`, then zeroize the in-memory
    /// authtok. `payload` is what actually lands on disk — the derived
    /// vault key when a password-mode vault exists, the raw authtok
    /// otherwise. A failed write keeps the stash intact so the other hook
    /// can retry — `/run/user/<uid>` can appear only once logind registers
    /// the session.
    fn promote(
        &mut self,
        payload: &[u8],
        write: impl FnOnce(libc::uid_t, libc::gid_t, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.planted {
            return Ok(());
        }
        write(self.uid, self.gid, payload)?;
        self.authtok.zeroize();
        self.planted = true;
        Ok(())
    }

    /// Decide against planting (a keyfile-mode vault unlocks itself, so a
    /// token is pointless): zeroize the stash and mark it handled without
    /// touching the filesystem.
    fn suppress(&mut self) {
        self.authtok.zeroize();
        self.planted = true;
    }
}

/// libpam cleanup for the stashed token: zeroize and release it. Runs at
/// `pam_end` — including failed logins, where no token file was ever
/// written — and when a re-authentication replaces the stash
/// (`PAM_DATA_REPLACE`).
unsafe extern "C" fn pending_token_cleanup(_pamh: PamHandle, data: *mut c_void, _status: c_int) {
    if data.is_null() {
        return;
    }
    // SAFETY: `data` is the Box<PendingToken> pointer adopted by
    // pam_set_data; libpam calls this cleanup exactly once for it.
    let mut stash = unsafe { Box::from_raw(data.cast::<PendingToken>()) };
    stash.authtok.zeroize();
}

/// Copy a C-string PAM item out of libpam's memory. The copy is ours to
/// zeroize; the original stays owned (and scrubbed) by libpam.
fn get_item_bytes(pamh: PamHandle, item_type: c_int) -> Option<Zeroizing<Vec<u8>>> {
    let mut item: *const c_void = std::ptr::null();
    // SAFETY: `item` is writable out-pointer storage; on success libpam
    // points it at handle-owned memory that stays valid until pam_end and
    // is copied out here.
    let rc = unsafe { pam_get_item(pamh, item_type, &mut item) };
    if rc != PAM_SUCCESS || item.is_null() {
        return None;
    }
    // SAFETY: on PAM_SUCCESS the item is a NUL-terminated C string.
    let bytes = unsafe { CStr::from_ptr(item.cast::<c_char>()) }.to_bytes();
    Some(Zeroizing::new(bytes.to_vec()))
}

/// Copy a variable out of the PAM environment — never the process
/// environment. This module runs inside setuid and otherwise privileged
/// clients, so only values the application deliberately passed through PAM
/// are trusted. `name` must be NUL-terminated (byte-string literals with a
/// trailing `\0`).
fn pam_env_bytes(pamh: PamHandle, name: &'static [u8]) -> Option<Vec<u8>> {
    // SAFETY: `name` is a NUL-terminated static; the returned pointer is
    // libpam-owned and copied out immediately.
    let value = unsafe { pam_getenv(pamh, name.as_ptr().cast()) };
    if value.is_null() {
        return None;
    }
    // SAFETY: a non-null PAM environment value is a NUL-terminated C string.
    Some(unsafe { CStr::from_ptr(value) }.to_bytes().to_vec())
}

/// `authenticate`: stash the authtok under the resolved account identity;
/// write nothing.
fn authenticate(pamh: PamHandle) -> c_int {
    let Some(user) = get_item_bytes(pamh, PAM_USER) else {
        return PAM_USER_UNKNOWN;
    };
    let Some(mut authtok) = get_item_bytes(pamh, PAM_AUTHTOK) else {
        return PAM_SUCCESS;
    };
    if authtok.is_empty() {
        return PAM_SUCCESS;
    }

    let Ok(c_user) = CString::new(user.as_slice()) else {
        return PAM_USER_UNKNOWN;
    };
    let Some(entry) = passwd_entry(&c_user) else {
        return PAM_USER_UNKNOWN;
    };

    // Move the bytes out of the zeroizing shell; the stash cleanup owns
    // scrubbing from here on.
    let authtok = std::mem::take(&mut *authtok);
    stash_token(
        pamh,
        PendingToken {
            uid: entry.uid,
            gid: entry.gid,
            home: entry.home,
            planted: false,
            authtok,
        },
    );
    PAM_SUCCESS
}

/// Park the token in PAM module data until the login is confirmed or the
/// handle ends. A storage failure is silent: the module is optional and the
/// vault simply stays locked.
fn stash_token(pamh: PamHandle, stash: PendingToken) {
    let boxed = Box::into_raw(Box::new(stash));
    // SAFETY: STASH_NAME is NUL-terminated and `boxed` is a live Box
    // pointer. On success libpam adopts the pointer until
    // pending_token_cleanup runs (pam_end or replacement); on failure it
    // does not, so reclaim and scrub it here.
    let rc = unsafe {
        pam_set_data(
            pamh,
            STASH_NAME.as_ptr().cast(),
            boxed.cast(),
            Some(pending_token_cleanup),
        )
    };
    if rc != PAM_SUCCESS {
        // SAFETY: storage failed; the box was never adopted by libpam.
        let mut stash = unsafe { Box::from_raw(boxed) };
        stash.authtok.zeroize();
    }
}

/// Run `f` against the stashed token, if `authenticate` parked one.
fn with_pending_token<R>(pamh: PamHandle, f: impl FnOnce(&mut PendingToken) -> R) -> Option<R> {
    let mut data: *const c_void = std::ptr::null();
    // SAFETY: `data` is valid out-pointer storage and STASH_NAME is
    // NUL-terminated.
    let rc = unsafe { pam_get_data(pamh, STASH_NAME.as_ptr().cast(), &mut data) };
    if rc != PAM_SUCCESS || data.is_null() {
        return None;
    }
    // SAFETY: `data` is the Box<PendingToken> pointer stored by
    // stash_token; the pointee is ours to mutate until libpam runs the
    // cleanup. PAM drives one handle's hooks sequentially on a single
    // thread, so no second live reference can alias this one.
    Some(f(unsafe { &mut *(data.cast_mut().cast::<PendingToken>()) }))
}

/// Plant the stashed token (setcred/open_session, whichever fires first).
/// No stash means `authenticate` never ran or found nothing — a no-op.
fn plant_stashed_token(pamh: PamHandle) -> c_int {
    let Some(result) = with_pending_token(pamh, |stash| {
        if stash.planted {
            return Ok(());
        }
        let xdg_data_home = pam_env_bytes(pamh, b"XDG_DATA_HOME\0");
        let dir = vault_dir(xdg_data_home.as_deref(), &stash.home);
        match token_payload(dir.as_deref(), &stash.authtok) {
            TokenPayload::Skip => {
                stash.suppress();
                Ok(())
            }
            TokenPayload::Plant(content) => stash.promote(content.as_slice(), write_token),
        }
    }) else {
        return PAM_SUCCESS;
    };
    match result {
        Ok(()) => PAM_SUCCESS,
        Err(_) => PAM_SESSION_ERR,
    }
}

/// What to plant beneath the runtime directory, decided at plant time from
/// the account's vault layout.
enum TokenPayload {
    /// A keyfile-mode vault unlocks itself from `vault.key`: a token is
    /// pointless and planting one only leaves secret material at rest.
    Skip,
    /// The file content: the derived vault key for a password-mode vault,
    /// the raw authtok otherwise (today's behavior).
    Plant(Zeroizing<Vec<u8>>),
}

/// Whether `dir` holds a vault and how the token should be planted for it.
/// Mirrors daemon startup like `vault_plan`: a keyfile (`vault.key`) takes
/// precedence and unlocks without any token; `vault.kdf` or `vault.salt`
/// beside `vault.enc` marks password mode, whose token carries the derived
/// master key. Anything else keeps the legacy raw-password plant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlantPlan {
    Skip,
    Raw,
    Derived,
}

fn plant_plan(dir: &Path) -> PlantPlan {
    if dir.join("vault.key").exists() {
        return PlantPlan::Skip;
    }
    if !dir.join("vault.enc").exists() {
        return PlantPlan::Raw;
    }
    if dir.join("vault.kdf").exists() || dir.join("vault.salt").exists() {
        PlantPlan::Derived
    } else {
        PlantPlan::Raw
    }
}

/// Decide the token content for the stashed authtok. A password-mode vault
/// gets the derived master key (falling back to the raw password when the
/// derivation declines, for example a non-UTF-8 password); a keyfile-mode
/// vault skips planting; anything unresolved plants the raw password
/// exactly as before.
fn token_payload(dir: Option<&Path>, authtok: &[u8]) -> TokenPayload {
    let raw = || TokenPayload::Plant(Zeroizing::new(authtok.to_vec()));
    let Some(dir) = dir else {
        return raw();
    };
    match plant_plan(dir) {
        PlantPlan::Skip => TokenPayload::Skip,
        PlantPlan::Raw => raw(),
        PlantPlan::Derived => match atrium_portal_secret::derive_token_key_in(dir, authtok) {
            Some(token) => {
                let mut token = token;
                let content = std::mem::take(&mut *token).into_bytes();
                TokenPayload::Plant(Zeroizing::new(content))
            }
            None => raw(),
        },
    }
}

/// `password` update phase: propagate a changed login password to the
/// vault. Every failure is swallowed — the module is optional and a
/// password change must never be blocked by vault propagation — and nothing
/// sensitive is logged.
fn rekey_vault_from_pam(pamh: PamHandle) {
    let Some(old) = get_item_bytes(pamh, PAM_OLDAUTHTOK) else {
        return;
    };
    let Some(new) = get_item_bytes(pamh, PAM_AUTHTOK) else {
        return;
    };
    if old.is_empty() || new.is_empty() {
        return;
    }
    let Some(user) = get_item_bytes(pamh, PAM_USER) else {
        return;
    };
    let Ok(c_user) = CString::new(user.as_slice()) else {
        return;
    };
    let Some(entry) = passwd_entry(&c_user) else {
        return;
    };
    let xdg_data_home = pam_env_bytes(pamh, b"XDG_DATA_HOME\0");
    let Some(dir) = vault_dir(xdg_data_home.as_deref(), &entry.home) else {
        return;
    };

    // The secret crate validates vault file ownership against the REAL uid,
    // so propagation only happens when the invoking real uid is the target
    // account — a user changing their own password. Admin resets (real uid
    // 0 or another account) skip the vault, which then falls back to the
    // Portal's interactive unlock prompt.
    // SAFETY: both calls only read process credentials.
    let (real_uid, effective_uid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if real_uid != entry.uid {
        return;
    }

    if effective_uid == entry.uid {
        rekey_vault_in_dir(&dir, &old, &new);
        return;
    }

    // Setuid-root clients such as `passwd` run with euid 0 and ruid = the
    // target user; files written as-is would be root-owned and later
    // rejected by the daemon's ownership checks. Borrow the account's
    // filesystem identity for the re-key only.
    let Some(identity) = FsIdentity::assume(entry.uid, entry.gid) else {
        return;
    };
    rekey_vault_in_dir(&dir, &old, &new);
    drop(identity);
}

/// Resolve the vault directory the way `dirs::data_dir()` would for the
/// target account: an absolute `XDG_DATA_HOME` from the PAM environment
/// wins, otherwise `<pw_dir>/.local/share`. `None` when neither source
/// yields an absolute base — a relative vault path inside a privileged
/// process would be resolved against an arbitrary working directory.
fn vault_dir(xdg_data_home: Option<&[u8]>, home: &[u8]) -> Option<PathBuf> {
    let base = if let Some(dir) = xdg_data_home.filter(|dir| dir.first() == Some(&b'/')) {
        PathBuf::from(std::ffi::OsStr::from_bytes(dir))
    } else if home.first() == Some(&b'/') {
        PathBuf::from(std::ffi::OsStr::from_bytes(home)).join(".local/share")
    } else {
        return None;
    };
    Some(base.join("tessera").join("secrets"))
}

/// Whether `dir` holds a password-mode vault that a chauthtok update should
/// re-key. Mirrors daemon startup: a keyfile (`vault.key`) takes precedence
/// and never re-keys by password; `vault.kdf` or `vault.salt` marks
/// password mode, which is only meaningful alongside `vault.enc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VaultPlan {
    Skip,
    Rekey,
}

fn vault_plan(dir: &Path) -> VaultPlan {
    if dir.join("vault.key").exists() || !dir.join("vault.enc").exists() {
        return VaultPlan::Skip;
    }
    if dir.join("vault.kdf").exists() || dir.join("vault.salt").exists() {
        VaultPlan::Rekey
    } else {
        VaultPlan::Skip
    }
}

/// Re-key the password-mode vault in `dir`, swallowing every failure:
/// chauthtok is optional and a password change must never be blocked by
/// vault propagation. Keyfile-mode and absent vaults skip, as do non-UTF-8
/// passwords — the vault's password domain is UTF-8 strings (the prompter
/// and the token consumer both produce `String`).
fn rekey_vault_in_dir(dir: &Path, old: &[u8], new: &[u8]) {
    if vault_plan(dir) != VaultPlan::Rekey {
        return;
    }
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return;
    };
    let _ = atrium_portal_secret::rekey_password_vault_in(dir, old, new);
}

/// A thread's borrowed filesystem identity: setfsuid/setfsgid to the target
/// account for the duration of a vault re-key, restored on drop. Unlike
/// seteuid this touches only the calling thread's filesystem credentials,
/// so nothing else in the PAM client process is affected. The libc calls
/// report ids as `c_int`; the bit pattern round-trips unchanged.
struct FsIdentity {
    previous_uid: c_int,
    previous_gid: c_int,
}

impl FsIdentity {
    /// Become `uid`/`gid` for filesystem operations. `None` when the fsuid
    /// change was refused — the caller then skips the re-key rather than
    /// writing vault files the daemon would reject. The fsgid change is
    /// best-effort: if it is refused, files keep the caller's group, which
    /// stays inaccessible at mode 0600.
    fn assume(uid: libc::uid_t, gid: libc::gid_t) -> Option<Self> {
        // SAFETY: setfsgid/setfsuid change only this thread's filesystem
        // credentials and each call reports the previous value. The probe
        // passes an invalid id, which changes nothing and reports the
        // current value, verifying the change actually took effect before
        // any file is touched.
        unsafe {
            let previous_gid = libc::setfsgid(gid);
            let previous_uid = libc::setfsuid(uid);
            if libc::setfsuid(u32::MAX) != uid.cast_signed() {
                // Best-effort restore before giving up.
                libc::setfsuid(previous_uid.cast_unsigned());
                libc::setfsgid(previous_gid.cast_unsigned());
                return None;
            }
            Some(Self {
                previous_uid,
                previous_gid,
            })
        }
    }
}

impl Drop for FsIdentity {
    fn drop(&mut self) {
        // SAFETY: restoring the thread's recorded filesystem credentials.
        // Restoration uses the process's own login-time ids and cannot
        // realistically fail; if it somehow did, the thread keeps the LESS
        // privileged identity, which fails safe.
        unsafe {
            libc::setfsuid(self.previous_uid.cast_unsigned());
            libc::setfsgid(self.previous_gid.cast_unsigned());
        }
    }
}

/// A resolved passwd entry: the account's ids and home directory.
struct PasswdEntry {
    uid: libc::uid_t,
    gid: libc::gid_t,
    home: Vec<u8>,
}

/// Thread-safe passwd lookup. Display managers may authenticate more than
/// one session concurrently, so the process-global storage from `getpwnam`
/// is not safe inside a PAM module.
fn passwd_entry(user: &CString) -> Option<PasswdEntry> {
    // SAFETY: sysconf reads one process configuration value.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    }
    .clamp(1024, 1024 * 1024);

    loop {
        // SAFETY: passwd is an output-only C struct initialized before use.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        // SAFETY: pointers and buffer are valid for the duration of the call.
        let status = unsafe {
            libc::getpwnam_r(
                user.as_ptr(),
                &mut passwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        return (status == 0 && !result.is_null()).then(|| {
            // SAFETY: a successful getpwnam_r initialized the returned
            // entry; pw_dir (when non-null) points into `buffer` and is
            // copied out before the buffer is dropped.
            let home = unsafe {
                if passwd.pw_dir.is_null() {
                    Vec::new()
                } else {
                    CStr::from_ptr(passwd.pw_dir).to_bytes().to_vec()
                }
            };
            PasswdEntry {
                uid: passwd.pw_uid,
                gid: passwd.pw_gid,
                home,
            }
        });
    }
}

/// PAM runs in a privileged, environment-sensitive process. Never trust an
/// inherited `XDG_RUNTIME_DIR` here: logind's `/run/user/<uid>` location has
/// a root-owned parent and is the only directory accepted by the module.
fn runtime_dir(uid: libc::uid_t) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

fn last_os_error() -> io::Error {
    io::Error::last_os_error()
}

/// Open and validate the logind runtime directory without following a
/// symlink. All subsequent filesystem operations are relative to this file
/// descriptor, closing the check/use race on the directory path.
fn open_runtime_dir(path: &Path, uid: libc::uid_t) -> io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in runtime path"))?;
    // SAFETY: `path` is NUL terminated; open returns a new owned descriptor.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_os_error());
    }
    // SAFETY: `fd` was just returned by open and ownership moves to `File`.
    let directory = unsafe { File::from_raw_fd(fd) };

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and the fd remains open.
    if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(last_os_error());
    }
    // SAFETY: a successful fstat initialized the structure.
    let stat = unsafe { stat.assume_init() };
    let mode = stat.st_mode;
    if mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_uid != uid
        || mode & (libc::S_IRWXG | libc::S_IRWXO) != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory must be owned by the target user and mode 0700",
        ));
    }
    Ok(directory)
}

fn random_temporary_name() -> io::Result<CString> {
    let mut random = [0u8; 16];
    let mut filled = 0;
    while filled < random.len() {
        // SAFETY: the remaining slice is valid writable memory.
        let count = unsafe {
            libc::getrandom(
                random[filled..].as_mut_ptr().cast(),
                random.len() - filled,
                0,
            )
        };
        if count < 0 {
            let error = last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned no data",
            ));
        }
        filled += count as usize;
    }
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    CString::new(format!(".{TOKEN_NAME}.{suffix}.tmp"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temporary name"))
}

fn unlink_at(directory: RawFd, name: &CString) {
    // SAFETY: the directory fd and NUL-terminated relative name are valid.
    let _ = unsafe { libc::unlinkat(directory, name.as_ptr(), 0) };
}

/// Write the token beneath an already selected runtime directory. This is
/// split out so the security invariants can be exercised without a live
/// logind session in unit tests.
fn write_token_in(
    runtime_dir: &Path,
    uid: libc::uid_t,
    gid: libc::gid_t,
    secret: &[u8],
) -> io::Result<()> {
    let directory = open_runtime_dir(runtime_dir, uid)?;
    let directory_fd = directory.as_raw_fd();
    let token_name = CString::new(TOKEN_NAME).expect("static token name has no NUL");

    let (temporary_name, mut temporary) = loop {
        let name = random_temporary_name()?;
        // SAFETY: all pointers are valid and the relative name is NUL
        // terminated. O_EXCL prevents opening any attacker-created entry;
        // O_NOFOLLOW is defense in depth.
        let fd = unsafe {
            libc::openat(
                directory_fd,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is newly owned by this scope.
            break (name, unsafe { File::from_raw_fd(fd) });
        }
        let error = last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    };

    let result = (|| {
        temporary.write_all(secret)?;
        // Set ownership through the descriptor. A path substitution can
        // therefore never redirect privileged chown to another file.
        // SAFETY: the fd remains open for the duration of the call.
        if unsafe { libc::fchown(temporary.as_raw_fd(), uid, gid) } != 0 {
            return Err(last_os_error());
        }
        // SAFETY: the fd remains open for the duration of the call.
        if unsafe { libc::fchmod(temporary.as_raw_fd(), 0o600) } != 0 {
            return Err(last_os_error());
        }
        temporary.sync_all()?;

        // renameat replaces a pre-existing token entry itself; it never
        // follows a symlink stored at the destination.
        // SAFETY: both names are NUL terminated and relative to the open
        // directory descriptor.
        if unsafe {
            libc::renameat(
                directory_fd,
                temporary_name.as_ptr(),
                directory_fd,
                token_name.as_ptr(),
            )
        } != 0
        {
            return Err(last_os_error());
        }
        directory.sync_all()
    })();

    if result.is_err() {
        unlink_at(directory_fd, &temporary_name);
    }
    result
}

/// Write the token atomically, mode 0600, owned by the user.
fn write_token(uid: libc::uid_t, gid: libc::gid_t, secret: &[u8]) -> io::Result<()> {
    write_token_in(&runtime_dir(uid), uid, gid, secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory(tag: &str) -> PathBuf {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("atrium-pam-{tag}-{}-{sequence}", std::process::id()))
    }

    fn current_identity() -> (libc::uid_t, libc::gid_t) {
        // SAFETY: these calls only read the process credentials.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn pending_token(secret: &[u8]) -> PendingToken {
        let (uid, gid) = current_identity();
        PendingToken {
            uid,
            gid,
            home: Vec::new(),
            planted: false,
            authtok: secret.to_vec(),
        }
    }

    #[test]
    fn destination_symlink_is_replaced_without_following_it() {
        let directory = test_directory("symlink");
        let victim = test_directory("victim");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&victim, b"do not touch").unwrap();
        symlink(&victim, directory.join(TOKEN_NAME)).unwrap();
        let (uid, gid) = current_identity();

        write_token_in(&directory, uid, gid, b"login password").unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
        let token = directory.join(TOKEN_NAME);
        let metadata = std::fs::symlink_metadata(&token).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.gid(), gid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(token).unwrap(), b"login password");

        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_file(victim).unwrap();
    }

    #[test]
    fn rejects_runtime_directory_accessible_to_other_users() {
        let directory = test_directory("unsafe-mode");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
        let (uid, gid) = current_identity();

        let error = write_token_in(&directory, uid, gid, b"secret").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!directory.join(TOKEN_NAME).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn promote_writes_once_and_zeroizes_the_stash() {
        let mut stash = pending_token(b"hunter2");
        let payload = b"aegis-key-v1:derived-key-material";
        let mut writes: Vec<(libc::uid_t, libc::gid_t, Vec<u8>)> = Vec::new();
        let (uid, gid) = current_identity();

        stash
            .promote(payload, |uid, gid, secret| {
                writes.push((uid, gid, secret.to_vec()));
                Ok(())
            })
            .unwrap();

        assert_eq!(writes, vec![(uid, gid, payload.to_vec())]);
        assert!(stash.planted);
        assert!(
            stash.authtok.iter().all(|byte| *byte == 0),
            "a planted stash must hold no password bytes"
        );

        stash
            .promote(payload, |_, _, _| {
                panic!("a planted stash must not be written again")
            })
            .unwrap();
    }

    #[test]
    fn promote_retries_after_a_failed_write() {
        let mut stash = pending_token(b"hunter2");

        assert!(
            stash
                .promote(b"hunter2", |_, _, _| Err(io::Error::other(
                    "simulated write failure"
                )))
                .is_err()
        );
        assert!(!stash.planted);
        assert_eq!(stash.authtok, b"hunter2", "a failed write keeps the stash");

        stash.promote(b"hunter2", |_, _, _| Ok(())).unwrap();
        assert!(stash.planted);
        assert!(stash.authtok.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn suppress_marks_the_stash_handled_without_writing() {
        let mut stash = pending_token(b"hunter2");

        stash.suppress();
        assert!(stash.planted);
        assert!(
            stash.authtok.iter().all(|byte| *byte == 0),
            "a suppressed stash must hold no password bytes"
        );

        stash
            .promote(b"payload", |_, _, _| {
                panic!("a suppressed stash must never be written")
            })
            .unwrap();
    }

    #[test]
    fn plant_plan_only_skips_keyfile_vaults() {
        let cases: &[(&str, &[&str], PlantPlan)] = &[
            ("empty", &[], PlantPlan::Raw),
            ("keyfile", &["vault.key"], PlantPlan::Skip),
            (
                "keyfile-beats-password",
                &["vault.key", "vault.enc", "vault.salt", "vault.kdf"],
                PlantPlan::Skip,
            ),
            ("enc-only", &["vault.enc"], PlantPlan::Raw),
            ("salt-only", &["vault.salt"], PlantPlan::Raw),
            (
                "password-salt",
                &["vault.enc", "vault.salt"],
                PlantPlan::Derived,
            ),
            (
                "password-kdf",
                &["vault.enc", "vault.kdf"],
                PlantPlan::Derived,
            ),
            (
                "password-full",
                &["vault.enc", "vault.kdf", "vault.salt"],
                PlantPlan::Derived,
            ),
        ];
        for (tag, files, expected) in cases {
            let dir = test_directory(&format!("plant-{tag}"));
            std::fs::create_dir(&dir).unwrap();
            for file in *files {
                std::fs::write(dir.join(file), b"fixture").unwrap();
            }
            assert_eq!(plant_plan(&dir), *expected, "{tag}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A password-mode vault layout with a valid salt, for token payload
    /// tests: `derive_token_key_in` only checks that `vault.enc` exists and
    /// reads the KDF files, so a dummy ciphertext is enough here — the
    /// decrypt-verified roundtrip lives in the secret crate's tests.
    fn write_password_vault_layout(dir: &Path) {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("vault.enc"), b"fixture ciphertext").unwrap();
        std::fs::write(dir.join("vault.salt"), "c29tZXNhbHQ").unwrap();
        std::fs::set_permissions(
            dir.join("vault.salt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[test]
    fn token_payload_skips_keyfile_vaults() {
        let dir = test_directory("payload-keyfile");
        write_password_vault_layout(&dir);
        std::fs::write(dir.join("vault.key"), b"fixture key").unwrap();

        assert!(
            matches!(token_payload(Some(&dir), b"hunter2"), TokenPayload::Skip),
            "a keyfile-mode vault must not get a token"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_payload_derives_the_vault_key_for_password_vaults() {
        let dir = test_directory("payload-derived");
        write_password_vault_layout(&dir);

        let TokenPayload::Plant(content) = token_payload(Some(&dir), b"hunter2") else {
            panic!("a password-mode vault must plant a payload");
        };
        let prefix = atrium_portal_secret::PAM_TOKEN_KEY_PREFIX;
        assert!(content.starts_with(prefix.as_bytes()));
        assert_eq!(content.len(), prefix.len() + 64);
        assert_eq!(
            content.as_slice(),
            atrium_portal_secret::derive_token_key_in(&dir, b"hunter2")
                .expect("derive directly")
                .as_bytes(),
            "the planted payload is exactly the derived v2 token"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_payload_falls_back_to_the_raw_password() {
        // A non-UTF-8 password is outside the vault's domain: plant raw.
        let dir = test_directory("payload-non-utf8");
        write_password_vault_layout(&dir);
        let TokenPayload::Plant(content) = token_payload(Some(&dir), &[0xff, 0xfe]) else {
            panic!("a non-UTF-8 password must still plant a payload");
        };
        assert_eq!(content.as_slice(), &[0xff, 0xfe]);
        std::fs::remove_dir_all(&dir).unwrap();

        // No resolvable vault directory: today's behavior, plant raw.
        let TokenPayload::Plant(content) = token_payload(None, b"hunter2") else {
            panic!("an unresolved directory must plant the raw password");
        };
        assert_eq!(content.as_slice(), b"hunter2");

        // No vault at all: today's behavior, plant raw.
        let dir = test_directory("payload-no-vault");
        std::fs::create_dir(&dir).unwrap();
        let TokenPayload::Plant(content) = token_payload(Some(&dir), b"hunter2") else {
            panic!("a missing vault must plant the raw password");
        };
        assert_eq!(content.as_slice(), b"hunter2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setcred_plants_only_when_credentials_are_committed() {
        const ESTABLISH_CRED: c_int = 0x0002;
        const REINITIALIZE_CRED: c_int = 0x0008;
        const REFRESH_CRED: c_int = 0x0010;
        assert!(setcred_plants(0));
        assert!(setcred_plants(ESTABLISH_CRED));
        assert!(setcred_plants(REINITIALIZE_CRED));
        assert!(setcred_plants(REFRESH_CRED));
        assert!(!setcred_plants(PAM_DELETE_CRED));
        assert!(!setcred_plants(PAM_DELETE_CRED | ESTABLISH_CRED));
    }

    #[test]
    fn chauthtok_only_acts_on_the_update_phase() {
        const CHANGE_EXPIRED_AUTHTOK: c_int = 0x0020;
        assert!(chauthtok_updates(PAM_UPDATE_AUTHTOK));
        assert!(chauthtok_updates(
            PAM_UPDATE_AUTHTOK | CHANGE_EXPIRED_AUTHTOK
        ));
        assert!(!chauthtok_updates(PAM_PRELIM_CHECK));
        assert!(!chauthtok_updates(0));
        assert!(!chauthtok_updates(PAM_PRELIM_CHECK | PAM_UPDATE_AUTHTOK));
    }

    #[test]
    fn vault_dir_prefers_an_absolute_xdg_data_home() {
        assert_eq!(
            vault_dir(Some(b"/run/xdg-data"), b"/home/user").unwrap(),
            PathBuf::from("/run/xdg-data/aegis/secrets")
        );
    }

    #[test]
    fn vault_dir_falls_back_to_the_passwd_home() {
        let expected = PathBuf::from("/home/user/.local/share/aegis/secrets");
        assert_eq!(vault_dir(None, b"/home/user").unwrap(), expected);
        // A relative or empty XDG_DATA_HOME is ignored, like dirs::data_dir.
        assert_eq!(
            vault_dir(Some(b"relative/xdg"), b"/home/user").unwrap(),
            expected
        );
        assert_eq!(vault_dir(Some(b""), b"/home/user").unwrap(), expected);
    }

    #[test]
    fn vault_dir_rejects_relative_bases() {
        assert_eq!(vault_dir(None, b"relative/home"), None);
        assert_eq!(vault_dir(None, b""), None);
        assert_eq!(vault_dir(Some(b"relative/xdg"), b"relative/home"), None);
    }

    #[test]
    fn vault_plan_only_rekeys_password_mode_vaults() {
        let cases: &[(&str, &[&str], VaultPlan)] = &[
            ("empty", &[], VaultPlan::Skip),
            ("keyfile", &["vault.key"], VaultPlan::Skip),
            (
                "keyfile-beats-password",
                &["vault.key", "vault.enc", "vault.salt"],
                VaultPlan::Skip,
            ),
            ("salt-only", &["vault.salt"], VaultPlan::Skip),
            ("kdf-only", &["vault.kdf"], VaultPlan::Skip),
            ("enc-only", &["vault.enc"], VaultPlan::Skip),
            (
                "password-salt",
                &["vault.enc", "vault.salt"],
                VaultPlan::Rekey,
            ),
            (
                "password-kdf",
                &["vault.enc", "vault.kdf"],
                VaultPlan::Rekey,
            ),
            (
                "password-full",
                &["vault.enc", "vault.kdf", "vault.salt"],
                VaultPlan::Rekey,
            ),
        ];
        for (tag, files, expected) in cases {
            let dir = test_directory(tag);
            std::fs::create_dir(&dir).unwrap();
            for file in *files {
                std::fs::write(dir.join(file), b"fixture").unwrap();
            }
            assert_eq!(vault_plan(&dir), *expected, "{tag}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn rekey_vault_in_dir_is_a_noop_for_keyfile_and_missing_vaults() {
        // Keyfile mode: every file stays byte-identical.
        let dir = test_directory("rekey-keyfile");
        std::fs::create_dir(&dir).unwrap();
        for file in ["vault.key", "vault.enc", "vault.salt"] {
            std::fs::write(dir.join(file), b"fixture").unwrap();
        }
        rekey_vault_in_dir(&dir, b"old", b"new");
        for file in ["vault.key", "vault.enc", "vault.salt"] {
            assert_eq!(std::fs::read(dir.join(file)).unwrap(), b"fixture");
        }
        std::fs::remove_dir_all(&dir).unwrap();

        // A missing directory is never created.
        let dir = test_directory("rekey-missing");
        rekey_vault_in_dir(&dir, b"old", b"new");
        assert!(!dir.exists());
    }

    #[test]
    fn rekey_vault_in_dir_swallows_failures_and_touches_nothing() {
        // A password-mode layout whose ciphertext does not decrypt: the
        // re-key error is swallowed before any file is modified.
        let dir = test_directory("rekey-undecryptable");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("vault.salt"), "c29tZXNhbHQ").unwrap();
        std::fs::write(dir.join("vault.enc"), [0x5a_u8; 64]).unwrap();
        std::fs::set_permissions(
            dir.join("vault.enc"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let enc_before = std::fs::read(dir.join("vault.enc")).unwrap();
        let salt_before = std::fs::read(dir.join("vault.salt")).unwrap();

        rekey_vault_in_dir(&dir, b"old", b"new");
        // A non-UTF-8 password is outside the vault's domain and also skips.
        rekey_vault_in_dir(&dir, &[0xff, 0xfe], b"new");

        assert_eq!(std::fs::read(dir.join("vault.enc")).unwrap(), enc_before);
        assert_eq!(std::fs::read(dir.join("vault.salt")).unwrap(), salt_before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
