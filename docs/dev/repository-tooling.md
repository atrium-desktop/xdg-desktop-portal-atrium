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

## `scripts/meson-cargo-build.sh`

Meson's wrapper around the Cargo release build. Meson invokes it with a
mode, the source root, the build root, the Cargo executable, and the
output paths:

```text
meson-cargo-build.sh portal <source-root> <build-root> <cargo> <portal-out> <prompter-out>
meson-cargo-build.sh pam <source-root> <build-root> <cargo> <pam-out>
```

The `portal` mode builds `xdg-desktop-portal-atrium` and
`atrium-portal-prompter` with `--locked --release` into the build root's
`cargo-target/` and copies both binaries to the Meson output paths; the
`pam` mode does the same for `libpam_atrium.so`. Run it through Meson, not
by hand; `meson compile` wires the arguments.

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

## Prompter `build.rs` Rpath Re-Emission

`rustc-link-arg` does not propagate across crates, so the terminal
binary must re-emit the optics library paths itself.
`crates/atrium-portal-prompter/build.rs` reads the `DEP_IRIS_RPATHS`,
`DEP_LENS_RPATHS`, and `DEP_FLUX_RPATHS` metadata published by the optics
`-sys` crates and re-emits them as link search paths plus `DT_RPATH`
entries (via `-Wl,--disable-new-dtags`, so the search also covers
transitive `NEEDED` libraries such as liblens under libiris). This keeps
the prompter loading the chosen libflux/liblens/libiris both for
system-installed optics and for the opt-in local optics override. The
prompter's direct `flux-sys`/`iris-sys`/`lens-sys` dependencies exist only
to make that metadata visible; do not prune them.
