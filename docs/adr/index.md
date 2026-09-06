# Architecture Decision Records

| ADR | Decision | Status |
|-----|----------|--------|
| [0001](0001-repository-and-compatibility-boundary.md) | Repository and compatibility boundary | Superseded by [0002](0002-resource-authority-and-file-chooser-process-boundary.md) |
| [0002](0002-resource-authority-and-file-chooser-process-boundary.md) | Resource authority and FileChooser process boundary | Superseded by [0003](0003-production-interface-boundary.md) |
| [0003](0003-production-interface-boundary.md) | Production interface and secret boundary | Superseded by [0004](0004-portal-ownership-and-runtime-ipc-boundary.md) |
| [0004](0004-portal-ownership-and-runtime-ipc-boundary.md) | Portal ownership and runtime IPC boundary | Accepted (interface boundary extended by [0007](0007-full-stack-interface-ownership.md)) |
| [0005](0005-screencast-dmabuf-slot-protocol.md) | ScreenCast dmabuf transport and the slot protocol | Accepted (fallback amended by [0006](0006-shm-consumers-switch-to-readback-transport.md)) |
| [0006](0006-shm-consumers-switch-to-readback-transport.md) | SHM consumers switch the compositor stream to the readback transport | Accepted |
| [0007](0007-full-stack-interface-ownership.md) | Full-stack interface ownership | Accepted (wallpaper design point superseded by [0011](0011-wallpaper-wire-reconciliation.md)) |
| [0008](0008-optics-prompter-rewrite.md) | Prompter dialogs on the optics (iris/lens) stack | Superseded by [0021](0021-headless-portal-and-optics-retirement.md) |
| [0009](0009-vault-kdf-persistence-and-password-lifecycle.md) | Vault KDF persistence and the password lifecycle | Superseded by [0020](0020-secret-vault-delegation-to-sigil.md) |
| [0010](0010-pam-confirmed-planting-and-libpam-abi.md) | PAM confirmed planting, vault re-key, and the libpam C ABI | Superseded by [0020](0020-secret-vault-delegation-to-sigil.md) |
| [0011](0011-wallpaper-wire-reconciliation.md) | Wallpaper wire reconciliation and the protocol-25 baseline | Accepted |
| [0012](0012-derived-key-pam-tokens.md) | Derived-key PAM unlock tokens | Superseded by [0020](0020-secret-vault-delegation-to-sigil.md) |
| [0013](0013-two-phase-vault-rekey.md) | Crash-safe two-phase vault re-key | Superseded by [0020](0020-secret-vault-delegation-to-sigil.md) |
| [0014](0014-prompter-privatizes-standard-output.md) | The prompter privatizes its standard output | Superseded by [0021](0021-headless-portal-and-optics-retirement.md) |
| [0015](0015-protocol-29-projection.md) | The protocol-29 projection | Accepted |
| [0016](0016-screencast-runtime-protocol-29.md) | The ScreenCast runtime surface for protocol 29 | Accepted |
| [0017](0017-file-chooser-image-preview.md) | The FileChooser previews images in the prompter | Superseded by [0021](0021-headless-portal-and-optics-retirement.md) |
| [0018](0018-compositor-appearance-and-adaptive-sizing.md) | Compositor-owned appearance and adaptive dialog sizing | Superseded by [0021](0021-headless-portal-and-optics-retirement.md) |
| [0019](0019-vault-lock-follows-the-session-lock-boundary.md) | The vault lock state follows the session lock boundary | Accepted (implementation owned by sigil, see [0020](0020-secret-vault-delegation-to-sigil.md)) |
| [0020](0020-secret-vault-delegation-to-sigil.md) | Secret vault delegation to sigil | Accepted |
| [0021](0021-headless-portal-and-optics-retirement.md) | Headless portal architecture, prompt delegation to companion components, and optics retirement | Accepted |
| [0022](0022-stateless-deterministic-secret-pipeline-and-memory-hardening.md) | Stateless deterministic secret pipeline and zero-trace memory hardening | Accepted |
| [0023](0023-sigil-wire-v2-binary-protocol-and-zero-allocation-hardening.md) | sigil-wire-v2 binary protocol and zero-allocation hardening | Accepted |
