# atrium-portal-ipc

`atrium-portal-ipc` is the Portal-owned client for the subset of Tessera IPC
protocol version 29 (negotiating down to 24) needed for compositor-owned
resources: settings, output enumeration, screenshots, target picking, and
output streaming.

The crate implements the wire contract independently. It does not depend on
the Tessera source tree or on compositor-internal Rust crates. Its test server
drives daemon integration tests without making the production dependency
graph trust the server implementation.
