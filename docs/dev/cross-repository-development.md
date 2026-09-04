# Cross-Repository Protocol Development

The Portal and Tessera repositories have independent source trees, dependency
graphs, lockfiles, versions, and release lifecycles. They integrate at runtime
through the narrow Tessera IPC contract described in
[ADR-0004](../adr/0004-portal-ownership-and-runtime-ipc-boundary.md).

Two further sibling repositories integrate at runtime as well
([ADR-0020](../adr/0020-secret-vault-delegation-to-sigil.md)):

- **sigil** — the `org.freedesktop.secrets` provider. The Secret portal
  projects onto its Unix socket (`$XDG_RUNTIME_DIR/sigil/native.sock`,
  u32 big-endian length prefix + JSON, externally tagged enums) through the
  Portal-owned blocking client in `atrium-portal-secret/src/native.rs`. No
  sigil crate is imported; tests use literal wire fixtures and a
  Portal-owned fake server. The vault, `pam_sigil`, and the logind lock
  listener live in the sigil repository.
- **arca** — the FileChooser prompt binary. The portal locates `arca`
  beside the backend or under the standard `bin` directories and invokes
  `arca --chooser-prompt`; this is a runtime binary contract, not a source
  dependency. An `arca` release that renames or drops the flag requires a
  Portal change in the same window.

## Dependency Boundary

| Concern | Portal ownership | Tessera ownership |
|---------|------------------|-----------------|
| Public portal ABI | D-Bus adapters, request lifecycle, result encoding | None |
| Portal UI | FileChooser, Account confirmation, Secret password input | Window parenting through standard Wayland protocols |
| Runtime wire client | Protocol-29 projection and sealed-memfd receiver | Protocol server and authorization |
| Compositor resources | Validation, persistence, PipeWire publication | Settings, pixels, target selection, capture consent, frame streams |
| Source dependencies | Portal workspace crates and registry packages | No Portal build dependency |

Do not add Tessera internal crates, Git dependencies, or sibling path patches
to this repository. A local Tessera checkout is optional and never changes
Portal dependency resolution.

Per [ADR-0021](../adr/0021-headless-portal-and-optics-retirement.md), the portal
is purely headless and has no optics dependencies.

## Daily Development

Run the canonical dependency graph in every worktree:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
```

`Cargo.lock` is committed and authoritative. A linked Git worktree may still
be useful for ordinary branch isolation, but it does not need a particular
directory name or adjacent Tessera checkout.

## Compatible Tessera Changes

An internal Tessera refactor requires no Portal change when all serialized
protocol-24 requests, responses, events, blob framing, scope behavior, and
authorization remain compatible. Validate the Portal independently:

```bash
cargo test --locked -p atrium-portal-ipc --features test-server
cargo test --locked -p xdg-desktop-portal-atrium --test media
```

The tests use literal wire fixtures and a minimal server owned by this
repository. They do not import Tessera model, authority, client, or server code.
This separation prevents a matching bug in a shared implementation from
making both sides pass.

Before declaring a new Tessera release compatible, compare its advertised IPC
version and run the Portal against that released compositor. Add the verified
release to the
[Compatibility Reference](../reference/compatibility.md). Do not infer wire
compatibility from Tessera package versioning alone.

## Incompatible Wire Changes

Coordinate a wire change in this order:

1. Define the smallest compositor-owned operation and reject Portal-owned
   filesystem, account, secret, email, or policy state at the boundary.
2. Change the Tessera protocol version when an existing version cannot decode
   or preserve the new semantics safely.
3. Update `atrium-portal-ipc` with only the required projection.
4. Add literal request, response, event, and blob fixtures before using the
   new operation in a D-Bus adapter.
5. Extend the independent test server and run daemon-level tests.
6. Test against a tagged Tessera release that implements the same protocol.
7. Update the compatibility reference and changelog in both repositories.

Land and tag the compositor side before releasing a Portal version that
requires it. A temporary development branch may coordinate both changes, but
the Portal branch must remain buildable without fetching or locating the
Tessera source tree.

This ordering is a hard rule, not a courtesy. The projection once defined a
wallpaper operation ahead of the compositor — sealed memfd, a placement
field, a reply variant no Tessera release ever implemented — and every
request against a real compositor failed closed (see
[ADR-0011](../adr/0011-wallpaper-wire-reconciliation.md)). A projection
must never define an operation ahead of the compositor: the compositor
lands and tags first, and when any doubt arises the compositor's schema is
the wire truth. Derive fixtures from that schema, never from the
projection's own types.

## Release Validation

Run the complete Portal graph from a clean checkout:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
cargo build --locked --release --workspace
cargo tree --workspace
```

The dependency tree and `Cargo.lock` must contain no Tessera repository source
or internal Tessera crate. Then run the packaging checks and the real
runtime compatibility tests listed in the
[Release Checklist](release-checklist.md).
