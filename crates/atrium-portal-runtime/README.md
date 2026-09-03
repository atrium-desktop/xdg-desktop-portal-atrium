# atrium-portal-runtime

Shared backend primitives for Tessera portal components.

The crate owns the `org.freedesktop.impl.portal.Request` lifecycle, including
exact-path registration, `Close` cancellation tracking, and cleanup. It has
no Tessera IPC dependency and is linked into the private
`xdg-desktop-portal-atrium` process.
