# Release Checklist

## Version Alignment

1. Bump `workspace.package.version` in `Cargo.toml` and every
   `[Unreleased]` heading in `CHANGELOG.md` (add the matching
   `[0.0.N]: …/tag/v0.0.N` link-ref definition and move the
   `[Unreleased]` compare base to the new tag).
2. Set `meson.build`'s `project(version:)` to the same value.
3. Run `scripts/version-consistency.sh` — it fails until every versioned
   surface below agrees:
   - `Cargo.toml` workspace version ↔ `meson.build` version ↔ the newest
     `CHANGELOG.md` release heading and link refs,
   - `README.md` and `docs/reference/compatibility.md` protocol numbers ↔
     `atrium-portal-ipc`'s `PROTOCOL_VERSION`/`MIN_PROTOCOL_VERSION`,
   - `docs/dev/portal-ui-testing.md` payload versions ↔
     `atrium-portal-prompter`'s `PROCESS_CONTRACT_VERSION`,
   - `docs/dev/documentation/` is never modified by a code change
     (governance firewall, see `AGENTS.md`).

## Canonical Dependency State

1. Resolve the workspace with `--locked` from a clean checkout.
2. Confirm that `Cargo.lock` and `cargo tree --workspace` contain no Tessera Git
   source or internal Tessera crate.
3. Run the independent IPC fixtures and daemon-level media tests.
4. Confirm that the runtime protocol mapping matches the
   [Compatibility Reference](../reference/compatibility.md).

## Verification

Run every gate from the repository root:

```bash
cargo fmt --all -- --check
cargo +1.88.0 check --locked --workspace --all-targets
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked --workspace --all-targets -- -D warnings
ATRIUM_PORTAL_REQUIRE_E2E=1 \
ATRIUM_PORTAL_REQUIRE_PIPEWIRE_E2E=1 \
  cargo test --locked --workspace
cargo deny check
cargo doc --locked --workspace --no-deps
cargo build --locked --release --workspace
```

The required end-to-end mode fails instead of skipping when `dbus-daemon`,
the real `xdg-desktop-portal` frontend, PipeWire, WirePlumber, or the
GStreamer PipeWire consumer is unavailable.

## Package Staging

Build and inspect both licensing variants:

```bash
meson setup build-package --wipe \
  --buildtype=release --prefix=/usr -Dpam=false
meson compile -C build-package
DESTDIR="$PWD/stage" meson install -C build-package

meson setup build-package --reconfigure -Dpam=true
meson compile -C build-package
DESTDIR="$PWD/stage-pam" meson install -C build-package
```

Confirm executable modes, the configured D-Bus `Exec` path, the portal
metadata interface list, the interface routing, and the PAM module's
distribution license before publishing artifacts.

## Release Metadata

1. Move the `CHANGELOG.md` Unreleased entries into the release version and
   date.
2. Update the workspace and Meson project versions together.
3. Update the compatibility table when the verified Tessera runtime set or IPC
   protocol changes.
4. Tag only the reviewed canonical-lockfile commit.
