# ADR-0024: The protocol-32 projection (session-scoped virtual extend outputs)

- Status: Accepted
- Date: 2026-10-30

## Context

The Tessera compositor extended its IPC to protocol 32 (Tessera ADR-0186,
ADR-0187). The additions the Portal needs are the ones that let
`org.freedesktop.impl.portal.ScreenCast` offer a **virtual source**
(`SourceType::VIRTUAL`, portal value 4): an output that does not exist on any
physical connector, created per session and streamed through PipeWire like any
other source.

The additions are:

- `CreateVirtualOutput { spec: VirtualOutputSpec }`, answered by
  `VirtualOutputCreated { output: OutputInfo }`. The spec's `connector` is
  optional (the compositor allocates the lowest free `VIRTUAL-n`), and
  `persistent` defaults to `false`: a session-scoped output is reclaimed if the
  requesting connection dies, which is what a Portal session wants.
- `ResizeVirtualOutput { connector, mode, scale?, logical_origin? }`, answered
  by `VirtualOutputResized { output }`. This is how a sink-driven mode change
  (RTSP M3/M4 negotiation, a tablet rotating, an adaptive-bitrate renegotiation)
  is requested.
- `DestroyVirtualOutput { connector }`, answered by `VirtualOutputDestroyed`.
- `StreamOutputRenegotiate { stream_id, max_fps?, dmabuf? }`, answered by the
  existing `StreamOutputStarted`. It rebuilds the stream's backing target at
  the target's current geometry **in place**, preserving `stream_id`,
  `sequence`, and `dropped`. This replaces the stop-and-restart that a geometry
  change otherwise forces.
- `OutputInfo` gains `kind` (`internal` | `external` | `virtual`), omitted when
  `external`, so a projection negotiating below 32 reads the shape it always
  has.

Every addition is additive on the wire; none changes a shape protocol 24 peers
exchange. The projection's coordination rule still applies
([ADR-0011](0011-wallpaper-wire-reconciliation.md)): never project ahead of the
compositor, pin literal fixtures, and cover the additions with the independent
test server rather than any compositor code.

## Decision

1. The `atrium-portal-ipc` projection is re-baselined to
   `PROTOCOL_VERSION = 32`, negotiating down to 24. Version-gated features key
   off the negotiated version, as dmabuf slots do at 25 and the v29 surface
   does at 29. Upstream protocols 26–28, 30, and 31 remain deliberately
   unprojected.
2. The wire additions above are projected exactly, with literal fixtures
   produced by serializing the compositor's own schema types and pinned in both
   directions where meaningful — including that `kind` is absent for an
   external output and present as `"virtual"` for a virtual one, and that an
   auto-allocated `VirtualOutputSpec` omits `connector` and `logical_origin`.
3. The independent test server answers all four operations, mirroring the real
   dispatch's gate order (control, live lease, the explicit scope op, and the
   lock/VT gate) and delegating to handler methods that default to
   *unsupported*, so a test opts in explicitly.
4. The client exposes `create_virtual_output`, `resize_virtual_output`,
   `destroy_virtual_output`, and `renegotiate_output_stream`. A compositor that
   negotiated below 32 refuses the operations; the refusal surfaces as the
   reply's `Error`, and the projection must treat "no virtual source" as an
   honest capability gap rather than offering a source it cannot create.

## Alternatives

- **Project the additions only when the ScreenCast virtual source lands.**
  Rejected: the wire contract, its fixtures, and the test-server coverage are
  the coordination surface with the compositor; landing them first keeps the
  consumer change reviewable on its own, and ADR-0004 requires wire changes to
  arrive with literal fixtures and independent-server coverage.
- **Reuse `StreamOutputStop` + `StreamOutputStart` for a geometry change.**
  Rejected: it mints a new `stream_id` and resets `sequence` and `dropped`, so
  the PipeWire consumer must tear down and rebuild its node for what is a mode
  change. `StreamOutputRenegotiate` exists precisely to avoid that
  (Tessera ADR-0186 §5, `[INV-ARCH-77]`).
- **Model a virtual output as an extra flag on `OutputInfo` rather than a
  `kind`.** Rejected: `kind` is a closed set the compositor already owns; a
  boolean would have to grow a second boolean the moment an internal-panel
  distinction matters, which it already does.

## Consequences

- The Portal offers protocol 32 at the handshake and behaves identically to
  its previous self against protocol 24–31 compositors; the v32 surface
  activates only where negotiated.
- A Portal `ScreenCast` session can now create, resize, stream, and destroy a
  virtual extend output, and renegotiate its stream in place.
- `docs/reference/compatibility.md` records the new baseline; the README's
  protocol sentence is updated.
