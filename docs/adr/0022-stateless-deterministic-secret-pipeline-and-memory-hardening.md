# ADR-0022: Stateless deterministic secret pipeline and zero-trace memory hardening

- Status: Accepted
- Date: 2026-09-08

## Context

[ADR-0020](0020-secret-vault-delegation-to-sigil.md) retired the portal's embedded
vault and delegated secret storage, unlock, and lock-state authority to the
`sigil` daemon, while [ADR-0021](0021-headless-portal-and-optics-retirement.md)
established a 100% headless daemon.

However, several architectural ambiguities and engineering gaps remained across
the Secret boundary:

1. **Stateful Database Paradigm vs. Stateless Mathematical Pipeline**:
   Traditional Linux desktop keyrings (e.g. GNOME Keyring / `org.freedesktop.secrets`)
   treat secrets as mutable, stateful database records ($O(N)$ growth). Every
   installed application triggers database writes, file locking, and schema
   maintenance, while application uninstallation leaves orphaned keys indefinitely.
   By contrast, the XDG Desktop Portal Secret specification (`RetrieveSecret`)
   only requires serving a stable, mutually isolated master secret to the calling
   application so the application can encrypt its own local sandbox state.
2. **Re-Keying Disaster Risk in Naive KDF**:
   If a deterministic KDF derives directly from a password-derived key (e.g.
   $\text{Argon2id}(\text{password})$), any user login password change alters
   the root key, catastrophically invalidating every sandboxed application's
   local encrypted databases (Chrome passwords, auth cookies, tokens).
3. **Transient Memory Hygiene (Zeroization)**:
   While secrets are conveyed over caller-supplied file descriptors (pipes)
   rather than D-Bus method returns, the portal relay path previously received
   and wrote secrets via unscrubbed heap buffers (`Vec<u8>`). Plain heap
   allocations leave raw secret material in process memory until overwritten by
   subsequent allocations, exposing secrets to memory scanners or core dumps.
4. **Stale Documentation and Residual Baggage**:
   Residual crate documentation (`atrium-portal-secret/README.md`) continued to
   claim ownership of "the at-rest vault, per-application HKDF key derivation,
   and single-flight unlock coordinator", contradicting the post-ADR-0020
   architecture.

## Decision

The Secret portal adopts an uncompromised, 100% stateless, mathematically
deterministic key pipeline backed by a two-tier envelope root and zero-trace
memory hardening.

### 1. Radical Separation of Authority

- **Portal Backend (`xdg-desktop-portal-atrium`)**:
  - Operates as a **zero-state identity gateway and pipe relay**.
  - Owns the D-Bus `org.freedesktop.impl.portal.Secret` v1 projection.
  - Resolves and enforces the caller's authentic `app_id` via kernel credentials
    (`SO_PEERCRED`/cgroups).
  - Performs **zero disk I/O**, maintains **zero databases**, and performs **zero
    in-tree cryptographic vault operations**.
- **Cryptographic Sovereign (`sigil`)**:
  - Operates as the **sole authority for root persistence, authentication, and KDF**.
  - Manages PAM integration, logind session lock listeners, and user unlock prompts.
  - Derives application secrets on demand in memory and returns them over the native
    Unix domain socket (`$XDG_RUNTIME_DIR/sigil/native.sock`).

### 2. Two-Tier Envelope Root and Deterministic KDF

To eliminate both database corruption risks and password-rotation disasters:

1. **Physical Persistence ($O(1)$ Immutable Master Seed)**:
   - Sigil provisions an immutable 256-bit CSPRNG `MasterKey` (Master Seed) at
     vault creation. This root seed is permanently stable and never changes
     over the life of the installation.
   - User authentication credentials (PAM/Password) derive an ephemeral Key
     Encryption Key (KEK) via Argon2id.
   - The KEK seals the Master Seed into Slot 0 ($\text{AES-256-GCM}$).
   - **Password Rotation**: Changing user passwords unseals Slot 0 with the old
     KEK and reseals the exact same Master Seed with the new KEK. The underlying
     Master Seed remains unchanged; all derived application keys remain 100% stable.
2. **Pure Mathematical Derivation (RFC 5869 HKDF-SHA256)**:
   - Subkeys are derived deterministically on-the-fly:
     $$\text{AppSecret} = \text{HKDF-Expand}(\text{PRK}=\text{MasterSeed}, \text{info}, 32)$$
   - Neither Sigil nor Atrium persists per-application keys on disk.
   - Keys exist in memory only during active derivation and transfer, then are
     instantly discarded.
   - When an application is uninstalled, zero disk residue or orphan records
     remain on the system.

### 3. Zero-Trace Memory Hardening

All secret-handling paths in `atrium-portal-secret` enforce strict memory hygiene:

1. **Automatic Zeroization**:
   - The native IPC response and transit buffers are wrapped in `zeroize::Zeroizing<Vec<u8>>`.
   - All secret bytes delivered from Sigil are deterministically wiped from memory
     as soon as the buffer drops.
2. **Minimal Transient Lifetime**:
   - The secret bytes are written directly to the caller-supplied file descriptor
     and synced if regular file; the transit buffer is dropped immediately.
3. **Companion Hardening in Sigil**:
   - Sigil locks the master key memory using `mlock(2)` to prevent swapping to disk,
     marks memory pages with `MADV_DONTDUMP` to prevent inclusion in core dumps,
     and clears memory on system lock (`org.freedesktop.login1.Session.Lock`).

### 4. Purge of Legacy Cruft and Misleading Documentation

- All claims of vault ownership, at-rest persistence, and in-tree KDF in
  `crates/atrium-portal-secret/README.md` and related source files are purged.
- The portal backend explicitly declines to claim or emulate `org.freedesktop.secrets`.

## Alternatives

- **Stateful per-application Keyring table (GNOME Keyring approach)**:
  Rejected. Storing a persistent record for every sandboxed application creates
  file lock contention during login, risks database corruption on unexpected
  shutdowns, and leaves permanent orphaned records after uninstallation.
- **Direct derivation from user password**:
  Rejected. Binding derived application secrets directly to user password hashes
  destroys all sandboxed application data whenever the user updates their
  login password.
- **Unscrubbed heap allocations**:
  Rejected. Passing secrets in standard `Vec<u8>` buffers leaves plaintext
  sensitive material scattered in process heap memory indefinitely.

## Consequences

- The secret retrieval path requires zero disk reads or writes on both Atrium
  and Sigil during standard application execution.
- Application uninstallation leaves zero residual cryptographic artifacts.
- Modifying system passwords does not break sandboxed application encryption.
- Transient heap memory in the portal daemon is actively scrubbed after every
  `RetrieveSecret` call.
- The repository documentation accurately matches the headless, delegation-based
  reality of the codebase.