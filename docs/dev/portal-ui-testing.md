# Portal UI Testing

Portal interactions surface in two places, and since ADR-0026 neither of them
is a dialog this repository draws:

- **Arca's file chooser** for `FileChooser` (the `arca --chooser-prompt`
  stdio contract, ADR-0027).
- **Compositor chrome** for every other prompt: consent, application choice,
  source choice, and notification cards.

Every routed interface is served by this repository (see the
[Portal Support Reference](../reference/portal-support.md)), so all portal UI
renders in one of those two places, and the portal's job in both cases is to
make the request and read the answer.

## UI Surfaces

| Portal call | UI shown | UI owner |
|-------------|----------|----------|
| `FileChooser.OpenFile` / `SaveFile` | File browser with filters, choices, and save-name entry | Arca |
| `Account.GetUserInformation` | Consent dialog | Compositor chrome (`PickConfirm`) |
| `Access.AccessDialog` | Consent dialog with the frontend's labels | Compositor chrome (`PickConfirm`) |
| `Background.RequestBackground` | Consent dialog naming the reason and autostart | Compositor chrome (`PickConfirm`) |
| `Wallpaper.SetWallpaperURI` with `show-preview=true` | Consent dialog naming the image file | Compositor chrome (`PickConfirm`) |
| `AppChooser.ChooseApplication`, `OpenURI` with `ask` | Application picker | Compositor chrome (`PickApp`) |
| `ScreenCast.SelectSources` | Source/target picker | Compositor chrome (`PickTarget`) |
| `Screenshot.Screenshot` with `interactive=true` | Region picker | Compositor chrome (`PickTarget`) |
| `Screenshot.PickColor` | Crosshair pixel picker | Compositor chrome (`PickTarget`) |
| `Notification.AddNotification` | Notification cards | Compositor queue (`Notify`) |
| Secret vault unlock | Masked password prompt | sigil (ADR-0020) |

`DynamicLauncher` is not served: its backend surface is an editable launcher
form, and no companion owns a form. See ADR-0026.

## Arca Contract Smoke Tests

Arca is a stdin/stdout contract process: write one versioned JSON request to
its standard input and it shows the real file browser; the response JSON
appears on standard output when you answer, press Escape, or close the window.
No bus or daemon setup is required. The `"version"` field must equal
`atrium_arca_contract::ARCA_CONTRACT_VERSION` (currently `1`);
`scripts/version-consistency.sh` checks that the payloads below stay in sync
with it.

A file chooser with a filter and multi-selection:

```bash
printf '%s' '{"version":1,"request":{"mode":"open_file","app_id":"dev.tessera.Test","title":"Open File","accept_label":null,"modal":false,"parent_window":null,"multiple":true,"current_folder":null,"current_name":null,"current_file":null,"filters":[{"label":"Images","rules":[{"kind":"glob","value":"*.png"}]}],"current_filter":null,"choices":[],"files":[]}}' \
  | arca --chooser-prompt; echo
```

A save dialog — the download-location prompt a browser raises before writing a
download — with a suggested file name:

```bash
printf '%s' '{"version":1,"request":{"mode":"save_file","app_id":"dev.tessera.Test","title":"Save Download","accept_label":null,"modal":false,"parent_window":null,"multiple":false,"current_folder":null,"current_name":"report.pdf","current_file":null,"filters":[],"current_filter":null,"choices":[],"files":[]}}' \
  | arca --chooser-prompt; echo
```

Request also accepts `open_directory` and `save_files` (the latter requires a
non-empty `files` list of suggested basenames), and any mode can embed
`choices` controls. The response is `selected`, `cancelled`, or `failed`.

## Compositor Chrome Tests

Chrome prompts render inside the compositor, so they need a live Tessera
session; there is no headless substitute in this repository. The Tessera
repository covers the rendering itself — these setups validate that the
*request path* reaches the chrome and that the answer flows back.

Start the daemon on a private bus to keep the session clean. The daemon finds
`arca` through `PATH`, a sibling of its own binary, or
`ATRIUM_FILE_CHOOSER_PROMPTER`; the compositor socket is
`$XDG_RUNTIME_DIR/tessera.sock`:

```bash
dbus-daemon --session --nofork --print-address=1 > /tmp/bus.addr &
export DBUS_SESSION_BUS_ADDRESS=$(cat /tmp/bus.addr)
ATRIUM_FILE_CHOOSER_PROMPTER=$PWD/target/release/arca \
  RUST_LOG=info ./target/debug/xdg-desktop-portal-atrium &
```

It must run inside the session under test, because every non-FileChooser
prompt is a compositor round-trip.

Every impl method takes its request handle as the first argument: the caller
chooses the object path, and reusing an active path is rejected. The public
frontend normally synthesizes these paths from `handle_token` options; a
direct impl call supplies them explicitly. Issue real portal calls and answer
the compositor chrome:

```bash
# Compositor consent: Account shares identity only after the user agrees.
gdbus call --session -d org.freedesktop.impl.portal.desktop.atrium \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Account.GetUserInformation \
  '/org/freedesktop/portal/desktop/request/gdbus/t2' \
  'dev.tessera.Test' '' "{'reason': <'smoke'>}"

# Compositor chrome: region picker and crosshair pixel picker.
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

The ScreenCast picker requires the session dance before the chrome appears at
`SelectSources`. The session handle is the second argument to both calls:

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

Each impl method returns its `(response, results)` tuple once the prompt
closes, so `gdbus` prints the outcome directly. To exercise cancellation,
call `Close` on the request handle from a second shell while the chrome is up.

## Headless End-to-End Tests

The integration suite under `crates/xdg-desktop-portal-atrium/tests/` spawns
the real daemon on a private D-Bus session and swaps the external components
for test doubles, so no display participates:

- the file chooser gets a pipe-compatible fake whose fixture directory records
  every request as `request-N.json`;
- every other prompt gets a fake compositor
  (`tests/common/fake_compositor.rs`) speaking the real scoped IPC protocol
  and recording what the daemon asked.

```bash
ATRIUM_PORTAL_REQUIRE_E2E=1 cargo nextest run -p xdg-desktop-portal-atrium
```

Capture one of the recorded `request-N.json` files to replay a realistic,
backend-generated request through the real UI in the smoke setup above.
