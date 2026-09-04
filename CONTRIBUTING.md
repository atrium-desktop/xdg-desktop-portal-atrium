# Contributing

Contributor documentation lives under `docs/dev/`; the
[documentation index](docs/index.md) links it. Read
[Documentation Governance](docs/dev/documentation/index.md) before changing
documentation, and report security issues through
[SECURITY.md](SECURITY.md) instead of a public issue.

## Build and Test

Install Rust 1.88 or newer (the minimum supported version), `pkg-config`,
the optics C libraries (flux, lens, and iris from the tagged
`ming2k/optics` release), and the PipeWire and SPA development files, then
run from the repository root:

```bash
cargo build --locked --workspace
cargo test --locked --workspace
```

`Cargo.lock` is committed and authoritative; always build with `--locked`.
The Portal source and build graph is independent from the Tessera repository:
do not add Tessera internal crates, Tessera Git dependencies, or sibling-path
patches. Coordinate compositor wire changes through the `atrium-portal-ipc`
projection as described in
[Cross-Repository Protocol Development](docs/dev/cross-repository-development.md).
For joint development against a sibling optics checkout, enable the local
override documented in the same page (copy `.cargo/optics-local.toml` to
`.cargo/config.toml`) and keep the path-resolved lockfile out of commits.

## Git Hooks

Enable the repository hooks once per clone:

```bash
git config core.hooksPath .githooks
```

The `pre-commit` hook rejects staged changes to `Cargo.toml`, `Cargo.lock`,
`.cargo/`, or any crate manifest that reference the Tessera source repository
(an `aegis-shell/tessera` Git source or a `../tessera` path dependency). Portal
builds must not depend on Tessera sources; use the Portal-owned
`atrium-portal-ipc` wire projection instead. Do not bypass the hooks.

## Commits and Pull Requests

- Keep changes minimal and scoped; update the documentation surface that
  corresponds to the code change in the same commit (the
  [update checklist](docs/dev/documentation/update-checklist.md) maps code
  changes to required documentation actions).
- Record user-visible changes in the `Unreleased` section of
  `CHANGELOG.md`.
- Record durable architectural decisions as ADRs under `docs/adr/`; do not
  edit accepted ADRs.
- State in the pull request description whether documentation was updated
  or why it was not.

## Continuous Integration

Every pull request runs, on Ubuntu 24.04 against the tagged optics release:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
ATRIUM_PORTAL_REQUIRE_E2E=1 \
ATRIUM_PORTAL_REQUIRE_PIPEWIRE_E2E=1 \
  cargo test --locked --workspace
cargo deny check
cargo doc --locked --workspace --no-deps
cargo build --locked --release --workspace
```

The clippy gate additionally runs on the 1.88 MSRV toolchain, and CI stages
the installation script's package. The required
end-to-end mode fails instead of skipping when `dbus-daemon`, the real
`xdg-desktop-portal` frontend, PipeWire, WirePlumber, or the GStreamer
PipeWire consumer is unavailable, so run it locally before pushing when the
change touches the portal request path. The full gate list, including
package staging, is the [Release Checklist](docs/dev/release-checklist.md).
