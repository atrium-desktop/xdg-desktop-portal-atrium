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

The Secret portal delegates the at-rest vault, unlock, and lock-state
authority to the sigil daemon (the desktop's `org.freedesktop.secrets`
provider; see [ADR-0020](docs/adr/0020-secret-vault-delegation-to-sigil.md)),
and this backend only projects the `org.freedesktop.impl.portal.Secret`
interface onto sigil's native IPC socket. The backend itself protects
per-request secret material in transit to the caller and isolates
per-application delivery. It is not a security boundary against processes
running as the same user: it assumes the session's same-uid processes are
not hostile. The subsections record the concrete postures and the known
limitations, with their source of truth.

### Vault at Rest

The vault is owned and enforced by the sigil daemon: at-rest encryption,
key derivation, unlock, and the logind session-lock binding are sigil's
responsibilities, maintained in the sigil repository. The portal never
reads vault files and holds no master key; a locked or absent sigil daemon
surfaces as the portal response codes (1 cancelled / 2 error), never as a
fallback implementation. Its IPC socket lives at
`$XDG_RUNTIME_DIR/sigil/native.sock` inside the user's runtime directory —
a same-uid artifact like everything else there.

Per-application secrets are derived by sigil on demand and delivered over
the runtime-directory socket; the portal forwards the received bytes to
the caller's file descriptor without persisting them. An application that
learns its own value cannot derive another application's value from it.

### Vault Unlock Prompting

sigil owns unlock prompting, including its PAM auto-unlock path
(`pam_sigil`). The former portal-side PAM module (`pam_atrium`), PAM token
files, and session-lock watcher are removed from this repository; no
unlock material transits the portal.

### Password Input and the IME

Typed passwords pass through the compositor's input method unmasked at the
Wayland text-input layer: iris enables text input per surface, so there is
no per-field content purpose marking the field as a password. The prompt
itself masks the rendered text and the IME preedit, but a compositor-side
input method observes the committed text. This limitation is documented in
`crates/atrium-portal-prompter/src/ui/edit.rs` and lifting it requires an
optics release.

### Process and Memory Boundaries

- The prompter accumulates a typed
  password in a fixed 256-byte heap buffer (`SecretBuffer`) that is
  `mlock`ed and `MADV_DONTDUMP`ed best-effort and never reallocated, so
  growth cannot smear partial passwords across freed pages; the secret
  response is `mlock`ed and dump-excluded during serialization, and the
  backend `mlock`s every prompter response while parsing it (best
  effort, warning only). Both
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
