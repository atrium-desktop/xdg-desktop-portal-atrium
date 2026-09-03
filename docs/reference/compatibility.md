# Compatibility Reference

Portal and Tessera use independent release sequences. The current Portal
line implements the required Tessera IPC wire subset inside the
Portal-owned `atrium-portal-ipc` crate. Compatibility is defined by the wire
protocol and verified Tessera protocol schemas; it is not a Cargo source
dependency.

| Portal line | Tessera runtime | IPC protocol | Tessera build dependency |
|-------------|---------------|--------------|------------------------|
| `v0.0.11`–`v0.0.18` | `v0.0.16`–`v0.0.21` (29); `v0.0.15` (25); `v0.0.11`–`v0.0.14` (24) | 29, negotiates down to 24 | None |
| `v0.0.10` | `v0.0.16`–`v0.0.21` (27); `v0.0.15` (25); `v0.0.11`–`v0.0.14` (24) | 25, negotiates down to 24 | None |
| `v0.0.9` | `v0.0.16`–`v0.0.18` (27); `v0.0.15` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.8` | `v0.0.16`–`v0.0.18` (27); `v0.0.15` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.7` | `v0.0.16`–`v0.0.17` (27); `v0.0.15` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.6` | `v0.0.15` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.5` | `v0.0.11`, `v0.0.12` | 24 | None |
| `v0.0.4` | `v0.0.11`, `v0.0.12` | 24 | None |
| `v0.0.3` | `v0.0.11`, `v0.0.12` | 24 | Exact `v0.0.11` tagged Git crates |
| `v0.0.2` | `v0.0.11` | 24 | Exact tagged Git crates |
| `v0.0.1` | `v0.0.9` | Release-specific | Exact tagged Git crates |

Portal `v0.0.11` and later build and test without a Tessera checkout. The
committed
`Cargo.lock` contains no package from the Tessera repository. A production
installation still needs a running Tessera compositor because Settings,
Screenshot, color and target selection, ScreenCast, and Wallpaper consume
compositor-owned resources.

Protocol 24 is verified against the `v0.0.11` and `v0.0.12` schemas. Portal `v0.0.6`
offers protocol 25 at the handshake and accepts a
downgrade to 24, so it keeps working with protocol-24 compositors using the
SHM transports while protocol-25 compositors additionally provide the
zero-copy dmabuf slot stream (see
[ADR-0005](../adr/0005-screencast-dmabuf-slot-protocol.md)). The current
workspace offers protocol 29 and negotiates down to 24; upstream protocols
26 (`CaptureWindow`), 27 (`LaunchApp`, `Focus.reveal`), and 28 are
deliberately not projected. The protocol-29 additions (output enumeration,
connector-addressed stream targets, stream cursor mode,
`StreamGeometryChanged`, output picking) activate only where negotiated
(see [ADR-0015](../adr/0015-protocol-29-projection.md)). Wallpaper uses the
compositor's path-based `SetWallpaper`
op, which the compositor has spoken since protocol 17 — before this
projection's floor — so it works against every listed Tessera release (see
[ADR-0011](../adr/0011-wallpaper-wire-reconciliation.md)). A future Tessera
release is compatible when it preserves protocol 24.

Only Settings, Screenshot, ScreenCast, and Wallpaper require compositor
IPC; every other served interface is Portal-owned. FileChooser, Account
confirmation, and Secret password input use the
versioned one-shot Portal prompter contract.
