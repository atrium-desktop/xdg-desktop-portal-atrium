# Portal UI Testing

Portal interactions surface in two places: **Portal-owned prompter
windows** (iris/lens dialogs hosted by this repository) and
**compositor-owned chrome pickers** (rendered by the running Tessera session
for requests that need compositor resources). Every routed interface is
served by this repository (see the
[Portal Support Reference](../reference/portal-support.md)), so all portal
UI renders in one of those two places.

## UI Surfaces

| Portal call | UI shown | UI owner |
|-------------|----------|----------|
| `FileChooser.OpenFile` / `SaveFile` | File browser with filters, choices, and save-name entry | Prompter |
| `Account.GetUserInformation` | Confirmation dialog | Prompter |
| `Access.AccessDialog` | Confirmation dialog with the frontend's labels | Prompter |
| `AppChooser.ChooseApplication`, `OpenURI` with `ask` | Application list with a "Remember this choice" checkbox | Prompter |
| `DynamicLauncher.PrepareInstall` | Launcher name editor | Prompter |
| `Background.RequestBackground` | Consent dialog naming the reason and autostart | Prompter |
| `Wallpaper.SetWallpaperURI` with `show-preview=true` | Confirmation dialog naming the image file | Prompter |
| `Notification.AddNotification` | Cards in the notification daemon's window | Prompter (daemon mode) |
| Secret vault unlock | Masked password prompt | Prompter |
| `Screenshot.Screenshot` with `interactive=true` | Region picker | Compositor chrome |
| `Screenshot.PickColor` | Crosshair pixel picker | Compositor chrome |
| `ScreenCast` `SelectSources` | Source picker and capture consent | Compositor chrome |

## Prompter Tests

The prompter runs in three setups, from the fastest iteration loop to the
full request path.

### Prerequisites

Build the prompter once per change:

```bash
cargo build -p atrium-portal-prompter
```

The binary resolves the optics shared libraries from the sibling meson
build tree when local optics mode is active (`.cargo/config.toml`, see the
repository `AGENTS.md`), or from the system installation otherwise.

### Direct Contract Smoke Tests

The prompter is a stdin/stdout contract process: write one versioned JSON
request to standard input and it shows the real lens window; the response
JSON appears on standard output when you answer, press Escape, or close
the window. No bus, daemon, or display server setup is required. The
`"version"` field must equal
`atrium_portal_prompter::PROCESS_CONTRACT_VERSION` (currently `6`);
`scripts/version-consistency.sh` checks that the payloads below stay in
sync with it.

A confirmation dialog:

```bash
printf '%s' '{"version":6,"prompt":{"kind":"confirm","request":{"title":"Smoke Test","body":"Lens UI works.","accept_label":"_Continue","modal":false,"parent_window":null}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

A secret prompt (masked editing: typing, Backspace, caret keys, Ctrl+V,
Enter to submit):

```bash
printf '%s' '{"version":6,"prompt":{"kind":"secret","request":{"title":"Unlock Keyring","reason":"dev.tessera.Test wants access."}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

A confirmation dialog rendered with a compositor appearance snapshot
(contract v6): an explicit light scheme, an accent override, high
contrast, and reduced motion —

```bash
printf '%s' '{"version":6,"prompt":{"kind":"confirm","request":{"title":"Smoke Test","body":"Light scheme with accent.","accept_label":"_Continue","modal":false,"parent_window":null}},"appearance":{"color_scheme":"light","accent_color":{"red":43,"green":101,"blue":232},"high_contrast":true,"reduced_motion":true}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

A file chooser with a filter and multi-selection:

```bash
printf '%s' '{"version":6,"prompt":{"kind":"file_chooser","request":{"mode":"open_file","app_id":"dev.tessera.Test","title":"Open File","accept_label":null,"modal":false,"parent_window":null,"multiple":true,"current_folder":null,"current_name":null,"current_file":null,"filters":[{"label":"Images","rules":[{"kind":"glob","value":"*.png"}]}],"current_filter":null,"choices":[],"files":[]}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

A save dialog — the download-location prompt a browser raises before
writing a download — with a suggested file name:

```bash
printf '%s' '{"version":6,"prompt":{"kind":"file_chooser","request":{"mode":"save_file","app_id":"dev.tessera.Test","title":"Save Download","accept_label":null,"modal":false,"parent_window":null,"multiple":false,"current_folder":null,"current_name":"report.pdf","current_file":null,"filters":[],"current_filter":null,"choices":[],"files":[]}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

An application chooser (the AppChooser/OpenURI surface) with the
remember checkbox:

```bash
printf '%s' '{"version":6,"prompt":{"kind":"choose_app","request":{"app_id":"dev.tessera.Test","title":"Open With","content_type":"text/plain","parent_window":null,"apps":[{"id":"org.foo.Editor.desktop","name":"Foo Editor","icon":null},{"id":"org.bar.Notes.desktop","name":"Bar Notes","icon":null}],"choices":[{"id":"remember","label":"Remember this choice","options":[],"selected":"false"}]}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

A launcher-name editor (the DynamicLauncher surface):

```bash
printf '%s' '{"version":6,"prompt":{"kind":"launcher_edit","request":{"app_id":"dev.tessera.Test","title":"Install Launcher","name":"Cool App","editable_name":true,"target":null,"icon_label":"cool-app","modal":false,"parent_window":null}}}' \
  | ./target/debug/atrium-portal-prompter; echo
```

The notification daemon is a long-lived stream process instead of a
one-shot dialog: start it with `--notification-daemon` and write
newline-delimited commands (the `atrium_portal_prompter::notify`
protocol) to its standard input:

```bash
printf '%s\n' '{"v":2,"cmd":{"kind":"notify","app_id":"dev.tessera.Test","id":"n1","title":"Build finished","body":"","priority":"normal","default_action":null,"buttons":[],"expire_hint":10}}' \
  | ./target/debug/atrium-portal-prompter --notification-daemon
```

File chooser requests also accept `open_directory` and `save_files` (the
latter requires a non-empty `files` list of suggested basenames), and
any mode can embed `choices` controls. Set
`RUST_LOG=debug` to trace failures; the dialog reports a `failed`
response instead of crashing when no Wayland display is available.

### UI Acceptance & Verification Checklist

When accepting UI updates for portal dialogs, verify the following:

#### 1. File Chooser (SaveFile / OpenFile / Directory)
- **Top Navigation Group**: Segmented `[ ← → ↑ ]` buttons handle history back/forward and parent directory navigation.
- **Breadcrumb Bar**:
  - Displays home `🏠` or drive glyph followed by clickable path segments separated by `/`.
  - Current folder is subtly highlighted without crowding container borders.
  - Pressing `Ctrl+L` or selecting "Type Path" enters location text input mode.
- **Top Actions**:
  - `[📁+ New Folder]` opens an inline folder creation input.
  - `[ ⋮ ]` menu displays "Show Hidden Files" (`Ctrl+H`), "Type Path" (`Ctrl+L`), "Reload" (`Ctrl+R`), and bookmark actions.
- **Sidebar (PLACES)**:
  - `PLACES` header is fixed at the top of the sidebar rail, neatly aligned to the left edge.
  - Standard folders (`Home`, `Desktop`, `Documents`, `Downloads`, `Music`, `Pictures`, `Videos`) use clean capitalized labels.
  - Active item is styled with a refined slate-blue container and bright text/icon.
  - The scrollbar applies only to overflowing items below the header.
- **Table / Directory Listing**:
  - Clean, unboxed header row (`Name ▾`, `Size`, `Modified`) with muted text color.
  - Folder icons are 18px blue glyphs, vertically aligned with item text.
  - Formatted file sizes (`12 KB`, `1.5 MB`, `—` for folders) and timestamps (`YYYY/MM/DD HH:MM`).
- **Footer**:
  - Format/filter dropdown on the left.
  - `Cancel` (secondary neutral) and `Save` / `Open` (primary accent blue) on the right.

#### 2. Confirm / Permission Dialog
- Clean window chrome with dialog title, body text, and prominent `[ Cancel ]` / `[ Continue ]` actions.
- Theme responsiveness: inherits dark or light palette and compositor accent override.

#### 3. Secret / Password Dialog
- Masked password input field with auto-focus.
- Return submits password; Escape cancels.

#### 4. Application Chooser
- Application grid/list with desktop app icons and titles.
- "Remember this choice" checkbox state changes.

#### 5. Notification Toast Daemon
- Spawns unobtrusive top/corner toasts with title, message body, action buttons, and automatic expiration timeout.

### Headless End-to-End Tests

The integration suite under `crates/xdg-desktop-portal-atrium/tests/`
spawns the real daemon on a private D-Bus session and swaps the prompter
for a pipe-compatible fake, so no display participates:

```bash
ATRIUM_PORTAL_REQUIRE_E2E=1 cargo test -p xdg-desktop-portal-atrium
```

The fake prompter records every request the daemon issues as
`request-N.json` in its fixture directory. Capture one of those files to
replay a realistic, backend-generated request through the real UI in the
direct setup above.

### Headless UI Interaction Tests

The prompter's `ui_tests` unit tests (e.g.
`ui::file_chooser::ui_tests`) run the dialog's real per-frame `build`
closure on a headless lens `Ui` with synthetic input — key presses,
modifier chords, and text commits — and assert the resulting state:
the listing table's cursor/selection/activation contract, typeahead,
Ctrl+Space multi-select, location-field Tab completion and navigation,
the pre-filled save name's caret, Escape cancellation, and the preview
pane's state machine (hide for non-images, request-and-await one decode
per file, cache revisit) with real PNG fixtures the tests generate:

```bash
cargo test -p atrium-portal-prompter
```

They need no Wayland display, compositor, or D-Bus session (the lens
text stack still discovers fonts through fontconfig), so they run in CI
with the rest of the unit tests. The headless `Ui` has no `flux`
device, so preview tests assert the decode pipeline (caps, downsampling,
premultiplication, format gating) and the pane's state transitions; the
texture upload itself is exercised by the direct setup above — set
`current_file` to an image and watch for the `preview texture …
uploaded` debug line.

### Synthetic Interaction Tests

To verify buttons and keyboard interaction in the real lens dialog (not
just rendering), drive it through the compositor's Agent Interaction
Domain (`tessera-mcp serve`, see the Tessera repository's `tessera-mcp`
reference): launch the prompter from the direct setup, transfer its
window into the domain with `interaction_domain_transfer_window`, then
cycle `interaction_domain_observe` → `interaction_domain_input` batches
and confirm with `interaction_domain_capture`. Input is window-local:
pointer actions take logical pixel coordinates relative to the window's
reported local extent, and `key_press` actions take Linux evdev codes
(single press+release only — modifier chords cannot be composed, so
prefer flows that do not need Ctrl/Shift). Each input call must carry the
single-use token from the immediately preceding observation; captures
invalidate observations asynchronously, so leave a beat between a capture
and the next input batch.

## Compositor Chrome Tests

Chrome pickers render inside the compositor, so they need a live Tessera
session; there is no headless substitute in this repository. The Tessera
repository covers the picker rendering itself with offscreen-canvas
tests — these setups validate that the *request path* reaches the chrome
and that the answer flows back.

### Full-Stack Manual Tests

Run the daemon on a private bus to keep the session clean, with
`ATRIUM_PORTAL_PROMPTER` pointing at a debug prompter (the daemon locates
the prompter through that variable, see `prompter.rs`):

```bash
dbus-daemon --session --nofork --print-address=1 > /tmp/bus.addr &
export DBUS_SESSION_BUS_ADDRESS=$(cat /tmp/bus.addr)
ATRIUM_PORTAL_PROMPTER=$PWD/target/debug/atrium-portal-prompter \
  RUST_LOG=info ./target/debug/xdg-desktop-portal-atrium &
```

The daemon still connects to the running compositor's IPC socket for
compositor-owned requests, so run this inside the session under test.

Every impl method takes its request handle as the first argument: the
caller chooses the object path, and reusing an active path is rejected.
The public frontend normally synthesizes these paths from `handle_token`
options; a direct impl call supplies them explicitly. Issue real portal
calls and interact with the window:

```bash
# Prompter: file browser and confirmation dialog
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.FileChooser.OpenFile \
  '/org/freedesktop/portal/desktop/request/gdbus/t1' \
  'dev.tessera.Test' '' 'Open File' {}

gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Account.GetUserInformation \
  '/org/freedesktop/portal/desktop/request/gdbus/t2' \
  'dev.tessera.Test' '' "{'reason': <'smoke'>}"
```

The save dialog — the download-location prompt — is `SaveFile` with a
suggested name:

```bash
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.FileChooser.SaveFile \
  '/org/freedesktop/portal/desktop/request/gdbus/t5' \
  'dev.tessera.Test' '' 'Save Download' {'current_name': <'report.pdf'>}
```

```bash
# Compositor chrome: region picker and crosshair pixel picker
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Screenshot.Screenshot \
  '/org/freedesktop/portal/desktop/request/gdbus/t3' \
  'dev.tessera.Test' '' "{'interactive': <true>}"

gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Screenshot.PickColor \
  '/org/freedesktop/portal/desktop/request/gdbus/t4' \
  'dev.tessera.Test' '' {}
```

The ScreenCast picker requires the session dance before the compositor
chrome appears at `SelectSources`. The session handle is the second
argument to both calls:

```bash
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.ScreenCast.CreateSession \
  '/org/freedesktop/portal/desktop/request/gdbus/s1' \
  '/org/freedesktop/portal/desktop/session/gdbus/s1' \
  'dev.tessera.Test' {}

gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.ScreenCast.SelectSources \
  '/org/freedesktop/portal/desktop/request/gdbus/s2' \
  '/org/freedesktop/portal/desktop/session/gdbus/s1' \
  'dev.tessera.Test' {}
```

Each impl method returns its `(response, results)` tuple once the dialog
closes, so `gdbus` prints the outcome directly. To exercise cancellation,
call `Close` on the request handle from a second shell while the dialog
is open:

```bash
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop/request/gdbus/t1 \
  -m org.freedesktop.impl.portal.Request.Close
```

## What Each Setup Covers

| Setup | Renders real UI | Exercises backend | Needs a display | Runs in CI |
|-------|-----------------|-------------------|-----------------|------------|
| Direct contract (prompter) | Yes | No | Yes | No |
| Headless e2e | No | Yes | No | Yes |
| Full-stack manual | Yes | Yes | Yes (live session) | No |
