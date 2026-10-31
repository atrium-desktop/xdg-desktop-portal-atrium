# Repository Tooling

This page covers the repository's local tooling: the Git hooks, the
helper scripts in `scripts/`, the Rust toolchain pin, and the prompter's
linker fixup. For build and test commands see the
[project README](../../README.md); for CI gates see the
[Release Checklist](release-checklist.md).

## Git Hooks

Enable the repository hooks once per clone:

```bash
scripts/setup-dev.sh
```

The script sets `core.hooksPath` to `.githooks` (the manual equivalent is
`git config core.hooksPath .githooks`) and is idempotent: re-running it
reports that nothing changed.

`.githooks/pre-commit` inspects staged changes to `Cargo.toml`,
`Cargo.lock`, `.cargo/`, and every crate manifest. It rejects any added
line referencing the Tessera source repository — an `aegis-shell/tessera` Git
source or a `../tessera` path dependency. Portal builds must not depend on
Tessera sources; compositor integration goes through the Portal-owned
`atrium-portal-ipc` wire projection instead (see
[Cross-Repository Protocol Development](cross-repository-development.md)).
Do not bypass the hook.

## Rust Toolchain Pin

The root `rust-toolchain.toml` selects the `stable` channel with the
`rustfmt` and `clippy` components, so a fresh clone builds with the same
toolchain shape CI's main job uses. The CI MSRV job overrides the pin
with `RUSTUP_TOOLCHAIN=1.88.0`, which outranks `rust-toolchain.toml`, so
the minimum supported toolchain really runs there.

## `scripts/install.sh`

Builds the release artifacts with Cargo and installs the full backend
surface: the two private executables, the generated D-Bus service file
(with `@libexecdir@` substituted to the final path, ignoring `DESTDIR`),
and the portal metadata:

```text
scripts/install.sh [--prefix PREFIX] [--libexecdir DIR] [--datadir DIR]
                   [--no-build]
```

Honors `DESTDIR` for staged distribution packaging; `--no-build` installs
from the existing `target/release` artifacts. Run it directly or from a
package recipe — there is no configure step to cache.

## `scripts/optics-release-ref.sh`

Extracts the single shared optics release tag from `Cargo.toml`:

```bash
scripts/optics-release-ref.sh        # prints the tag, e.g. v0.0.14
```

The script hard-fails unless exactly five tagged `ming2k/optics`
dependencies (`iris`, `lens`, `flux-sys`, `iris-sys`, `lens-sys`) share
one tag. CI uses it to check out the matching optics release. Promoting an
optics release is the manual inverse: bump every tagged dependency in
`Cargo.toml` together, regenerate the canonical lockfile, and keep the
script's expected package count in sync if a sixth optics crate is ever
added.

## Native Dependency Boundary

ADR-0025 concentrates every native-library dependency and every `unsafe`
block in `crates/atrium-portal-cast` (the `pipewire` binding and `libc`).
Nothing else in the workspace links them, so:

- interface work, unit tests, and `atrium-portal-ipc` do not need the PipeWire
  development packages installed;
- a dependency or link-flag change is scoped to one crate;
- `cargo clippy --workspace` enforces `undocumented_unsafe_blocks`, so every
  `unsafe` block in that crate carries a `SAFETY:` justification.

The daemon does keep `libc` for `getpwuid`, `sysconf`, and `setrlimit`, and
`atrium-portal-ipc` keeps it for sealed-memfd handling; both are audited the
same way.
