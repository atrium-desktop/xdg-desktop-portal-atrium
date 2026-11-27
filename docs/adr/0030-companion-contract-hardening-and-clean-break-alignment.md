# ADR-0030: Companion contract hardening and clean-break alignment

- Status: Accepted
- Date: 2026-11-25

## Context

An end-to-end audit of the three repositories on the desktop portal's
runtime boundary — `xdg-desktop-portal-atrium`, `arca` (FileChooser), and
`sigil` (Secret vault) — found drift and boundary gaps in the runtime
contracts established by ADR-0020, ADR-0023, and ADR-0027:

1. **Arca FileChooser wire drift.** ADR-0027 replaced the legacy v6
   multi-prompt envelope (`{"version":6,"prompt":{"kind":"file_chooser",…}}`)
   with flat contract v1 (`{"version":1,"request":…}` /
   `{"version":1,"response":…}`), while `arca` still spoke v6 and rejected
   the empty initial `Choice.selected == ""` that `atrium-arca-contract`
   and the freedesktop FileChooser specification permit.
2. **Two cap mismatches on the arca request path.** The backend measured
   the bare request JSON against its 1 MiB cap, but arca caps the complete
   stdin line — the envelope plus its trailing newline. A request within
   the envelope overhead of the cap passed backend validation and then
   failed inside arca as a generic code 2. The backend also wrote that
   request synchronously before its cancellation poll began, so a child
   that never read `stdin` blocked a write larger than the pipe buffer and
   `Request.Close` was never observed.
3. **Sigil unlock and endpoint gaps.** `atrium-portal-secret` assumed sigil
   prompts interactively when locked, hardcoded
   `$XDG_RUNTIME_DIR/sigil/native.sock` without honoring
   `SIGIL_SOCKET_PATH`, and did not decode `STATUS_DESYNCED (0x04)`. On the
   daemon side, sigil failed closed immediately on a locked
   `GetApplicationSecret` and admitted any same-UID caller to
   `atrium.portal.Secret/v1` without peer executable verification.
4. **Permissive framing and leftover dependencies.**
   `atrium-portal-secret` still depended on `serde` and `serde_json` after
   ADR-0023, and `native.rs` accepted trailing bytes on empty-body statuses
   and after TLV error strings.

## Decision

All three repositories align on strict, clean-break runtime contracts with
no backwards-compatibility shims and no build-graph coupling.

### Arca FileChooser contract v1

- Both sides speak `ARCA_CONTRACT_VERSION = 1` with
  `#[serde(deny_unknown_fields)]` flat envelopes.
- `atrium_arca_contract::MAX_REQUEST_BYTES` (1 MiB) caps the request line
  arca reads: envelope and trailing newline included. The backend encodes
  that exact line once, refuses it as `InvalidArgs` before enqueueing when
  it exceeds the cap, and writes the same bytes to the child.
- The request is written on a dedicated thread, so cancellation is
  observed even when the child never reads `stdin`; terminating the child
  breaks the pipe and ends the write.
- Once the child has exited or been terminated, the three pipe drainers get
  a 2 s grace to reach EOF and are then detached. A process arca left
  behind that inherited a pipe fails the request instead of parking the
  FileChooser worker; the process group is deliberately not killed, since
  it can contain applications the user launched from the chooser.
- `Choice.selected == ""` is accepted on both sides (arca normalizes it to
  `"false"` or the first option ID), and choice answers are validated in
  request order.
- arca owns `save_files` path synthesis and returns all
  `request.files.len()` final paths. The backend validates exact
  cardinality, and the unused `finish_paths` helper is removed from
  `atrium-arca-contract`.
- Termination sends `SIGTERM`, allows 250 ms, then escalates to `SIGKILL`
  and reaps the child. arca keeps the default `SIGTERM` disposition today,
  so it exits immediately either way; the grace window exists so a future
  handler can exit on its own terms without a contract change.

### Sigil `sigil-wire-v2` hardening

- `native.rs` honors `$SIGIL_SOCKET_PATH` before
  `$XDG_RUNTIME_DIR/sigil/native.sock` and fails closed when neither is
  set. Both must be absolute: the D-Bus-activated portal has no meaningful
  working directory, so a relative override is refused and a relative
  runtime dir is ignored. It never falls back to a shared path.
- `native.rs` reads the response in 50 ms `poll(2)` slices and checks
  `RequestTracker::was_closed` between them, so `Request.Close` closes the
  socket at once (sigil observes EOF and abandons its unlock prompt) and
  answers 1 instead of waiting out the 300 s liveness floor.
- `STATUS_DESYNCED` decodes to `NativeError::Desynced`, which the portal
  answers with code 2 and logs with `sigil recover` guidance.
- Every status has exactly one payload shape: a non-empty secret for
  `STATUS_SECRET`; no bytes for `STATUS_LOCKED`, `STATUS_DESYNCED`, and
  `STATUS_CANCELLED`; exactly one length-prefixed string for
  `STATUS_ACCESS_DENIED` and `STATUS_ERROR`. Any other shape fails closed.
- `serde` and `serde_json` are removed from `atrium-portal-secret`, and
  every workspace crate inherits `[workspace.lints]`.
- sigil runs single-flight interactive `sigil-prompter` unlock on a locked
  `GetApplicationSecret` (client-disconnect cancellation, headless
  fast-fail) and enforces `PortalPeerPolicy` executable verification on
  `atrium.portal.Secret/v1`, so a non-portal same-UID process cannot forge
  `app_id`.

### Minimum companion releases

The clean break makes the companions a hard runtime floor, recorded in the
[Compatibility Reference](../reference/compatibility.md): arca `v0.2.18`
and sigil `v1.4.3`.

## Alternatives

- **Dual-version support (v1 and v6 for arca, lenient framing for sigil).**
  Rejected: every peer is shipped by the same project, and a second accepted
  shape doubles the parser surface on a trust boundary for no deployed
  consumer.
- **A shared contract crate imported by arca and sigil.** Rejected by
  ADR-0020 and ADR-0027: a shared implementation lets a matching bug pass
  both sides' tests. Each side keeps its own decoder and literal fixtures.
- **Cancelling sigil reads with a shorter socket timeout.** Rejected: any
  timeout short enough to be responsive would abort a user who is still
  typing their password. Polling a cancellation predicate keeps the long
  liveness floor and still reacts to `Request.Close` within 50 ms.
- **A backend cap with headroom below arca's cap.** Rejected: a margin is
  a guess about envelope overhead that drifts silently. Measuring the exact
  bytes written makes both caps describe the same quantity.

## Consequences

- FileChooser and Secret runtime contracts agree byte-for-byte and
  semantically across `xdg-desktop-portal-atrium`, `arca`, and `sigil`.
- Malformed or padded `sigil-wire-v2` responses fail closed instead of
  being accepted.
- A Portal release built on this decision does not work with arca before
  `v0.2.18` or sigil before `v1.4.3`; packagers must ship them together.
- Removing `FileChooserRequest::finish_paths` is a breaking API change for
  any consumer of `atrium-arca-contract`.
- The Portal build graph stays independent of `tessera`, `arca`, and
  `sigil` checkouts.
