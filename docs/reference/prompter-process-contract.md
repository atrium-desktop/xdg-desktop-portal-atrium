# Prompter Process Contract

The `atrium-portal-prompter` binary renders all Portal-owned UI. This page
summarizes its two process contracts for lookup; the field-level truth is
`crates/atrium-portal-prompter/src/lib.rs` (one-shot prompts),
`crates/atrium-portal-prompter/src/notify.rs` (notification daemon), and
`crates/atrium-portal-prompter/src/main.rs` (binary framing).
[Portal UI Testing](../dev/portal-ui-testing.md) shows how to drive both
contracts by hand. Decision history lives in
[ADR-0004](../adr/0004-portal-ownership-and-runtime-ipc-boundary.md) and
[ADR-0008](../adr/0008-optics-prompter-rewrite.md).

## Standard Streams

Standard output is the protocol wire in both contracts; standard error is
the diagnostics channel. At startup, before any library code runs, the
prompter claims the wire (`Wire::acquire` in
`crates/atrium-portal-prompter/src/wire.rs`): fd 1 is duplicated into a
private descriptor that both protocol writers use, and fd 1 itself is
re-aliased onto fd 2 for the rest of the process lifetime. A library — or
a stray `println!` — that writes to stdout lands on the journal as a
diagnostic, not on the wire. The backend reads the pipe to EOF and parses
the whole buffer strictly; trailing bytes fail the request. See
[ADR-0014](../adr/0014-prompter-privatizes-standard-output.md).

## One-Shot Prompts

The backend spawns one prompter process per interactive request, writes
one JSON request to its standard input, and reads one JSON response from
its standard output. A request larger than 8 MiB
(`MAX_MESSAGE_BYTES`) is rejected.

Both directions are versioned envelopes: `PrompterRequest` carries
`version` plus a tagged `prompt`; `PrompterResponse` carries `version`
plus a tagged `result`. The version must equal
`PROCESS_CONTRACT_VERSION` (currently 6) exactly — a mismatched
backend/prompter pair refuses to interpret each other's fields — and the
envelopes deny unknown fields.

### Appearance Snapshot (version 6)

Every request carries an optional `appearance` object — the compositor's
desktop preferences projected by the backend from its settings store
(Tessera IPC `GetSettings`): `color_scheme` (`system`/`dark`/`light`),
`accent_color` (`null` or 8-bit RGB), `high_contrast`, and
`reduced_motion`. The prompter resolves the palette from it (an explicit
scheme beats the platform query; `system` defers to it), overrides the
accent and selection wash when the compositor publishes one, restyles for
high contrast, and passes reduced motion to the UI toolkit. A
black-transparent accent is rejected at validation. A backend without a
compositor snapshot still sends the all-defaults projection, which is
indistinguishable in effect from an absent one.

| `prompt.kind` | Request / response | Portal users |
|---------------|--------------------|--------------|
| `file_chooser` | `FileChooserRequest` / `FileChooserResponse` (`selected`, `cancelled`, `failed`) | FileChooser |
| `confirm` | `ConfirmRequest` / `ConfirmResponse` (`confirmed`, `cancelled`) | Account, Access, Background, Wallpaper preview |
| `secret` | `SecretRequest` / `SecretResponse` (`secret`, `cancelled`) | Secret vault unlock |
| `choose_app` | `ChooseAppRequest` / `ChooseAppResponse` (`selected`, `cancelled`) | AppChooser, OpenURI with `ask` |
| `choose_source` | `ChooseSourceRequest` / `ChooseSourceResponse` (`selected`, `cancelled`) | ScreenCast source selection |
| `launcher_edit` | `LauncherEditRequest` / `LauncherEditResponse` (`saved`, `cancelled`) | DynamicLauncher |

### Validation Rules

- Requests are validated before any dialog is shown: prompt text is
  non-empty, NUL-free, and capped at 16 KiB; choice lists have unique ids
  and consistent selections; paths are absolute and NUL-free; `SaveFiles`
  basenames are single path components; an app chooser offers 1–64
  candidates; a source chooser offers 1–16 options with unique
  NUL-free ids; a launcher name is capped at 1 KiB.
- Responses are validated against the exact request (`validate_for`)
  before they become portal results: selected paths are absolute and match
  the mode's cardinality, a returned filter was offered, choice answers
  match the offered controls in order, a chosen app was offered, a chosen
  source was offered and `remember` is set only when the checkbox was
  shown, and a non-editable launcher name comes back unchanged.
- Filesystem paths are byte arrays (`BytePath`), so non-UTF-8 names
  round-trip without loss.
- Secret values are redacted from `Debug` output and zeroized on drop.

## Notification Daemon

With `--notification-daemon` the process is long-lived instead of
one-shot. Notifications are asynchronous, so the daemon speaks
newline-delimited JSON on both pipes: `CommandFrame` (`v`, `cmd`) on
standard input, `EventFrame` (`v`, `event`) on standard output. Every
frame's `v` must equal `NOTIFY_STREAM_VERSION` (currently 2); a version
mismatch or an oversized line is rejected, never panicked on.

- Commands: `notify`, `close`, `set_appearance` (version 2; carries the
  same appearance snapshot as the one-shot request, pushed once after
  spawn and again whenever desktop preferences change), `shutdown`.
  Events: `action_invoked`, `closed`.
- One JSON line past 64 KiB (`MAX_NOTIFY_LINE_BYTES`) is rejected; both
  peers read through the same bounded reader so neither grows memory
  unboundedly.
- Decode-time field caps: app id and notification id 255 bytes, title
  1 KiB, body 4 KiB, action name 255 bytes, button label 256 bytes, at
  most 8 buttons. A card needs a non-empty title or body.
- Cards are keyed by `(app_id, id)`. The daemon holds at most 64 live
  cards; a new card past the cap evicts the oldest.
- `expire_hint` is the auto-dismiss timeout in seconds; `null` persists
  the card until the user or the application closes it. The priority-to-
  timeout mapping the backend derives is listed in the
  [Portal Support Reference](portal-support.md).
