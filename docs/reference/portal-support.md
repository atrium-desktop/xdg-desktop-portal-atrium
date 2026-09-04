# Portal Support Reference

## Native Interfaces

| Backend interface | Contract level | Tessera behavior |
|-------------------|----------------|----------------|
| `org.freedesktop.impl.portal.Settings` | Version 1 | Compositor-owned appearance and input settings |
| `org.freedesktop.impl.portal.Screenshot` | Version 3 | Area target, color picking, and consent-checked legacy output capture |
| `org.freedesktop.impl.portal.ScreenCast` | Version 6 | Monitor and (protocol 29) window sources through a source chooser plus compositor consent; per-output selection on multi-output compositors; Hidden and (protocol 29) Embedded cursor modes; `persist_mode` 1–2 restore tokens for monitor selections ([ADR-0016](../adr/0016-screencast-runtime-protocol-29.md)); stable `pipewire-serial`, 60 fps ceiling, zero-copy dmabuf delivery over the protocol-25 [slot protocol](../adr/0005-screencast-dmabuf-slot-protocol.md) with a shared-memory fallback; output geometry changes renegotiate the live stream; per-frame damage rides `SPA_META_VideoDamage` when the consumer requests it |
| `org.freedesktop.impl.portal.Secret` | Version 1 | Stable per-application secret derived by the sigil daemon (ADR-0020); storage, unlock, and the logind session-lock boundary (ADR-0019) are sigil-owned; a locked or absent sigil daemon reports cancelled/error response codes |
| `org.freedesktop.impl.portal.Lockdown` | Current seven-property ABI | All properties are read-write and process-resident |
| `org.freedesktop.impl.portal.FileChooser` | Current backend ABI | Open, save, directory, and multiple-file flows through a one-shot optics (iris/lens) process |
| `org.freedesktop.impl.portal.Email` | Current backend ABI | `xdg-email` handoff, attachment URI validation, activation token forwarding |
| `org.freedesktop.impl.portal.Account` | Current backend ABI | Name and optional avatar after explicit Portal-owned confirmation |
| `org.freedesktop.impl.portal.Access` | Version 1 | Frontend-driven consent dialog through the one-shot prompter, honoring the supplied deny/grant labels |
| `org.freedesktop.impl.portal.AppChooser` | Version 4 | Portal-owned chooser over in-process desktop-file/mimeapps resolution; live `UpdateChoices` acknowledged but not rendered; a "Remember this choice" checkbox writes `mimeapps.list` defaults |
| `org.freedesktop.impl.portal.OpenURI` | Version 3 | In-process content-type (`globs2`) and default-app resolution; the `ask` flow reuses the chooser; `file://` targets only, other schemes resolve as `x-scheme-handler/*`; `Terminal=true` entries are refused; `writable`/`activation_token` ignored |
| `org.freedesktop.impl.portal.Background` | Version 1 | Consent prompt on every request (no permission-store persistence); autostart writes `$XDG_CONFIG_HOME/autostart/<app_id>.desktop` atomically, mode 0644 |
| `org.freedesktop.impl.portal.DynamicLauncher` | Version 1 | Portal-owned install-confirmation dialog with name editing (the icon is echoed verbatim, never edited); install tokens are never issued; Application and Webapp types |
| `org.freedesktop.impl.portal.Inhibit` | Version 3 | logind-backed idle/suspend inhibition in `block` mode; logout and user-switch are tracked no-ops; monitors get one `StateChanged` (Running), and `QueryEndResponse` is an acknowledged no-op |
| `org.freedesktop.impl.portal.Notification` | Version 2 | Portal-owned notification daemon window stacking cards (text and buttons only; icons, sounds, and action targets ignored); low/normal priority auto-dismisses after 5/10 seconds, high/urgent persists |
| `org.freedesktop.impl.portal.Wallpaper` | Version 1 | Local `file://` images up to 64 MiB, optional textual preview confirmation; the image is staged at `$XDG_RUNTIME_DIR/atrium-portal/wallpaper/current.<ext>` (directory 0700, file 0600, atomic replace, kept after a successful swap) and applied through the compositor's path-based `SetWallpaper` op, which every supported compositor speaks; `set-on` is validated but not forwarded (single compositor wallpaper) |
| `org.freedesktop.impl.portal.Print` | Version 3 | `PreparePrint` echoes settings and page setup with a fresh token; `Print` spools to a private temp file and submits to the default printer through the system `lp` client |

`FileChooser`, `Email`, and `Account` do not define a backend `version`
property. The backend does not add one. The Print backend serves version 3:
its impl interface XML defines no version property, so the property reports
the local frontend contract level it implements.

## Unserved Interfaces

Every interface the routing configuration names is served natively by
Tessera; the default route is `tessera` alone and no fallback backend is
installed or required. Interfaces with no backend in this stack —
Camera, RemoteDesktop, GlobalShortcuts, InputCapture, USB, Location, and
Documents — are not advertised, and the portal frontend fails requests for
them cleanly.

## Runtime Dependencies

| Component | Purpose |
|-----------|---------|
| Tessera IPC protocol 29 (negotiates down to 24) | Compositor settings, screenshot capture and selection, capture consent, ScreenCast frames, and wallpaper application |
| `xdg-desktop-portal` | Public portal frontend |
| Optics (flux, lens, iris) shared libraries | All prompter UI processes, from the tagged `ming2k/optics` release |
| PipeWire and WirePlumber | ScreenCast transport and routing |
| logind (`org.freedesktop.login1`) | Inhibit locks; without it Inhibit calls fail with a backend error |
| `lp` (CUPS client) | Print submission; without it `Print` answers with a backend error |
| `xdg-email` | Email handoff |
| sigil daemon | Secret storage: the at-rest vault, unlock prompting, PAM auto-unlock (`pam_sigil`), and the logind session-lock binding live in the sigil repository; its IPC socket must exist at `$XDG_RUNTIME_DIR/sigil/native.sock` |

The release gates exercise two production integration baselines:

| Baseline | Frontend | PipeWire | WirePlumber |
|----------|----------|----------|-------------|
| Ubuntu 24.04 | 1.18.4 | 1.0.5 | 0.4.17 |
| Current development | 1.20.4 | 1.6.4 | 0.5.14 |

The target session must run PipeWire 0.3 or newer with the SPA 0.2 ABI for
ScreenCast consumers. The Ubuntu baseline is tested with Rust 1.88, the
minimum supported Rust version. Compatible newer releases remain supported
through their stable ABIs.

See the [Compatibility Reference](compatibility.md) for the Tessera releases
whose wire schemas are verified by the current Portal line.

## Persistent State

The Secret vault is owned by the sigil daemon; see the sigil documentation
for its on-disk layout and procedures. This backend persists only:

ScreenCast `persist_mode` 1 restore tokens live in
`$XDG_DATA_HOME/atrium-portal/screencast-restore.json` (directory `0700`,
file `0600`, atomic replace). Each entry binds an opaque 128-bit token to
one application's stored monitor selection (whole desktop or one
connector) and cursor mode. Delete the file to revoke every persisted
selection. `persist_mode` 2 tokens are process-resident and never touch
the disk; they vanish when the owning application's bus connection
closes.

The production per-application derivation differs from the shared secret
returned by the pre-production `v0.0.1` implementation. The first production
upgrade preserved the vault but rotated the value returned to applications.
Data encrypted directly with the old portal value must be recreated.
