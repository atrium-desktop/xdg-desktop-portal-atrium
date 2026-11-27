# Compatibility Reference

Portal and Tessera use independent release sequences. The current Portal
line implements the required Tessera IPC wire subset inside the
Portal-owned `atrium-portal-ipc` crate. Compatibility is defined by the wire
protocol and verified Tessera protocol schemas; it is not a Cargo source
dependency.

| Portal line | Tessera runtime | IPC protocol | Tessera build dependency |
|-------------|---------------|--------------|------------------------|
| `v0.0.33`+ | `v0.0.78+` (32); `v0.0.16`–`v0.0.21` (29); `v0.0.15` (25); `v0.0.11`–`v0.0.14` (24) | 32, negotiates down to 24 | None |
| `v0.0.11`–`v0.0.32` | `v0.0.16`–`v0.0.21` (29); `v0.0.15` (25); `v0.0.11`–`v0.0.14` (24) | 29, negotiates down to 24 | None |
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
workspace offers protocol 32 and negotiates down to 24; upstream protocols
26 (`CaptureWindow`), 27 (`LaunchApp`, `Focus.reveal`), 28, 30, and 31 are
deliberately not projected. The protocol-29 additions (output enumeration,
connector-addressed stream targets, stream cursor mode,
`StreamGeometryChanged`, output picking) activate only where negotiated
(see [ADR-0015](../adr/0015-protocol-29-projection.md)); the protocol-32
additions (session-scoped virtual extend outputs, in-place stream
renegotiation, and `OutputInfo::kind`) activate only there too
(see [ADR-0024](../adr/0024-protocol-32-projection.md)). Wallpaper uses the
compositor's path-based `SetWallpaper`
op, which the compositor has spoken since protocol 17 — before this
projection's floor — so it works against every listed Tessera release (see
[ADR-0011](../adr/0011-wallpaper-wire-reconciliation.md)). A future Tessera
release is compatible when it preserves protocol 24.

Only Settings, Screenshot, ScreenCast, Wallpaper, consent (`PickConfirm`),
application choice (`PickApp`), and notifications (`Notify`) require compositor
IPC (see [ADR-0026](../adr/0026-prompter-process-retirement.md)). The two other
companion integrations are runtime-only wire contracts with no build-time
dependency:

| Companion | Interface | Wire contract | Discovery / Endpoint |
|---|---|---|---|
| **Arca** | `FileChooser` | `arca --chooser-prompt` JSON stdio, `ARCA_CONTRACT_VERSION = 1` ([ADR-0027](../adr/0027-arca-chooser-contract-crate.md)) | `$ATRIUM_FILE_CHOOSER_PROMPTER`, sibling `arca`, `PATH`, `/usr/local/bin/arca`, `/usr/bin/arca` |
| **Sigil** | `Secret` | `sigil-wire-v2` 8-byte binary framing (`b"SIGL"`, v2, max 4096 B payload; [ADR-0020](../adr/0020-secret-vault-delegation-to-sigil.md), [ADR-0023](../adr/0023-sigil-wire-v2-binary-protocol-and-zero-allocation-hardening.md)) | `$SIGIL_SOCKET_PATH`, or `$XDG_RUNTIME_DIR/sigil/native.sock`; both must be absolute |

Minimum companion releases for the Unreleased Portal line
([ADR-0030](../adr/0030-companion-contract-hardening-and-clean-break-alignment.md)).
An older arca speaks the retired v6 envelope and every FileChooser
request fails; an older sigil does not prompt for unlock or verify the
portal peer:

| Companion | Minimum | Required for |
|---|---|---|
| Arca | `v0.2.18` | Contract v1 envelopes, empty `Choice.selected`, Arca-side `save_files` paths |
| Sigil | `v1.4.3` | Interactive unlock on `GetApplicationSecret`, portal peer verification, `STATUS_DESYNCED` |
