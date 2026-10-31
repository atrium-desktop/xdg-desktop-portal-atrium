# Release Checklist

## Version Alignment

1. Bump `workspace.package.version` in `Cargo.toml` and every
   `[Unreleased]` heading in `CHANGELOG.md` (add the matching
   `[0.0.N]: …/tag/v0.0.N` link-ref definition and move the
   `[Unreleased]` compare base to the new tag).
2. Run `scripts/version-consistency.sh` — it fails until every versioned
   surface below agrees:
   - `Cargo.toml` workspace version ↔ the newest
     `CHANGELOG.md` release heading and link refs,
   - `README.md` and `docs/reference/compatibility.md` protocol numbers ↔
     `atrium-portal-ipc`'s `PROTOCOL_VERSION`/`MIN_PROTOCOL_VERSION`,
   - `docs/dev/portal-ui-testing.md` payload versions ↔
     `atrium-arca-contract`'s `ARCA_CONTRACT_VERSION`,
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

Build and inspect the package:

```bash
cargo build --locked --release -p xdg-desktop-portal-atrium
DESTDIR="$PWD/stage" ./scripts/install.sh --prefix /usr --no-build
```

Confirm executable modes, the configured D-Bus `Exec` path, and the portal
metadata interface list before publishing artifacts.

## Process Hygiene

1. Run `cargo nextest run --workspace` twice and confirm the `dbus-daemon`
   count is unchanged afterwards (see
   [Test Process Hygiene](test-process-hygiene.md)). A leak only shows up under
   a full parallel run, so a single green run is not evidence.
2. Confirm no `D`-state `wireplumber`/`pipewire` process remains from earlier
   runs; one wedged in uninterruptible audio I/O cannot be killed and will
   degrade every later PipeWire test until reboot.
3. Confirm every skipped test names a reason and that no skip is masking a
   required dependency in the target environment.

## Release Metadata

1. Move the `CHANGELOG.md` Unreleased entries into the release version and
   date.
2. Update the workspace version.
3. Update the compatibility table when the verified Tessera runtime set or IPC
   protocol changes.
4. Tag only the reviewed canonical-lockfile commit.
