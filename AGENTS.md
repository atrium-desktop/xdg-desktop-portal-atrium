# Repository Instructions

Before writing, modifying, or archiving documentation, read and follow
`docs/dev/documentation/index.md`. AI assistants may read
`docs/dev/documentation/` and suggest changes, but must not modify files in
that directory.

Do not bypass Git hooks. Enable them once per clone with
`scripts/setup-dev.sh` (idempotent; sets `core.hooksPath` to
`.githooks`).

Keep the Portal source and build graph independent from the Tessera repository:

- Do not add Tessera internal crates, Tessera Git dependencies, or sibling-path
  patches.
- Put compositor integration in the Portal-owned `atrium-portal-ipc` wire
  projection and keep it limited to compositor-owned resources.
- Test wire changes with literal protocol fixtures and the independent test
  server; do not import the compositor's server implementation into tests.

The prompter UI builds on the optics stack (iris/lens), resolved from the
tagged `ming2k/optics` release — an independent third repository, so the
rules above do not cover it. For joint development against a sibling optics
checkout:

- Enable local mode with `cp .cargo/optics-local.toml .cargo/config.toml`.
  The generated `.cargo/config.toml` is Git-ignored; keep it that way.
- Leave `Cargo.lock` in the state the local patch produces while local mode
  is active; do not commit the path-resolved lockfile.
- Promote an Optics release by bumping every tagged dependency in
  `Cargo.toml` together and regenerating the canonical lockfile; keep
  `scripts/optics-release-ref.sh`'s expected package count in sync.
- `atrium-portal-prompter/build.rs` re-emits the `-sys` crates' rpath
  metadata so the binary finds the chosen liblens/libflux/libiris at
  runtime; the direct `flux-sys`/`iris-sys`/`lens-sys` dependencies exist
  only to make that metadata visible — do not prune them.

## Testing Rules & AI Behavioral Boundaries
- **Runner Tool**: Always use `cargo nextest run` instead of `cargo test` for unit and integration tests. Use `cargo test --doc` only when verifying documentation tests.
- **Tiered Verification (Do Not Over-test)**:
  - During intermediate edits, use `cargo check -p <crate>` for fast type checking, or `cargo nextest run -p <crate> --lib` / `cargo nextest run -E 'test(name)'` for targeted test validation.
  - Run full workspace tests (`cargo nextest run --workspace`) ONLY at the final delivery stage of a task.
- **Async Test Safety**:
  - Never write unbounded `rx.recv().await` on open channels. Always wrap with `tokio::time::timeout` or use non-blocking `try_recv()` to prevent infinite hangs/deadlocks.
  - Always use `#[tokio::test(start_paused = true)]` for tests involving timers, timeouts, or sleep to advance virtual time instantly.
- **Environment & State Isolation**:
  - Never hardcode ports (always bind to `:0` for ephemeral ports).
  - Never touch user home or global state paths; always isolate using `tempfile::tempdir()` and local sandbox roots.
