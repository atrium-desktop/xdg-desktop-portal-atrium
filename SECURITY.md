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

### Password Input

No repository binary prompts for a password. Secret unlock prompting is
sigil's (ADR-0020), and the portal scope is not granted the compositor's
`PromptSecret` op, so the backend cannot trigger a compositor-hosted secret
prompt either (Tessera ADR-0112). The IME limitation recorded by earlier
releases moved with the prompt code into the components that own it.

### Process and Memory Boundaries

- The backend caps `RLIMIT_CORE` at zero at startup so no core dump file is
  written for it, and keeps itself dumpable: a non-dumpable process's
  `/proc/<pid>/exe` is unreadable, which would blind the compositor's
  kernel-verified identity check for built-in IPC scope claims. The Secret
  adapter forwards sigil's bytes to the caller and persists nothing.
- The backend spawns one `arca --chooser-prompt` child per FileChooser
  request, and the child inherits the backend's environment.
  `ATRIUM_FILE_CHOOSER_PROMPTER` overrides the arca binary path, so any
  same-uid process that controls the backend's environment can substitute the
  file-chooser UI. Chooser responses are validated against the exact request
  before they become portal results (`validate_for`: absolute paths, the
  mode's cardinality, an offered filter, offered choice values), but this
  validates shape, not intent.
- Every other prompt is a compositor round-trip. The backend sends only the
  text the interface supplies (a title, a body, an affirmative label, or a
  list of desktop-file ids) and reads back the answer; the rendering process,
  its input handling, and its memory are the compositor's.
- Compositor capture payloads travel as sealed memfds; the portal trusts
  the compositor it negotiates with and applies the scoped
  `atrium-portal` capability set
  (`crates/atrium-portal-ipc/src/schema.rs`). Wallpaper images are not
  payloads: the portal stages them at
  `$XDG_RUNTIME_DIR/atrium-portal/wallpaper/` (directory 0700, file 0600,
  atomic replace, wiped at startup) and the compositor reads the staged
  path — a same-uid artifact like everything else in the runtime
  directory.
