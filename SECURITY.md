# Security Policy

## Supported Versions

The project is pre-1.0. Only the latest release receives security fixes;
older releases are not patched.

## Reporting a Vulnerability

Report vulnerabilities privately through the GitHub security advisories of
[aegis-shell/xdg-desktop-portal-atrium](https://github.com/aegis-shell/xdg-desktop-portal-atrium/security/advisories)
("Report a vulnerability"). Do not open a public issue for an unpatched
vulnerability. When a private advisory is unavailable, open a public issue
describing only the affected component, without exploit details.

Maintainers triage reports against the latest release and coordinate a fix
and disclosure with the reporter; the project is maintained on a
best-effort basis and commits to no response-time guarantee. Credit is
given in the advisory unless the reporter declines.

## Threat Model

The backend protects the Secret vault at rest and isolates per-application
secrets. It is not a security boundary against processes running as the
same user: it assumes the session's same-uid processes are not hostile.
The subsections record the concrete postures and the known limitations,
with their source of truth.

### Vault at Rest

The vault lives at `$XDG_DATA_HOME/aegis/secrets/` (`vault.enc`,
`vault.key`, `vault.kdf`, `vault.salt`). `vault.enc` is
XChaCha20-Poly1305 ciphertext under a 32-byte master key held as hex in
`vault.key` (key-file mode) or derived from the vault password with
Argon2id (password mode). Password mode reads its exact parameters from
the `vault.kdf` sidecar, which is authoritative when present; a bare
`vault.salt` implies the legacy crate-default parameters, and malformed
`vault.kdf` content fails closed. The directory is mode 0700, the key and
ciphertext are mode 0600, files are opened with `O_NOFOLLOW`, and writes
are atomic. Symlinks, unexpected owners, unsafe modes, oversized input,
orphan ciphertext, and malformed encryption are startup errors: the
daemon fails closed rather than recovering from a suspect vault
(`crates/atrium-portal-secret/src/vault.rs`).

Per-application secrets are HKDF-SHA256-derived from the master key on
demand and are never stored; an application that learns its own value
cannot derive another application's value from it.

### PAM Token Handoff

With the optional PAM module enabled, `pam_atrium.so` hands vault-unlock
material to the portal through `/run/user/<uid>/atrium-pam-token` (mode
0600) so the portal can unlock a password-mode vault without prompting.
The token is planted only once the login is confirmed: `authenticate`
stashes the password in PAM module data, and the first committing
`setcred` or `open_session` hook writes the file, so a failed login never
leaves a token behind. Stacks that only authenticate (some screen
lockers) plant no token and unlock through the Portal's prompt instead.

What the file holds depends on the account's vault. When a password-mode
vault exists, the module derives the Argon2id vault master key and plants
`aegis-key-v1:` plus the key as hex — the at-rest tmpfs secret narrows
from the reusable login password to the vault key, and a stolen token of
this form cannot authenticate as the user anywhere else. A keyfile-mode
vault unlocks itself from `vault.key`, so no token is planted at all;
other layouts keep the legacy raw-password plant. The daemon accepts both
formats, but the `aegis-key-v1:` prefix commits: malformed key material
fails closed rather than falling through to the password path
(`crates/atrium-pam/src/lib.rs`,
`crates/atrium-portal-secret/src/lib.rs`).

Either form remains secret material in a same-uid-readable file: until
the portal consumes it, any same-uid process can read it. Consumption is
single-shot — the portal validates the file (regular file, same uid,
exact 0600 mode, size cap), unlinks it before reading, and refuses reuse,
and authenticated decryption, not token secrecy, is the integrity
boundary on the daemon side. If the portal never runs, the token persists
for the session. This window is a known, accepted trade-off of the
auto-unlock design.

### Password Input and the IME

Typed passwords pass through the compositor's input method unmasked at the
Wayland text-input layer: iris enables text input per surface, so there is
no per-field content purpose marking the field as a password. The prompt
itself masks the rendered text and the IME preedit, but a compositor-side
input method observes the committed text. This limitation is documented in
`crates/atrium-portal-prompter/src/ui/edit.rs` and lifting it requires an
optics release.

### Process and Memory Boundaries

- The vault master key is heap-pinned, `mlock`ed, and marked
  `MADV_DONTDUMP` on a best-effort basis: an `mlock`/`madvise` failure
  (for example `RLIMIT_MEMLOCK`) logs a warning and never fails an
  unlock, and the key is zeroized and `munlock`ed on drop. The daemon
  holds hot secret material — PAM token bytes, the unlock-prompt
  password, and re-key working copies — in `LockedBytes`, an owning
  buffer protected the same way. The prompter accumulates a typed
  password in a fixed 256-byte heap buffer (`SecretBuffer`) that is
  `mlock`ed and `MADV_DONTDUMP`ed best-effort and never reallocated, so
  growth cannot smear partial passwords across freed pages; the secret
  response is `mlock`ed and dump-excluded during serialization, and the
  daemon `mlock`s the prompter's response buffer after reading it. Both
  binaries cap `RLIMIT_CORE` at zero at startup so no core dump file is
  written for them, and the `MADV_DONTDUMP` page markings keep key
  material out of any dump image, including a piped core handler. The
  processes deliberately stay dumpable: a non-dumpable process's
  `/proc/<pid>/exe` is unreadable, which would blind the compositor's
  kernel-verified identity check for built-in IPC scope claims.
  Honest residuals: per-frame transient strings in the prompter's input
  path also live in platform and IME buffers by design, and the stdout
  line writer holds bytes briefly during serialization — zeroing those
  would be theater — and every buffer not named above stays pageable and
  can still reach swap. Credential buffers are zeroized on drop, which
  bounds but does not eliminate exposure.
- The backend spawns one prompter process per interactive request, and the
  child inherits the backend's environment. The `ATRIUM_PORTAL_PROMPTER`
  environment variable overrides the prompter binary path, so any same-uid
  process that controls the backend's environment can substitute the
  prompt UI. Prompter responses are validated against the exact request
  before they become portal results, but this validates shape, not intent.
- Compositor capture payloads travel as sealed memfds; the portal trusts
  the compositor it negotiates with and applies the scoped
  `atrium-portal` capability set
  (`crates/atrium-portal-ipc/src/schema.rs`). Wallpaper images are not
  payloads: the portal stages them at
  `$XDG_RUNTIME_DIR/atrium-portal/wallpaper/` (directory 0700, file 0600,
  atomic replace, wiped at startup) and the compositor reads the staged
  path — a same-uid artifact like everything else in the runtime
  directory.
