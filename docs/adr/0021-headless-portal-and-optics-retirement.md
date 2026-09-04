# ADR-0021: Headless portal architecture, prompt delegation to companion components, and optics retirement

- Status: Accepted
- Date: 2026-09-04

## Context

The portal backend (`xdg-desktop-portal-atrium`) previously compiled a dedicated
GUI prompter binary (`atrium-portal-prompter`) using the optics UI stack
(`flux`, `lens`, `iris`). This architecture was adopted in
[ADR-0008](0008-optics-prompter-rewrite.md) to replace an older GTK4 prompter,
with subsequent additions in [ADR-0014](0014-prompter-privatizes-standard-output.md),
[ADR-0017](0017-file-chooser-image-preview.md), and
[ADR-0018](0018-compositor-appearance-and-adaptive-sizing.md).

Over time, dedicated companion components in the Atrium desktop suite emerged
to own each user-facing interactive surface:

1. **Secret Service (`sigil`)**: Implements secret storage, master key
   derivation, and authentication prompts via `sigil-prompter`. The portal
   delegated all vault mechanics to sigil in
   [ADR-0020](0020-secret-vault-delegation-to-sigil.md).
2. **File Manager (`arca`)**: Provides native file management and the
   interactive FileChooser prompt (`arca --chooser-prompt`) implementing the
   versioned stdin/stdout JSON contract.
3. **Wayland Compositor (`tessera`)**: Implements compositor-level interactive
   pickers and consent dialogs over `tessera-ipc`: target picking (`PickTarget`
   for outputs, windows, regions, pixels), application selection (`PickApp`),
   and yes/no consent confirmations (`PickConfirm`).

Retaining the in-tree optics UI prompter inside the portal repository carried
substantial maintenance costs:
- It coupled the portal build graph to optics C libraries (`libflux`, `liblens`,
  `libiris`) and Vulkan graphics toolchains, requiring Meson in CI and local
  `[patch]` overrides (`.cargo/optics-local.toml`) during cross-repo development.
- It duplicated file chooser and confirmation logic already present in `arca`
  and `tessera`.
- It prevented the portal daemon from operating as a lightweight, purely headless
  IPC/D-Bus bridge.

## Decision

The portal backend (`xdg-desktop-portal-atrium`) becomes a **100% headless daemon**.
All GUI rendering and optics dependencies are retired from this repository.

1. **Prompt Delegation**:
   - **FileChooser**: Handled by the companion file manager `arca` via the
     runtime binary contract (`arca --chooser-prompt`).
   - **Secret**: Handled by `sigil` via the Unix socket contract
     (`$XDG_RUNTIME_DIR/sigil/native.sock`).
   - **Compositor Prompts**: Target picking (`PickTarget`), consent confirmation
     (`PickConfirm`), and application selection (`PickApp`) are delegated to the
     `tessera` compositor over `atrium-portal-ipc`.
2. **Retirement of Optics**:
   - The GUI implementation in `atrium-portal-prompter/src/ui/` and the GUI
     entrypoint in `src/main.rs` are removed.
   - The optics dependencies (`flux`, `flux-sys`, `lens`, `lens-sys`, `iris`,
     `iris-sys`) and media decoders (`image`, `mime_guess`) are completely
     removed from the workspace.
   - The `.cargo/optics-local.toml` local patch and `scripts/optics-release-ref.sh`
     are removed.
   - The `atrium-portal-prompter` crate is preserved strictly as a lightweight,
     headless data-contract library (`atrium_portal_prompter`) defining the
     serde types (`FileChooserRequest`, `ConfirmRequest`, `PrompterResponse`,
     etc.) shared across IPC boundaries and test fixtures.
3. **Packaging & CI**:
   - Only the headless `xdg-desktop-portal-atrium` binary is built and installed.
   - CI no longer requires Meson, ninja, or native optics C builds.

This decision supersedes [ADR-0008](0008-optics-prompter-rewrite.md),
[ADR-0014](0014-prompter-privatizes-standard-output.md),
[ADR-0017](0017-file-chooser-image-preview.md), and
[ADR-0018](0018-compositor-appearance-and-adaptive-sizing.md).

## Alternatives

- **Keep optics prompter as a secondary fallback binary.** Rejected: maintaining
  the optics dependencies, Vulkan linking, and rpath scripts solely for a
  redundant fallback keeps the build graph heavyweight and complex.
- **Move the contract types into atrium-portal-runtime and delete the crate
  entirely.** Rejected: keeping `atrium-portal-prompter` as a headless protocol
  crate preserves backwards compatibility for existing crate imports, test
  fixtures, and the version consistency verification suite.

## Consequences

- The repository is 100% pure Rust and compiles in seconds with zero graphics or
  Vulkan dependencies.
- Cross-repository development no longer requires `.cargo/config.toml` or optics
  sibling worktrees.
- Deployments require `arca` for file choosing and `sigil` for secret access.
