# Cast Frame Path

The pixel path behind a running cast: what the backend forwards, where the
clock lives, and what each side owns. The wire contract itself is
[IPC Wire Protocol](ipc-wire-protocol.md); this page describes the runtime
behavior on top of it. Decision history lives in
[ADR-0005](../adr/0005-screencast-dmabuf-slot-protocol.md),
[ADR-0006](../adr/0006-shm-consumers-switch-to-readback-transport.md),
and [ADR-0016](../adr/0016-screencast-runtime-protocol-29.md).

## Ownership Split

| Concern | Owner |
|---------|-------|
| Frame rate, capture, scanout exclusion, damage | Tessera compositor |
| Transport selection, buffer pools, PipeWire pacing | This backend |
| Pixel format conversion (e.g. to NV12), frame duplication | Consumer |

The backend adds no clock of its own. It is a passive relay: each
compositor `StreamFrame` event replaces the pending frame and triggers one
PipeWire DRIVER cycle. A quiet screen therefore forwards few frames; a
consumer that needs a steady rate (as OBS does) repeats its last received
frame locally.

## Session Lifecycle

| Phase | Backend action |
|-------|----------------|
| `SelectSources` | Prompter offers the entire desktop, one connector, or a window; choice is cached per session |
| `Start` | Open scoped IPC (`StreamOutputStart`, `max_fps` 60), learn format/size, create the PipeWire stream |
| Streaming | Relay `StreamFrame` events; renew the lease at half TTL; keepalive every 16 ms only to reclaim consumer-held dmabuf slots |
| Geometry change | `StreamGeometryChanged` freezes the cast; backend restarts the stream at the new geometry |
| Stop / disconnect | `StreamOutputStop` or IPC disconnect; the compositor unregisters and returns to damage-driven pacing |

## Transport Selection

| Transport | When | Path |
|-----------|------|------|
| Zero-copy dmabuf | Consumer accepts the announced DRM format and modifier | Pool buffers bind compositor slots by index; no pixel copy |
| Sealed memfd | Any other consumer | Frame memfd is mapped and copied once into the backend's own 4-buffer pool; linear layouts only — tiled modifiers are rejected to the SHM path |

## Frame Metadata

Each frame carries sequence, per-frame damage in the capture's coordinate
space (conservative: forced frames and moved crop origins report one
full-frame rectangle), and the cumulative drop count before this frame, so
a consumer can compute per-frame drop deltas. Delivery is best-effort:
compositor-side lanes are bounded, and backpressured frames are counted and
dropped rather than queued.
