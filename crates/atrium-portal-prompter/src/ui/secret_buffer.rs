//! A fixed-capacity, page-locked buffer for the secret prompt's password.
//!
//! A password accumulated in a growable `String` smears across the heap:
//! every growth reallocation copies the previous buffer and frees it
//! without zeroing, so partial passwords linger in freed pages, and the
//! live pages themselves are swappable. `SecretBuffer` instead holds the
//! secret in one fixed heap allocation that never reallocates, is
//! `mlock`'d against swapping on a best-effort basis (the same policy the
//! vault's master key follows), is marked `MADV_DONTDUMP` so its pages
//! stay out of any core dump image, and is zeroized on every clear path —
//! on drop the zeroing happens before the pages are `munlock`'d.
//!
//! The 256-byte cap is generous for the vault's UTF-8 password domain. It
//! is enforced on byte length without ever splitting a multi-byte
//! character: input that would not fit is ignored. A paste inserts the
//! char-boundary prefix that fits (the least surprising behavior, matching
//! `maxlength` fields); further typed characters simply do not append,
//! which the user sees as the bullet count stopping.

use zeroize::Zeroize;

use super::edit::EditBuffer;

/// The buffer's fixed capacity in bytes of UTF-8.
pub const CAPACITY: usize = 256;

// This crate does not depend on libc (unlike the daemon crate); declare
// the syscall entry points needed to pin the buffer against swapping and
// keep it out of core dumps, following main.rs's setrlimit precedent.
// Linux-only, like the iris/lens stack this binary links.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn mlock(addr: *const std::ffi::c_void, len: usize) -> std::ffi::c_int;
    fn munlock(addr: *const std::ffi::c_void, len: usize) -> std::ffi::c_int;
    fn madvise(addr: *mut std::ffi::c_void, len: usize, advice: std::ffi::c_int)
    -> std::ffi::c_int;
    fn sysconf(name: std::ffi::c_int) -> std::ffi::c_long;
}

/// Linux `MADV_DONTDUMP`: exclude the range's pages from core dumps.
#[cfg(target_os = "linux")]
const MADV_DONTDUMP: std::ffi::c_int = 16;

/// glibc `_SC_PAGESIZE`.
#[cfg(target_os = "linux")]
const _SC_PAGESIZE: std::ffi::c_int = 30;

/// Mark the pages covering a secret region `MADV_DONTDUMP` so its bytes
/// stay out of any core dump image, including a piped core handler, which
/// the process-wide `RLIMIT_CORE` cap alone cannot guarantee. Best effort
/// like the mlock policy: a failure is logged, never fatal. The flag dies
/// with the mapping, so it needs no undo.
#[cfg(target_os = "linux")]
fn mark_dontdump(what: &str, ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: sysconf is always safe to call. Page sizes are powers of two.
    let page = match unsafe { sysconf(_SC_PAGESIZE) } {
        size if size > 0 => size as usize,
        _ => 4096,
    };
    // madvise requires a page-aligned address, so round the range out to
    // its covering pages; over-excluding a neighbor's bytes on a shared
    // page is harmless. SAFETY: the rounded range covers the region the
    // caller guarantees is valid readable memory owned by this process.
    let start = ptr as usize & !(page - 1);
    let end = (ptr as usize + len + page - 1) & !(page - 1);
    if unsafe { madvise(start as *mut std::ffi::c_void, end - start, MADV_DONTDUMP) } != 0 {
        log::warn!(
            "prompter: could not mark {what} MADV_DONTDUMP: {}",
            std::io::Error::last_os_error()
        );
    }
}

pub struct SecretBuffer {
    // Heap-boxed so the mlock'd address stays valid when the buffer moves.
    bytes: Box<[u8; CAPACITY]>,
    /// Bytes used; `bytes[..len]` is always valid UTF-8 because only whole
    /// characters are ever inserted or removed.
    len: usize,
    locked: bool,
}

impl SecretBuffer {
    /// Allocate the zeroed buffer and pin its pages against swapping. The
    /// mlock is best effort: on failure (for example RLIMIT_MEMLOCK) the
    /// buffer still works, just pageable.
    pub fn new() -> Self {
        let mut buffer = Self {
            bytes: Box::new([0; CAPACITY]),
            len: 0,
            locked: false,
        };
        // SAFETY: the boxed array is a valid readable region owned by this
        // process; mlock only pins its pages against swapping. Failure is
        // non-fatal and reported below — an unlock must never fail over it.
        #[cfg(target_os = "linux")]
        {
            let result = unsafe {
                mlock(
                    buffer.bytes.as_ptr().cast::<std::ffi::c_void>(),
                    buffer.bytes.len(),
                )
            };
            if result == 0 {
                buffer.locked = true;
            } else {
                log::warn!(
                    "prompter: could not mlock the secret buffer: {}",
                    std::io::Error::last_os_error()
                );
            }
            // Independent of the mlock outcome: exclude the pages from any
            // core dump image.
            mark_dontdump(
                "the secret buffer",
                buffer.bytes.as_ptr(),
                buffer.bytes.len(),
            );
        }
        buffer
    }

    /// The current text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("the buffer keeps whole characters")
    }

    /// Insert at byte `index` (a char boundary) as much of `s` as fits,
    /// rounded down to a char boundary; returns the bytes inserted so the
    /// caret can advance by exactly that.
    fn insert_bytes(&mut self, index: usize, s: &str) -> usize {
        assert!(index <= self.len && self.as_str().is_char_boundary(index));
        let room = CAPACITY - self.len;
        let mut take = s.len().min(room);
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        if take == 0 {
            return 0;
        }
        self.bytes.copy_within(index..self.len, index + take);
        self.bytes[index..index + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        take
    }

    /// Remove the byte range (both ends on char boundaries), zeroing the
    /// vacated tail so deleted characters do not linger in the allocation.
    fn remove_bytes(&mut self, start: usize, end: usize) {
        assert!(start <= end && end <= self.len);
        assert!(self.as_str().is_char_boundary(start) && self.as_str().is_char_boundary(end));
        self.bytes.copy_within(end..self.len, start);
        let new_len = self.len - (end - start);
        self.bytes[new_len..self.len].zeroize();
        self.len = new_len;
    }

    /// Zero the whole allocation and reset the length. Every path that
    /// discards the secret (the submit handoff, drop) goes through here.
    pub fn clear(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }

    /// Test-only view of the full heap allocation, so tests can assert the
    /// unused region and cleared bytes stay zeroed.
    #[cfg(test)]
    pub(crate) fn raw_bytes(&self) -> &[u8; CAPACITY] {
        &self.bytes
    }
}

impl Default for SecretBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl EditBuffer for SecretBuffer {
    fn as_str(&self) -> &str {
        self.as_str()
    }

    fn insert_str(&mut self, index: usize, s: &str) -> usize {
        self.insert_bytes(index, s)
    }

    fn remove_range(&mut self, range: std::ops::Range<usize>) {
        self.remove_bytes(range.start, range.end);
    }
}

impl Drop for SecretBuffer {
    fn drop(&mut self) {
        self.clear();
        // SAFETY: the boxed array was successfully mlock'd in `new` and its
        // heap address has not changed since; it is zeroized above before
        // the pages are released.
        #[cfg(target_os = "linux")]
        if self.locked {
            unsafe {
                munlock(
                    self.bytes.as_ptr().cast::<std::ffi::c_void>(),
                    self.bytes.len(),
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = self.locked;
    }
}

/// A best-effort `mlock` guard for a byte region with a short, bounded
/// lifetime — the serialized `SecretResponse`'s password bytes. `None`
/// from [`PageLock::new`] means the region is empty, the platform has no
/// mlock, or the call failed; the caller proceeds without a lock in all
/// three cases.
pub struct PageLock {
    addr: *const std::ffi::c_void,
    len: usize,
}

impl PageLock {
    pub fn new(bytes: &[u8]) -> Option<PageLock> {
        if bytes.is_empty() {
            return None;
        }
        // SAFETY: `bytes` is a valid readable region owned by this process;
        // mlock only pins its pages against swapping. Failure (for example
        // RLIMIT_MEMLOCK) is non-fatal.
        #[cfg(target_os = "linux")]
        {
            let locked =
                unsafe { mlock(bytes.as_ptr().cast::<std::ffi::c_void>(), bytes.len()) } == 0;
            if locked {
                // Keep the guarded pages out of any core dump image too.
                // The flag deliberately survives the guard: the region's
                // contents are secret for the mapping's whole lifetime, not
                // just while the lock is held.
                mark_dontdump("the secret response", bytes.as_ptr(), bytes.len());
                return Some(PageLock {
                    addr: bytes.as_ptr().cast::<std::ffi::c_void>(),
                    len: bytes.len(),
                });
            }
            log::warn!(
                "prompter: could not mlock the secret response: {}",
                std::io::Error::last_os_error()
            );
        }
        None
    }
}

impl Drop for PageLock {
    fn drop(&mut self) {
        // SAFETY: the region was successfully mlock'd in `new`; its owner
        // keeps it alive and un-reallocated for the guard's whole lifetime
        // (the response String is only read while the guard is held).
        #[cfg(target_os = "linux")]
        unsafe {
            munlock(self.addr, self.len);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (self.addr, self.len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(bytes: usize) -> SecretBuffer {
        let mut buffer = SecretBuffer::new();
        assert_eq!(buffer.insert_bytes(0, &"a".repeat(bytes)), bytes);
        buffer
    }

    #[test]
    fn capacity_enforcement_never_splits_a_char() {
        // 253 bytes in, a 4-byte char would straddle the cap: rejected whole.
        let mut buffer = filled(253);
        assert_eq!(buffer.insert_bytes(253, "🦀"), 0);
        assert_eq!(buffer.as_str().len(), 253);
        // A 2-byte char still fits (255 <= 256); a second does not (257 > 256).
        assert_eq!(buffer.insert_bytes(253, "é"), 2);
        assert_eq!(buffer.insert_bytes(255, "é"), 0);
        // Landing exactly on the cap works, and then nothing more appends.
        let mut buffer = filled(252);
        assert_eq!(buffer.insert_bytes(252, "🦀"), 4);
        assert_eq!(buffer.as_str().len(), CAPACITY);
        assert_eq!(buffer.insert_bytes(CAPACITY, "a"), 0);
    }

    #[test]
    fn insert_in_the_middle_shifts_the_tail() {
        let mut buffer = SecretBuffer::new();
        assert_eq!(buffer.insert_bytes(0, "aé"), 3);
        assert_eq!(buffer.insert_bytes(1, "中"), 3);
        assert_eq!(buffer.as_str(), "a中é");
    }

    #[test]
    fn paste_overflow_keeps_only_the_char_boundary_prefix_that_fits() {
        let mut buffer = SecretBuffer::new();
        // 255 ASCII bytes + one 2-byte char = 257 bytes; the é does not fit.
        let paste = format!("{}é", "a".repeat(255));
        assert_eq!(buffer.insert_bytes(0, &paste), 255);
        assert_eq!(buffer.as_str().len(), 255);
        assert!(buffer.as_str().ends_with('a'));
    }

    #[test]
    fn edit_insert_advances_the_caret_by_what_actually_fit() {
        let mut buffer = filled(254);
        let mut caret = 254;
        // Room for the é (2 bytes) but not the x; the caret tracks the é.
        crate::ui::edit::insert(&mut buffer, &mut caret, "éx");
        assert_eq!(buffer.as_str().len(), CAPACITY);
        assert!(buffer.as_str().ends_with('é'));
        assert_eq!(caret, CAPACITY);
    }

    #[test]
    fn remove_range_zeroes_the_vacated_tail() {
        let mut buffer = SecretBuffer::new();
        buffer.insert_bytes(0, "sécret");
        buffer.remove_bytes(1, 3); // the é
        assert_eq!(buffer.as_str(), "scret");
        assert!(buffer.raw_bytes()[5..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn clear_zeroes_the_whole_allocation() {
        let mut buffer = SecretBuffer::new();
        buffer.insert_bytes(0, "hunter2");
        buffer.clear();
        assert_eq!(buffer.as_str(), "");
        assert!(buffer.raw_bytes().iter().all(|&byte| byte == 0));
    }
}
