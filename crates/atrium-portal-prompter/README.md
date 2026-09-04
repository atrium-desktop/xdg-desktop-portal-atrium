# atrium-portal-prompter

`atrium-portal-prompter` defines the versioned, out-of-process JSON data
contracts for interactive portal prompts (such as `FileChooserRequest`,
`ConfirmRequest`, `ChooseSourceRequest`, and `PrompterResponse`).

Per [ADR-0021](../../docs/adr/0021-headless-portal-and-optics-retirement.md), the
portal daemon is 100% headless. User-facing interactive prompts are delegated
to companion applications and services at runtime:

- **FileChooser**: Handled by `arca` (`arca --chooser-prompt`).
- **Secret**: Handled by `sigil` (`$XDG_RUNTIME_DIR/sigil/native.sock`).
- **Compositor prompts & pickers**: Handled by `tessera` via `atrium-portal-ipc`
  (`PickTarget`, `PickApp`, `PickConfirm`).

This crate contains no GUI code, no Vulkan dependencies, and no optics
dependencies. It provides strictly typed serde representations and serialization
validation across IPC boundaries and test fixtures.
