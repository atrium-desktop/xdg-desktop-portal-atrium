# Changelog

All notable changes to this project are documented in this file.

> **Project rename.** The portal formerly known as
> **xdg-desktop-portal-aegis** was renamed to
> **xdg-desktop-portal-atrium**. The compositor it pairs with was renamed
> from **Aegis** to **Tessera**. Entries below use the new names; historical
> entries retain the original identifiers. The GitHub repository moved from
> `atrium-desktop/xdg-desktop-portal-aegis` to
> `atrium-desktop/xdg-desktop-portal-atrium`. Environment variables use the
> `ATRIUM_PORTAL_*` prefix (previously `AEGIS_PORTAL_*`). The D-Bus backend
> name is now `org.freedesktop.impl.portal.desktop.atrium`. On-disk vault
> paths (`aegis/secrets/`, `aegis-key-v1:`) are unchanged for compatibility.

## [Unreleased]

## [0.0.27] - 2026-08-30

### Changed

- Promoted Optics dependencies to `v0.0.32` and aligned UI components with the updated Optics/Lens box model and layout APIs.

## [0.0.26] - 2026-08-29

### Changed

- Promoted the Optics dependency set from `v0.0.28` to `v0.0.29` and
  adopted its shared typed Flux/Lens/Iris FFI seams. The prompter no longer
  re-types device or image handles across independently generated bindings.
- CI now runs unit and integration tests through nextest, retaining a
  separate documentation-test pass.

## [0.0.25] - 2026-08-25

### Changed

- Promoted Optics dependencies from `v0.0.27` to `v0.0.28`: the layered
  backdrop material compositor and table scroll row hit-test repair in lens.

### Added

- **GTK bookmarks and sidebar places in the file chooser.** The file chooser now loads and parses bookmarks from `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, categorizes sidebar places into standard and pinned sections, and renders compact sidebar rows.

## [0.0.24] - 2026-08-24

### Changed

- **The secret vault master key is now retained across session locks and suspend.** Master keys are bound to the user login session and protected with `mlock` and `MADV_DONTDUMP` in memory, preventing credential wipes and cookie database resets in sandboxed applications (e.g. Chromium / Flatpak Chrome) during screen locks.
- **PAM token watching is now event-driven via Linux inotify.** Replaced the 500ms polling loop with inotify watching `/run/user/<uid>`, eliminating latency and race conditions when sandboxed applications launch immediately upon startup.
- **Simplified vault commitment structures.** Removed dead legacy item structures in `aegis-portal-secret`, keeping the Poly1305 commitment validation pure while retaining 100% backward compatibility with on-disk collections.

## [0.0.23] - 2026-08-24

### Changed

- Promoted the Optics dependency set from `v0.0.26` to `v0.0.27`, matching the installed C libraries. v0.0.27 is binary-compatible (patch release on the `0.0` soname); it repairs the glyph-atlas page-0 image leak on `atlas_clear` (a 16 MiB dedicated `VkDeviceMemory` per clear, the leak that grew the prompter's RSS over long sessions with CJK input) and two lens ghost-snapshot defects (a ghost-only fade stalling once the base tree was clean; snapshot command counts corrupting the heap under OOM truncation).

- **The secret vault's lock state now follows the desktop's session lock boundary instead of a 15-minute idle timer** ([ADR-0019](docs/adr/0019-vault-lock-follows-the-session-lock-boundary.md)). The backend subscribes to logind's `Session.Lock`/`Session.Unlock` signals (and `PrepareForSleep`) and zeroizes the vault master key when the session locks or the system suspends, re-unlocking when it returns. The old watcher measured "no app read a secret", not "the user left": it locked mid-session while the user worked and held the key in RAM through a real screen lock, and on a keyfile-mode vault its unlock prompt could never succeed (there is no password to type).
- Keyfile-mode vaults re-unlock from `vault.key` when the session returns — silently, with the same fail-closed validation as startup — so a locked keyfile vault no longer produces a dead-end password prompt for waiting `RetrieveSecret` callers; the request is served from the keyfile.
- Password-mode vaults stay locked when the session returns and are re-opened by a PAM token or the masked prompt; the PAM-token watcher now runs for the daemon's lifetime (it previously exited permanently when the vault happened to be unlocked at startup, so tokens planted after a later lock were never consumed).
- A lock now dismisses unlock requests queued behind the prompt instead of answering them from a vault the user never re-authorized after the lock.

## [0.0.22] - 2026-08-23

### Fixed

- **Screencast frames carried a zeroed timestamp/sequence header on the Ubuntu 24.04 baseline.** Two independent defects, both reproduced against the documented PipeWire 1.0.5 / WirePlumber 0.4.17 baseline and both invisible on current PipeWire:
  - PipeWire 1.0.x zeroes the `type` field of every `struct spa_meta` in a pool buffer's meta array once the buffer has been through a consumer-return round trip. The portal's publish path resolved `SPA_META_Header`/`SPA_META_VideoDamage` through the live array, so the first frame carried a correct PTS/sequence and every reused buffer shipped a zeroed header — consumers (OBS, encoders) read sequence 0 and PTS 0 forever. The portal now snapshots each buffer's meta array in `add_buffer` (data pointer + size per type) and resolves metas through that snapshot, attaching Header/VideoDamage to a reused buffer exactly as to a fresh one.
  - The copy path's free-buffer pool reused buffers last-returned-first (a `Vec` tail pop). That rewrites the very buffer the consumer just gave back, collapsing the negotiated pool to an effective depth of one: a continuous stream visibly dropped every other frame on PipeWire 1.0.x, where the consumer observes a buffer only after the next write has already replaced its contents. The pool now dequeues oldest-first (`VecDeque`), restoring the full negotiated depth as a real pipeline.
  - The multi-frame cadence test's own consumer offered `SPA_PARAM_Meta` pods in an order PipeWire 1.0.x's ParamMeta merge drops the Header offer from the negotiated layout entirely; the test consumer now offers Header first (verified: the layout then carries `[Busy, Header, VideoDamage]` where the previous order yielded `[Busy, VideoDamage]`).

## [0.0.21] - 2026-08-23

### Fixed

- The prompter child's stderr is now teed — forwarded live to the daemon's stderr as before, with its tail retained for failure reporting — so a prompter the dynamic loader refuses (exit 127, the signature of an optics soname bump landing without a prompter relink) answers the portal request with the loader's own line naming the missing library instead of a bare "exited with exit status: 127 and no response".
- The notification daemon's death no longer wedges notifications permanently: the live-id map is cleared when the exited daemon is reaped, so a crashed daemon cannot fill the 256-id cap with ids whose cards died with it.
- A `Request.Close` racing the backend's reply can no longer leave a stale cancellation marker in the request tracker (which would both leak one string per race and misreport a later request reusing the handle as cancelled): markers are only recorded while the request is still being served.

### Changed

- The Optics dependency set moved from the v0.0.19 tag to v0.0.26, matching the installed C libraries; the prompter now links `lib{flux,iris,lens}.so.0.0` (the major.minor compatibility boundary introduced by optics v0.0.25) instead of the retired `lib*.so.0` sonames. v0.0.26 is binary-compatible (patch release on the `0.0` soname); it brings the canvas hot-path lock elision and the continuous Wayland scroll channel.

## [0.0.20] - 2026-08-20

### Added

- Prompt dialogs now follow the compositor's desktop preferences: the prompter request (contract v6) carries an appearance snapshot — colour scheme, accent colour, high contrast, reduced motion — projected from the backend's settings store, fixing dialogs that previously guessed the scheme through a GNOME-only gsettings query and never saw the compositor's accent or contrast (ADR-0018).
- Notification cards re-skin live when desktop preferences change: the notification stream grows to v2 with a `set_appearance` command the settings watcher pushes to the running daemon.
- Content-adaptive window sizing for the permission-style dialogs: confirm, secret, and launcher-editor windows are measured from their actual text through the same headless lens context that renders them, bounded by new design tokens, and pinned to their content size as a compositor minimum; the chooser and file-chooser windows gain explicit minima.
- Finder-style material layering in the FileChooser: the places rail and preview plate render as translucent bands with hairlines and popover/card radii over the opaque browsing plane, and the overwrite modal dims with the design system's scrim.
- High-contrast restyling: text, strokes, fields, hover washes, and the modal scrim deepen when the compositor requests contrast; the accent and selection wash follow a published accent colour; reduced motion reaches lens.

### Changed

- Process contract version 5 → 6 and notification stream version 1 → 2; backend and prompter must deploy together (the version checks fail closed on mismatch, as before).
- The style module's `dark: bool` atoms are retired in favour of palette-driven `*_for(&Palette)` forms; dialog state carries a resolved `ThemeInput` instead of a scheme bool.

### Fixed

- The notification daemon panicked (`Rc::try_unwrap` unreachable) on every graceful window close: the run wrapper's own `start`/`stop`/`frame_builder` closures — each holding a state clone — outlived the iris run instead of being dropped before the unwrap. Sequential window batches (every card group after the first) hit it on close; one-shot dialogs never noticed because the process exits first.

## [0.0.19] - 2026-08-20

### Added

- The FileChooser now previews the file under the listing cursor in a pane beside the directory listing: PNG, JPEG, GIF (first frame), WebP, and BMP decode off-thread under strict size caps, downsample to the pane, and render as a GPU texture through the device iris owns (ADR-0017). Non-previewable targets keep the full browsing width, and a preselected `current_file` now also takes the listing cursor so the preview shows on the first frame.
- `scripts/version-consistency.sh`: a CI gate that fails when `Cargo.toml`/`meson.build`/`CHANGELOG.md` versions, the README/compatibility IPC protocol numbers, or the smoke-test payload contract versions drift from their sources of truth in code.

### Changed

- The shared request-dispatch tail (register, enqueue with backpressure refusal, await, finish) moved into `aegis_portal_runtime::dispatch`, replacing twelve copies of the same block across the portal interfaces.
- `ScreenCast` capability reporting is served from a process-wide cache of the negotiated compositor protocol, so D-Bus property reads no longer open a compositor socket; the capability half of `SelectSources` validation now runs on the worker against the live protocol.
- `apps.rs` split into `apps/{desktop,mimeapps,exec}.rs` by spec area (desktop-entry scanning, association lists, `Exec` expansion).
- Workspace dependency table now pins every multi-crate dependency (`async-channel`, `argon2`, `chacha20poly1305`, `hkdf`, `rand`, `sha2`), matching the audit policy `deny.toml` documents.

### Fixed

- Documentation drift: `portal-ui-testing.md` smoke payloads used contract version 4 against version 5, `README.md` and the compatibility table reported protocol 25 against the implemented 29, `CHANGELOG.md` lost its 0.0.15 section and every post-0.0.9 link reference, `meson.build` was eight releases behind, and `cast-frame-path.md` was orphaned from the reference index.
- A hung prompter child is now verified to be reaped when the caller cancels (unit coverage for the prompter supervision: crash, invalid JSON, oversized response, cancellation).

## [0.0.18] - 2026-08-18

### Fixed

- Replaced unconditional 4ms PipeWire screencast keepalive polling with demand-driven execution, eliminating idle 250Hz CPU/power drain on static desktops.
- Added explicit vault locking API (`lock()`), query status (`is_unlocked()`), and 15-minute inactivity auto-lock watcher to `SecretService` for zeroizing master keys in memory.
- Fixed D-Bus mutex guard scope in native Secret portal delivery to eliminate deadlock hazards during fast-path unlocks.

## [0.0.17] - 2026-08-17

### Added

- Multi-modifier cross-negotiation test verifying that PipeWire correctly matches and fixates GPU hardware DRM modifiers with standard multi-option client modifier offers without falling back to SHM.
- Multi-frame continuous streaming and cadence test asserting reliable packet delivery, monotonic PTS timestamp tracking, and sequential buffer recycling across consecutive frames.
- Support for offering preferred modifier format pods alongside fallback plain SHM pods in the test harness, matching production PipeWire consumer behavior.

## [0.0.16] - 2026-08-17

### Fixed

- Populated the alternatives list in the `VideoModifier` `ChoiceEnum` pod for
  PipeWire stream format negotiation. PipeWire's `spa_pod_filter` requires
  non-empty alternatives to match consumer-offered DRM modifiers, resolving
  DmaBuf zero-copy negotiation failures that previously caused consumers like OBS
  to fall back to CPU shared-memory readback.

## [0.0.15] - 2026-08-17

### Fixed

- ScreenCast frame delivery pacing and stream stutter in consumers like OBS:
  - Negotiate and attach `SPA_META_Header` with monotonic `CLOCK_MONOTONIC`
    presentation timestamps (`pts`) and sequence numbers to every PipeWire
    buffer, ensuring consumer sync and preventing frame jitter.
  - Lower the PipeWire stream keepalive timer from 16.6ms to 4ms, ensuring
    consumer-returned buffers are reclaimed with sub-frame latency and
    preventing circular wait deadlocks under `StreamFlags::DRIVER` mode.

## [0.0.14] - 2026-08-17

### Added

- Added [Cast Frame Path](docs/reference/cast-frame-path.md) reference documentation describing the runtime pixel path behind a running cast (ownership split, session lifecycle, transport selection, and per-frame metadata).

## [0.0.13] - 2026-08-16

### Fixed

- Every compositor-mediated portal function being refused. Both binaries
  cleared their dumpable flag at startup, which makes `/proc/<pid>/exe`
  unreadable even to same-uid processes; the compositor's peer-identity
  check for built-in IPC scope claims (ADR-0128) then failed closed,
  refusing the `aegis-portal` scope and breaking ScreenCast consent (OBS
  recording), Screenshot, Wallpaper, and idle Inhibit. Core-dump
  protection now caps `RLIMIT_CORE` at zero instead, and every
  secret-holding buffer (`LockedBytes`, the vault master key,
  `SecretBuffer`, the serialization `PageLock`) is additionally marked
  `MADV_DONTDUMP` so key material stays out of any dump image, including
  a piped core handler. The processes stay dumpable, as the compositor's
  identity verification requires.

## [0.0.12] - 2026-08-16

### Added

- Flatpak-OBS-grade ScreenCast source selection, window capture,
  persistence, and geometry renegotiation on the protocol-29 surface
  (see [ADR-0016](docs/adr/0016-screencast-runtime-protocol-29.md)):
  - A new `choose_source` prompter kind (process contract 5) renders the
    capture source chooser: the whole desktop, one entry per connector on
    multi-output compositors, and a "Window…" entry when the client
    accepts window sources. A single-option list skips the dialog, so the
    common single-monitor flow keeps its one compositor consent prompt.
    Every fresh selection is still gated by a compositor `PickConfirm`
    naming the concrete target.
  - `AvailableSourceTypes`/`AvailableCursorModes` now follow the
    negotiated compositor protocol: 1/1 before protocol 29, 3/3 at 29+.
    Window sources and the Embedded cursor mode are accepted only where
    the compositor speaks them.
  - Window capture goes through the compositor's interactive toplevel
    pick and streams `StreamTarget::Window`; the Start result reports
    `source_type` window.
  - `persist_mode` 1 and 2 are honored for monitor selections with the
    chooser's remember tick: mode 1 persists opaque 128-bit restore
    tokens in `$XDG_DATA_HOME/aegis-portal/screencast-restore.json`
    (0700/0600, atomic), mode 2 keeps them in memory until the caller's
    bus name vanishes. A valid token restores the stored selection with
    no UI; window selections never yield tokens and report
    `persist_mode 0`.
  - A compositor `StreamGeometryChanged` event (output mode change,
    hotplug on a whole-desktop stream, window resize) now restarts the
    compositor stream with the same target, cursor mode, and dmabuf
    opt-in and re-offers the PipeWire format at the new geometry, so the
    consumer re-fixates instead of freezing. A mismatched restart fails
    the stream cleanly.
  - The compositor's per-frame damage rects are attached to published
    PipeWire buffers as `SPA_META_VideoDamage` metadata when the consumer
    requests it (the OBS direction); over-capacity damage collapses to a
    full-frame region.
  - The client's cursor mode now crosses to the compositor stream start
    on protocol 29.

- The `aegis-portal-ipc` projection is re-baselined to protocol 29
  (negotiating down to 24), projecting the compositor's new
  output-addressing surface (see
  [ADR-0015](docs/adr/0015-protocol-29-projection.md)):
  - `EnumerateOutputs`, reporting each output's connector, primary flag,
    and logical rectangle.
  - A connector-addressed `StreamTarget::Output` for per-output streams;
    the bare whole-desktop shape is unchanged, so older compositors keep
    working. Against a pre-29 peer the client fails a connector-named
    target closed instead of silently streaming the whole desktop.
  - A `cursor` mode on `StreamOutputStart` (`hidden`/`embedded`), sent
    only to protocol-29 peers.
  - The `StreamGeometryChanged` stream event, surfaced on the client's
    stream lane; after it the compositor sends no further frames until
    the stream is restarted. The cast loop's restart handling is part of
    the runtime surface above.
  - Output picking (`PickKind::Output`, with an optional connector in
    `PickResult::Output`).

### Fixed

- A pending compositor slot frame leaked its dmabuf slot when the next
  frame arrived before any PipeWire process cycle published it: the
  latest-frame overwrite silently dropped the old payload, and with no
  publish, no consumer return, and no teardown, nothing ever released the
  slot — permanently shrinking the compositor's slot ring and degrading
  the zero-copy stream to frame drops. Storing a frame now goes through
  one helper that releases a superseded, never-published slot before
  overwriting it, so the invariant cannot be bypassed; a regression test
  drives two slot frames without an intervening process cycle and
  observes the release at the compositor.

- ScreenCast recordings froze on one frame for seconds at a time whenever
  the PipeWire consumer held every pool buffer at once (an encoder's
  reorder lookahead, or any slow reader). Buffer reclaim — and with it the
  compositor's dmabuf slot releases — only ran inside the DRIVER stream's
  `process` callback, cycles only ran when a compositor frame arrived, and
  the compositor stops sending frames while all its capture slots are
  consumer-owned: a circular wait that wedged the stream until some
  external event renegotiated it. A keepalive timer now triggers cycles at
  the stream's frame cadence while Streaming, so reclaim and slot releases
  always run, and a frame the starved pool could not take stays pending
  and is retried on a later cycle instead of being dropped. The
  shared-memory pool offer also grew from two buffers to four to absorb
  encoder lookahead holds.

- Download prompts (FileChooser `SaveFile`) never delivered a result, so
  no download ever started: every prompter response failed the backend's
  strict parse with "trailing characters at line 2 column 1". libiris
  printed its first-frame diagnostic to standard output, and C stdio's
  full buffering on pipes flushed those bytes at process exit, after the
  JSON response — corrupting the backend↔prompter wire for every prompt
  kind, not just file choosers. Optics now sends all iris diagnostics to
  standard error (Wayland, Cocoa, and Win32 backends alike), and the
  prompter additionally claims standard output privately at startup —
  fd 1 is duplicated for the protocol response and re-aliased to standard
  error — so no library write can corrupt the wire again
  ([ADR-0014](docs/adr/0014-prompter-privatizes-standard-output.md)).

## [0.0.11] - 2026-08-15

### Fixed

- ScreenCast streams stalled to one frame every few seconds on a static
  desktop (frozen picture in OBS and other PipeWire consumers): the
  compositor only ever produced stream frames as a by-product of
  damage-driven presentation. Live streams now pace the compositor's main
  loop at the negotiated cadence (aegis v0.0.25; see its ADR-0052), so
  frames flow at the requested rate whether or not anything on screen
  changes. No portal-side transport change was needed; the cast bridge
  already republishes every compositor frame.

## [0.0.10] - 2026-08-13

### Added

- Nine new native portal interfaces, completing full-stack ownership of
  the routing table (see
  [ADR-0007](docs/adr/0007-full-stack-interface-ownership.md)):
  - `org.freedesktop.impl.portal.Access` v1, the generic consent dialog,
    rendered by the one-shot prompter with the frontend's labels.
  - `org.freedesktop.impl.portal.AppChooser` v4, a Portal-owned chooser
    over in-process freedesktop desktop-entry, `mimeapps.list`, and
    `globs2` resolution, with a "Remember this choice" checkbox that
    records the default application. Live `UpdateChoices` is acknowledged
    but not rendered by the one-shot dialog.
  - `org.freedesktop.impl.portal.OpenURI` v3, launching the resolved
    default application directly or through the chooser when asked;
    `file://` targets take their content type from the shared-mime-info
    glob databases, other schemes resolve as `x-scheme-handler/*`.
  - `org.freedesktop.impl.portal.Background` v1, consent-prompted on every
    request, writing login autostart entries under
    `$XDG_CONFIG_HOME/autostart/`.
  - `org.freedesktop.impl.portal.DynamicLauncher` v1, a Portal-owned
    install-confirmation dialog with name editing; install tokens are
    never issued.
  - `org.freedesktop.impl.portal.Inhibit` v3, taking logind idle and
    suspend locks in `block` mode; logout and user-switch inhibition are
    tracked no-ops, and monitor sessions report the Running state.
  - `org.freedesktop.impl.portal.Notification` v2, rendered by the
    prompter's new daemon mode: a versioned newline-delimited JSON stream
    drives a single window stacking notification cards, with priority-based
    auto-dismiss and action buttons.
  - `org.freedesktop.impl.portal.Wallpaper` v1, staging local images for
    the compositor's path-based `SetWallpaper` IPC operation, with a
    textual confirmation for preview requests.
  - `org.freedesktop.impl.portal.Print`, echoing settings from
    `PreparePrint` and submitting documents to the default printer through
    the system `lp` client.
- Password-mode vault lifecycle (see
  [ADR-0009](docs/adr/0009-vault-kdf-persistence-and-password-lifecycle.md)):
  the `vault.kdf` sidecar persists the exact Argon2id parameters and salt,
  authoritative when present, so an argon2-crate default change can no
  longer silently invalidate password-mode vaults. A legacy
  `vault.salt`-only vault migrates on its first successful unlock and keeps
  `vault.salt` as the downgrade mirror.
  `SecretService::create_password_vault` and
  `SecretService::change_password` create and re-key password-mode vaults,
  and the prompter-free `rekey_password_vault_in` entry serves the PAM
  password hook.
- The `pam_aegis.so` `password` hook re-keys the user's password-mode
  vault on login password changes (see
  [ADR-0010](docs/adr/0010-pam-confirmed-planting-and-libpam-abi.md)), so
  the vault password tracks the login password. Admin-initiated resets
  skip the vault, which then falls back to the Portal's unlock prompt.
- CONTRIBUTING.md and SECURITY.md: the contributor workflow and CI gates,
  vulnerability reporting, and the threat model.
- CI: a coverage artifact (`cargo llvm-cov`, lcov), pushes to the `dev`
  branch, and a pinned `cargo-deny` release replacing the unpinned action.

### Changed

- The prompter process contract is version 4, adding the confirmation
  dialog's deny label, the application chooser, and the launcher editor
  prompt kinds.
- The routing configuration names no other backend: every interface routes
  to `aegis` and the default is `aegis` alone. Interfaces without a
  backend in this stack (Camera, RemoteDesktop, GlobalShortcuts,
  InputCapture, USB, Location, Documents) stay unadvertised and fail
  cleanly at the frontend.
- The PAM module plants the vault-unlock token only once the login is
  confirmed: `authenticate` stashes the authtok in PAM module data, and
  the first committing `setcred` or `open_session` hook writes the token
  file, the later hook retrying when the runtime directory does not exist
  yet. Stacks that only authenticate — some screen lockers — no longer
  plant a token and fall back to the Portal's unlock prompt (see
  [ADR-0010](docs/adr/0010-pam-confirmed-planting-and-libpam-abi.md)).
- `aegis-pam` is relicensed from GPL-3.0-only to MIT: the GPL obligation
  came solely from the removed `pamsm` dependency, and libpam itself is
  BSD-licensed. A binary package containing `pam_aegis.so` no longer
  carries a GPL requirement.
- The daemon survives a panicking worker: every non-test mutex and rwlock
  acquisition in the daemon and the secret crate goes through
  `aegis_portal_runtime::sync`, which recovers the inner state from a
  poisoned lock with one warning instead of letting a re-panicked
  `.lock().unwrap()` cascade-kill the D-Bus-activated daemon.
- The `aegis-portal-ipc` projection is re-baselined to protocol 25
  (negotiating down to 24): the dmabuf slot stream is the newest
  projected feature, and upstream protocols 26 (`CaptureWindow`) and 27
  (`LaunchApp`, `Focus.reveal`) are deliberately not projected (see
  [ADR-0011](docs/adr/0011-wallpaper-wire-reconciliation.md)). The
  verified Aegis mapping is corrected: `v0.0.11`–`v0.0.14` speak
  protocol 24, `v0.0.15` speaks 25, and `v0.0.16`–`v0.0.21` speak 27.
- Wallpaper's `set-on` option is still validated (unknown values answer
  response 2) but no longer forwarded: the compositor has a single
  wallpaper concept and the wire op carries no placement.

### Fixed

- The Wallpaper portal never worked against a real compositor: the
  protocol-26 sealed-memfd `SetWallpaper` op was projected ahead of the
  compositor and shipped in no Aegis release, so every wallpaper request
  failed closed. The daemon now stages the image at
  `$XDG_RUNTIME_DIR/aegis-portal/wallpaper/current.<ext>` (directory 0700,
  file 0600, atomic replace, kept after a successful swap) and hands the
  staged path to the compositor's actual `SetWallpaper` op — spoken since
  protocol 17 — so wallpaper application works against every supported
  Aegis release (see
  [ADR-0011](docs/adr/0011-wallpaper-wire-reconciliation.md)).

### Removed

- The `xdg-desktop-portal-gtk` fallback dependency. Production
  installations no longer install or require another portal backend.
- The `pamsm` dependency. Its `pam_module!` macro types libpam's flags
  argument as a Rust enum lacking the chauthtok phase values (and combined
  flag values), so every password-hook call materialized an invalid enum
  discriminant — undefined behavior exactly where the phase must be read;
  the six PAM entry points are implemented against libpam's stable C ABI
  instead.

### Security

- A failed login no longer plants the PAM token file: the authtok stays in
  PAM module data behind a zeroizing cleanup until credentials are
  committed or a session opens, and `pam_end` scrubs it otherwise.
- The vault master key is heap-pinned and `mlock`ed on a best-effort
  basis (an `mlock` failure never fails an unlock; the key is zeroized and
  `munlock`ed on drop), and both binaries clear their dumpable flag
  (`PR_SET_DUMPABLE`) at startup so process memory stays out of core
  dumps.
- On accounts with a password-mode vault, the PAM unlock token now carries
  the Argon2id-derived vault master key (`aegis-key-v1:<hex>`) instead of
  the raw login password, narrowing the at-rest tmpfs secret from the
  reusable login password to the vault key (see
  [ADR-0012](docs/adr/0012-derived-key-pam-tokens.md)). Keyfile-mode
  vaults plant no token at all; legacy raw-password tokens stay accepted,
  and malformed key material fails closed rather than falling through to
  the password path.
- The vault re-key is two-phase and self-healing (see
  [ADR-0013](docs/adr/0013-two-phase-vault-rekey.md)): the new parameters
  are staged as `vault.kdf.next`/`vault.salt.next`, the ciphertext is
  swapped, and the pending pair is adopted; unlock tries every KDF
  candidate in order and reconciles, so an interrupted re-key can no
  longer leave the vault undecryptable.
- Secret memory hygiene: the daemon pins PAM-token bytes, the
  unlock-prompt password, and re-key working copies in mlock'd
  `LockedBytes`; the prompter accumulates typed passwords in a fixed
  256-byte mlock'd `SecretBuffer` that never reallocates, ending the
  realloc smear of partial passwords; and the secret response is mlock'd
  during serialization and after the daemon reads it.


## [0.0.9] - 2026-08-12

### Added

- Full IME support in the prompter's app-owned text fields (the location
  path, save-name, and secret surfaces). They now render the in-progress
  composition (preedit) inline — accent text underlined, caret inside at
  the composition's own cursor — apply the IME's
  `delete_surrounding_text` requests, and report the caret rectangle every
  focused frame through `lens_set_caret_rect`, so the input method's
  candidate window anchors at the caret instead of falling back to a
  default screen position. The secret field masks the composition too, so
  a preedit never echoes the password.

### Changed

- Rebuild the FileChooser's text fields and directory listing on new
  optics host-control APIs instead of app-owned implementations. The
  location and save-name fields are plain lens text fields; after a
  programmatic rewrite (Tab completion, a pre-filled name) the caret is
  moved through `lens_textfield_set_caret` (optics ADR-0064), with the
  setter applied in the field's own id scope at build time. The listing
  is now a virtualized `lens_table` (optics ADR-0066) with a keyboard
  cursor, per-cell folder/file icons, host-owned selection, and a
  per-directory scroll position (back/forward restores it); IME preedit
  and candidate-window anchoring on those fields now come from the
  toolkit itself. The dialog's headless interaction tests drive the real
  build path on a `Ui::headless` with synthetic input. These APIs ship
  in the tagged optics v0.0.14 release.
- Put the prompter dialogs on one design-token grid (`ui::style::metrics`):
  a 4 px spacing scale, paired control heights (text fields 36, buttons
  and toolbar buttons 32, listing and sidebar rows 32 minimum), a single
  corner radius, and type roles (body 14, dialog title 17, small 12.5 for
  hints, typeahead, and inline errors — the latter now in a danger color
  instead of muted gray). The location toolbar is pinned to the field
  height so swapping breadcrumbs for the path field no longer shifts the
  dialog; breadcrumb names truncate to a measured pixel budget rather
  than a character count; and keyboard navigation keeps the focused row
  inside the viewport with ensure-visible scrolling instead of a fixed
  pixel lead.

### Fixed

- ScreenCast no longer delivers a scrambled picture to consumers that
  cannot import the compositor's dmabuf modifier (reported with Flatpak
  OBS). The shared-memory fallback memory-mapped the slot descriptors and
  copied them linearly, which returns tile-swizzled bytes for the
  device-native tiled modifiers the compositor exports. Fixating the
  modifier-less format now restarts the compositor stream on the SHM
  readback transport underneath the live PipeWire connection, and the
  copy path never memory-maps a non-`DRM_FORMAT_MOD_LINEAR` descriptor;
  see [ADR-0006](docs/adr/0006-shm-consumers-switch-to-readback-transport.md).
- The `SPA_PARAM_BUFFERS` offer now advertises the layout delivery
  actually uses: the slot's stride and size for zero-copy dmabuf,
  tightly packed dimensions for the shared-memory copy path.

## [0.0.8] - 2026-08-11

### Added

- Rework the FileChooser dialog after GTK's file chooser. A places
  sidebar (Home, the configured XDG user dirs, and the filesystem root)
  offers one-click jumps; the breadcrumb bar renders as clickable chips
  with the current folder highlighted; back/forward buttons walk the
  navigation history; a create-folder action makes a directory and enters
  it. Ctrl+L, the pencil button, or typing `/`/`~` opens a type-a-path
  location field with Tab completion that accepts `~`, relative, and
  absolute paths — directories navigate, an existing file selects it, and
  in save mode the tail seeds the name field. The listing is fully
  keyboard-driven: arrows/Home/End move a cursor with selection
  following, Ctrl+Space toggles in multiple mode, typing selects by name,
  Enter activates (or accepts), Backspace and Alt+Up walk up, and saving
  over an existing file asks for overwrite confirmation first. The
  location and save-name fields are app-owned editing surfaces (the
  secret prompt's pattern) so the caret stays at the end across
  programmatic edits, with Left/Right/Home/End, Delete, and Ctrl+V
  editing.

### Changed

- Rename the prompter process contract's `selection` prompt kind to
  `file_chooser`, aligning the private wire name with the public
  `FileChooser` portal interface: `SelectionRequest`, `SelectionResponse`,
  and `SelectionMode` are now `FileChooserRequest`, `FileChooserResponse`,
  and `FileChooserMode`. The contract version rises from 2 to 3; a
  mismatched backend/prompter pair keeps refusing to interpret each
  other's fields.

### Fixed

- Keep the FileChooser footer visible: the root layout column now fills
  the window so the flexible listing absorbs any deficit — previously a
  long places list or directory listing pushed the Cancel/accept buttons
  below the window's bottom edge.
- Accept the compositor's dmabuf stream descriptors. They are anonymous
  inodes — never regular files — and their allocation may exceed the
  announced stride*height plane bytes, so the dmabuf receive path now
  validates only the size floor instead of demanding a regular file of
  exactly the announced length. The sealed-memfd path keeps its
  exact-length regular-file contract. This was the visible
  `capture descriptor length/type mismatch` ScreenCast failure.
- Buffer stream frames that race ahead of `StreamOutputStarted`. The
  compositor publishes the stream lane before it queues the reply, so an
  already-produced frame can legitimately precede it; the client now
  demultiplexes events from responses during stream start/stop and drains
  buffered frames from `next_stream_message` in arrival order instead of
  failing the start with an `unknown variant` parse error.

## [0.0.7] - 2026-08-10

### Changed

- Re-implement the prompter dialogs on the optics (iris/lens) stack
  instead of GTK4, styled after the aegis design language and following
  the system light/dark preference. FileChooser, Account confirmation,
  and Secret password requests keep the same versioned stdin/stdout
  process contract; prompts now map as independent windows because iris
  cannot yet import an exported `wayland:` parent handle. The build no
  longer requires GTK 4 development files; it requires the flux, lens,
  and iris C libraries from the tagged `ming2k/optics` release.

## [0.0.6] - 2026-08-10

### Added

- Zero-copy ScreenCast over the protocol-25 dmabuf slot transport. The
  compositor transfers a fixed set of dmabuf slot descriptors once at
  stream start; frames reference slots by index; the Portal binds each
  PipeWire pool buffer to a slot descriptor at registration and releases
  slots back to the compositor when the consumer returns them. Consumers
  that cannot import the stream's DRM modifier keep the shared-memory
  copy path. The handshake negotiates down to protocol 24 against older
  compositors, which keeps the previous transports working.

### Changed

- Raise the ScreenCast frame-rate ceiling from 30 to 60 fps and offer
  PipeWire consumers a 1–360 fps range (default 60) instead of a fixed
  30/1, so capture matches the compositor's actual cadence on 60 Hz
  outputs and each consumer paces against its own clock.
- Accept dmabuf-announced compositor streams instead of failing `Start`,
  and deliver their frames through a single mmap-and-copy into the PipeWire
  pool. Sealed-memfd frames take the same path, which removes the previous
  per-frame `Vec` allocation and copy. Per-frame dmabuf descriptors cannot
  be forwarded through PipeWire's fixed buffer pools; true zero-copy
  delivery is specified as the protocol-25 slot protocol in
  [ADR-0005](docs/adr/0005-screencast-dmabuf-slot-protocol.md).
- Log compositor-reported stream frame drops (`dropped` counter deltas)
  and Portal-side delivery drops for capture diagnostics.

## [0.0.5] - 2026-08-07

### Fixed

- Accept ScreenCast `SelectSources` source-type masks that offer window
  alongside monitor and serve the monitor subset, instead of rejecting the
  mixed offer. OBS's unified "Screen Capture (PipeWire)" source always sends
  `types = monitor|window` and aborted with a backend error, which made
  screen recording impossible.
- Fix ScreenCast frame pacing and stutter for PipeWire consumers such as
  Flatpak OBS. The stream now advertises the fixed `30/1` framerate it
  produces, pushes each compositor frame exactly once via
  `pw_stream_trigger_process`, and avoids re-copying stale frames into later
  process cycles.

## [0.0.4] - 2026-08-04

### Changed

- Remove all Aegis Git crates and sibling-checkout Cargo patches from the
  source and build graph. The Portal now owns a narrow, independent Aegis IPC
  protocol-24 client for compositor settings, capture, picking, and streams.
- Move Account consent and Secret vault password input from compositor IPC to
  the supervised, one-shot GTK4 Portal prompter used by FileChooser.
- Remove dormant native implementations for interfaces that are routed to the
  complete GTK backend.

### Added

- Add literal protocol fixtures and an independent minimal IPC server for
  media integration tests, so client and server tests do not share the
  implementation under test.

## [0.0.3] - 2026-08-04

### Fixed

- Resolve the exact Aegis `v0.0.11` IPC crates from the canonical
  `aegis-shell/aegis` repository so clean distribution builds do not depend
  on the retired `ming2k/aegis` remote.

## [0.0.2] - 2026-08-03

### Changed

- Moved FileChooser UI and filesystem enumeration out of Aegis compositor
  chrome into a one-shot GTK4 `aegis-portal-prompter` child. The backend now
  owns the complete v3 option/result mapping and kills the child on
  `Request.Close`; filesystem paths never cross compositor IPC.
- Added lossless Unix-path transport, typed glob/MIME filters,
  `current_filter`, `choices`, `modal`, Wayland parent handles, and complete
  `current_file`/`SaveFiles` handling to the FileChooser process contract.
- Advertise only the eight complete native backend interfaces and delegate
  Inhibit, AppChooser, Notification, DynamicLauncher, and Wallpaper to the
  GTK backend at the interface-routing boundary.
- Align Screenshot with version 3, ScreenCast with version 6 stable PipeWire
  serials, and Lockdown with all seven read-write properties.
- Derive stable Secret values per application instead of returning one
  shared value. This rotates values returned by the pre-production `v0.0.1`
  implementation; the encrypted vault remains intact.

### Security

- Remove the incomplete `org.freedesktop.secrets` compatibility service.
  A complete distribution keyring service must provide that separate API.
- Make vault/key persistence atomic and durable, reject symlinks, unsafe
  owners or modes, oversized files, corrupt vaults, and orphan ciphertext,
  and refuse partial daemon startup when Secret initialization fails.
- Harden PAM token delivery against environment-controlled runtime paths,
  symlink replacement, partial writes, unsafe directory modes, and
  thread-unsafe passwd lookup; zeroize credential buffers.
- Reject symlinked screenshot cache directories without changing the link
  target's permissions.
- Require explicit compositor consent for screen sharing, Account data, and
  legacy screenshots whose frontend permission was not already checked.

### Fixed

- Correct PipeWire buffer data-type masks and producer routing so a real
  WirePlumber/GStreamer consumer can negotiate and receive compositor frames.
- Prevent head-of-line blocking across Screenshot, ScreenCast, Account, and
  FileChooser requests; bound worker queues, UI/mailer/unlock tasks, total
  sessions, live casts, and user-controlled request payloads, and make
  session close cleanup race-safe.
- Parse Email attachment URIs without lossy Unix-path conversion and reap
  every spawned mailer child.
- Make real frontend tests use the backend-discovery override supported by
  both `xdg-desktop-portal` 1.18 and current releases.

### Added

- Add real public-frontend tests for Secret, Email attachment FD translation,
  and FileChooser, plus sealed-memfd screenshot and real PipeWire frame
  delivery tests.
- Add a Meson production installer with configurable `libexecdir`, portal
  metadata/routing installation, staged packaging, and optional PAM output.
- Gate the declared Rust 1.88 MSRV and both PAM-disabled and PAM-enabled
  package variants in CI.
- Add production installation, interface support, migration, and release
  documentation.

## [0.0.1] - 2026-08-02

### Added

- Established the independent `xdg-desktop-portal-aegis` workspace with the
  backend composition crate, shared request runtime, encrypted Secret
  component, optional PAM helper, activation metadata, CI, and supply-chain
  policy.
- Declared compatibility with Aegis `v0.0.9` through exact tagged Cargo
  dependencies.

[Unreleased]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/compare/v0.0.27...HEAD
[0.0.27]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.27
[0.0.26]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.26
[0.0.25]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.25
[0.0.24]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.24
[0.0.23]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.23
[0.0.22]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.22
[0.0.21]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.21
[0.0.20]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.20
[0.0.19]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.19
[0.0.18]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.18
[0.0.11]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.11
[0.0.12]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.12
[0.0.13]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.13
[0.0.14]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.14
[0.0.15]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.15
[0.0.16]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.16
[0.0.17]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.17
[0.0.18]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.18
[0.0.9]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.9
[0.0.8]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.8
[0.0.7]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.7
[0.0.6]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.6
[0.0.5]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.5
[0.0.4]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.4
[0.0.3]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.3
[0.0.2]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.2
[0.0.1]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.1
