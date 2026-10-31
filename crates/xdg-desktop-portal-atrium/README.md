# xdg-desktop-portal-atrium

The `xdg-desktop-portal-atrium` crate builds the private D-Bus backend
process. It owns interface registration, scoped Tessera IPC adapters, request
workers, and the ScreenCast assembly.

It draws no dialog. Per [ADR-0026](../../docs/adr/0026-prompter-process-retirement.md)
every interactive prompt belongs to the component that already renders it:

- FileChooser runs one `arca --chooser-prompt` child per request
  ([ADR-0027](../../docs/adr/0027-arca-chooser-contract-crate.md)); neither the
  backend nor the compositor implements a file browser.
- Consent, application choice, and source choice are compositor chrome
  (`PickConfirm`, `PickApp`, `PickTarget`).
- Notifications go to the compositor's own queue (`Notify`).

## Layout

```
src/
├── lib.rs          assembly: bus, interface registration, workers
├── main.rs         the process entry point
├── companion/      out-of-process calls (arca)
├── fsx/            atomic writes under an owner-verified directory
├── uri.rs          file:// rendering and parsing
├── ipc.rs          the compositor client both halves share
├── screencast/     the interface plus its session registry and restore tokens
└── (one file per served portal interface)
```

A module used by exactly one interface belongs in that interface's slice: the
ScreenCast session registry and restore-token store live under
`screencast/` because deleting ScreenCast must delete them.

## Related crates

| Crate | Role |
|---|---|
| `atrium-portal-ipc` | The compositor wire projection |
| `atrium-portal-cast` | The PipeWire producer stream (native/`unsafe` boundary) |
| `atrium-portal-apps` | Desktop Entry and MIME Apps resolution |
| `atrium-portal-request` | The portal `Request` lifecycle |
| `atrium-portal-secret` | The Secret portal over sigil's socket |
| `atrium-arca-contract` | The arca FileChooser stdio contract |

Every one of them is linked into this single process; none is deployed
separately.
