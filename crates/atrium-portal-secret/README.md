# atrium-portal-secret

Encrypted Secret implementation linked into the
`xdg-desktop-portal-atrium` process.

The crate owns the at-rest vault, the native
`org.freedesktop.impl.portal.Secret` backend, per-application HKDF key
derivation, and the single-flight unlock coordinator. It receives password
prompting through the narrow `SecretPrompter` capability and does not depend
on Tessera IPC.

This crate does not implement the separate Secret Service API
`org.freedesktop.secrets`. Desktop keyring clients must use a complete Secret
Service provider supplied by the distribution.
