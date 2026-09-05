# atrium-portal-secret

Stateless `org.freedesktop.impl.portal.Secret` adapter linked into the
`xdg-desktop-portal-atrium` process.

Per [ADR-0020](../../docs/adr/0020-secret-vault-delegation-to-sigil.md),
[ADR-0021](../../docs/adr/0021-headless-portal-and-optics-retirement.md), and
[ADR-0022](../../docs/adr/0022-stateless-deterministic-secret-pipeline-and-memory-hardening.md):

- **Stateless Relay & D-Bus Projection**: The crate owns only the
  `org.freedesktop.impl.portal.Secret` v1 D-Bus interface projection. It maintains
  zero persistent state, owns no at-rest vault files, and performs zero in-tree
  cryptographic key derivations.
- **Delegation to Sigil**: Vault storage, unlock prompting, PAM integration,
  and deterministic HKDF key derivation are delegated at runtime to the companion
  `sigil` daemon via its native Unix domain socket (`$XDG_RUNTIME_DIR/sigil/native.sock`).
  No sigil crate is imported into the build graph.
- **Zero-Trace Memory Hardening**: All secret material retrieved from the native
  IPC socket is encapsulated in `zeroize::Zeroizing` buffers and deterministically
  scrubbed from memory immediately after being piped to the caller-supplied file descriptor.
- **No Secret Service Provider**: This crate does not implement or emulate the
  separate `org.freedesktop.secrets` API. Desktop keyring clients must use a complete
  Secret Service provider (such as `sigil`).
