# xdg-desktop-portal-atrium

The `xdg-desktop-portal-atrium` crate builds the private D-Bus backend
process. It owns interface registration, scoped Tessera IPC adapters, request
workers, the FileChooser prompter lifecycle, and the PipeWire ScreenCast
bridge. FileChooser requests run in a fresh `atrium-portal-prompter` child;
neither the backend nor Tessera compositor implements a file browser.

Secret storage is provided by `atrium-portal-secret`. Shared portal Request
objects and cancellation tracking come from `atrium-portal-runtime`. Both are
linked into this one process; neither is deployed separately.
