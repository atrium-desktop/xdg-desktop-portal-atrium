# Repository Tooling

This page covers the repository's local tooling: the Git hooks, the
helper scripts in `scripts/`, the Rust toolchain pin, and the native
dependency boundary. For build and test commands see the
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
with `RUSTUP_TOOLCHAIN=1.89.0`, which outranks `rust-toolchain.toml`, so
the minimum supported toolchain really runs there.

## `scripts/install.sh`

Builds the release artifacts with Cargo and installs the full backend
surface: the private daemon executable, the generated D-Bus service file
(with `@libexecdir@` substituted to the final path, ignoring `DESTDIR`),
and the portal metadata:

```text
scripts/install.sh [--prefix PREFIX] [--libexecdir DIR] [--datadir DIR]
                   [--no-build]
```

Honors `DESTDIR` for staged distribution packaging; `--no-build` installs
from the existing `target/release` artifacts. Run it directly or from a
package recipe — there is no configure step to cache.

## Native Dependency Boundary

ADR-0025 concentrates the native-library dependency — the `pipewire`
binding — in `crates/atrium-portal-cast`. Nothing else in the workspace links
it, so:

- interface work, unit tests, and `atrium-portal-ipc` do not need the PipeWire
  development packages installed;
- a PipeWire dependency or link-flag change is scoped to one crate.

`libc` is not confined the same way. The daemon calls it for `getpwuid_r`,
`sysconf`, `setrlimit`, `geteuid`, and the arca `SIGTERM`;
`atrium-portal-ipc` for sealed-memfd and descriptor passing;
`atrium-portal-request` for thread ids; and `atrium-portal-secret` for
`poll(2)`. Every crate inherits `[workspace.lints]`, so
`undocumented_unsafe_blocks = "deny"` requires a `SAFETY:` justification on
each of those `unsafe` blocks. Crates with no `unsafe` carry
`#![forbid(unsafe_code)]`.
