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

The sigil (Secret storage) and arca (FileChooser prompt) integrations follow
the same boundary ([ADR-0020](docs/adr/0020-secret-vault-delegation-to-sigil.md)):

- Integrate with sigil at runtime only, through the Portal-owned blocking
  client in `atrium-portal-secret/src/native.rs`; do not import sigil crates
  as path, Git, or registry dependencies.
- Integrate with arca as a runtime binary contract (`arca --chooser-prompt`);
  do not add arca source dependencies.
- Test both contracts with literal wire fixtures and Portal-owned fakes;
  never import sigil or arca implementation code into tests.

Per [ADR-0021](docs/adr/0021-headless-portal-and-optics-retirement.md), the
portal is 100% headless and has zero optics dependencies. All user-facing UI
prompts are delegated to companion runtime components (sigil, arca, tessera).
Do not re-introduce GUI or optics dependencies into this repository.

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
