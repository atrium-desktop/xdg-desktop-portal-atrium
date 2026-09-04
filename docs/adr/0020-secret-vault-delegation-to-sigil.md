# ADR-0020: Secret vault delegation to sigil

- Status: Accepted
- Date: 2026-09-03

## Context

The portal once embedded its own encrypted vault (Argon2id KDF,
XChaCha20-Poly1305 at-rest encryption, two-phase re-key, derived-key PAM
tokens, and a logind session-lock watcher). A separate repository, sigil,
now implements the full `org.freedesktop.secrets` service with its own
vault, its own PAM module (`pam_sigil`), and its own logind lock listener —
the same responsibilities, implemented twice.

Commit `48f7af4` replaced the embedded vault with path dependencies on the
sibling's crates, but that interim integration had three defects:

- It coupled the Portal build graph to a sibling checkout — the same
  boundary violation the Tessera rules forbid (see
  [ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md) and the
  cross-repository development guide).
- The sigil client crate is asynchronous (tokio), while the portal daemon
  is a blocking, thread-based process; the bridge panics at runtime.
- The portal-side vault machinery (`pam_atrium`, the session-lock watcher,
  the PAM token derivation) survived as dead code driving no-op stubs.

## Decision

The portal delegates secret storage, unlock, and lock-state authority to
the sigil daemon. The portal owns only the
`org.freedesktop.impl.portal.Secret` D-Bus projection.

- Integration is **runtime-only**: a Unix socket at
  `$XDG_RUNTIME_DIR/sigil/native.sock`, framed as a u32 big-endian length
  prefix followed by JSON, with externally tagged request and response
  enums. The Portal-owned blocking client lives in
  `atrium-portal-secret/src/native.rs`; no sigil crate is imported,
  mirroring the Tessera wire-projection boundary.
- Tests use literal wire fixtures and a Portal-owned fake sigil server;
  they never import sigil implementation code.
- The embedded vault, the `pam_atrium` PAM module, the session-lock
  watcher, and the PAM token derivation are removed from this repository.
  sigil owns `pam_sigil` and its logind lock listener, which continues to
  fulfil the ADR-0019 boundary: the vault lock state follows the session
  lock.
- The FileChooser prompt binary integration (`arca --chooser-prompt`)
  follows the same pattern: a runtime binary contract, not a source
  dependency.

This decision supersedes the portal-owned vault mechanics recorded in
[ADR-0009](0009-vault-kdf-persistence-and-password-lifecycle.md),
[ADR-0010](0010-pam-confirmed-planting-and-libpam-abi.md),
[ADR-0012](0012-derived-key-pam-tokens.md), and
[ADR-0013](0013-two-phase-vault-rekey.md).

## Alternatives

- **Keep the sigil client crate as a path dependency and bridge it with a
  tokio runtime.** Rejected: it couples the Portal build graph to a
  sibling checkout, introduces a second async runtime into a blocking
  daemon, and moves protocol authority into a crate the portal does not
  own.
- **Keep the embedded vault.** Rejected: two implementations of the same
  cryptographic surface double the audit and maintenance burden, and the
  desktop stack already runs sigil as the `org.freedesktop.secrets`
  provider.

## Consequences

- The portal binary has no compile-time dependency on sigil; a missing or
  locked sigil daemon surfaces at call time as portal response codes
  (1 cancelled / 2 error), never as a missing advertised interface.
- A sigil wire change requires updating the projection and its literal
  fixtures in the same change; wire compatibility is validated against a
  tagged sigil release, not inferred from package versions.
- The lock/unlock, re-key, and PAM-planting behaviors documented by the
  superseded ADRs are now maintained in the sigil repository.
- Deployments must run the sigil daemon for the Secret portal to serve
  secrets; see the production installation guide.
