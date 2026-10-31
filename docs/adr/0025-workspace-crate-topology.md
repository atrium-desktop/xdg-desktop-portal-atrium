# ADR-0025: Workspace crate topology

- Status: Accepted
- Date: 2026-10-31

## Context

ADR-0021 retired optics and made `xdg-desktop-portal-atrium` a 100% headless
daemon, but it deliberately left the workspace shape untouched: the binary
crate still carried every portal interface, the PipeWire producer stack,
freedesktop application resolution, and the shared request plumbing in a
single 15k-line crate, while three sibling libraries held the wire projection,
the prompter process contract, and the request lifecycle.

Two structural costs followed:

1. **The native dependency and the `unsafe` are uncontained.**
   `pipewire` (a binding over system `libpipewire`) is a dependency of the
   binary crate, so every test and every consumer of the daemon libraries
   links it. The `unsafe` blocks in `cast/` (`copy.rs` 9, `meta.rs` 17,
   `mod.rs` 7, `publish.rs` 2) sit beside pure parsing and D-Bus code with no
   crate boundary between them, so a reviewer cannot scope an audit.
2. **Freedesktop resolution is unreachable for tests.**
   `.desktop` scanning, the `mimeinfo.cache`/globs2 indexes, `mimeapps.list`
   precedence, and `Exec` field-code expansion are pure logic with zero
   `unsafe`, but they are private to the daemon crate. They are used by three
   portal interfaces (AppChooser, OpenURI, DynamicLauncher) and can only be
   exercised through a process-wide daemon harness.

A third, subtler cost is naming discipline. `atrium-portal-runtime` today
holds the `org.freedesktop.impl.portal.Request` lifecycle and nothing else.
It is small, but it is coherent: its module documentation records one
process-wide policy (a panicking worker must not poison a resident daemon
into a crash loop). An earlier proposal in review was to rename it to `core`
and also relocate the ScreenCast session registry and restore-token store into
it. That proposal was rejected before adoption: both of those types are used
by exactly one portal interface, so the only shared property they have with
the request lifecycle is that they are "infrastructure". A crate named for
being infrastructure becomes a junk drawer by construction — anything whose
home is unclear ends up there, and the crate stops answering "what changes
together?".

## Decision

### 1. A crate exists only for an isolation boundary or an independent consumer

A workspace member is justified by exactly one of:

- **An isolation boundary**: a native dependency, an `unsafe` surface, or a
  trust boundary that must not spread to the rest of the build.
- **An independent consumer**: code that more than one consumer compiles
  against *and* whose interface is a real contract (a wire format, a process
  protocol, a shared state machine with its own invariants).

Module count, line count, and "this looks generic" are not justifications.
Names must be concrete nouns. `core`, `common`, `utils`, `shared`, `misc`, and
`support` are banned as crate names; a crate that cannot be named with a
concrete noun is a crate that has not found its boundary.

### 2. The workspace members and their single responsibilities

| Crate | Single responsibility | Kind |
|---|---|---|
| `xdg-desktop-portal-atrium` | Process assembly: bus connection, interface registration, workers | binary |
| `atrium-portal-ipc` | The Tessera compositor wire projection (protocol 24–32) | boundary |
| `atrium-portal-cast` | The PipeWire producer stream | isolation |
| `atrium-portal-apps` | Desktop Entry and MIME Apps resolution | consumer |
| `atrium-portal-secret` | The Secret portal projected onto the sigil socket | boundary |
| `atrium-portal-request` | The `org.freedesktop.impl.portal.Request` lifecycle | consumer |

`atrium-portal-runtime` is renamed to `atrium-portal-request`. The rename is
not cosmetic: `runtime` is a generic-container name that invited the rejected
`core` proposal, and the crate's actual content — `RequestTracker`, the
`Request` object's `Close` handler, and the dispatch/registration helpers plus
the poison-tolerant lock helpers those use — is exactly one lifecycle. The
`sync` helpers stay, because `RequestTracker` is their first user and their
module documentation is a process-wide policy statement, not a grab bag.

### 3. The daemon crate reorganizes by vertical, not by horizontal layer

The daemon keeps every portal interface, because a portal interface is D-Bus
projection plus that interface's own request policy — there is no second
consumer for any of them, and a crate per interface would add 17 members whose
versions move in lockstep with no isolation to show for it. Inside the crate
the layout is vertical slices:

```
src/
├── lib.rs              # assembly only
├── main.rs             # the process entry point
├── iface/              # one module per D-Bus interface, no cross-imports
├── companion/          # the one out-of-process call (arca)
├── screencast/         # ScreenCast + its session registry + its restore tokens
│   ├── mod.rs
│   ├── session.rs
│   └── restore.rs
├── fsx/atomic.rs       # owner-verified atomic writes
├── uri.rs              # file:// rendering and parsing
└── ipc.rs              # the compositor client the interfaces share
```

`lib.rs` re-exports the interface modules (`pub(crate) use iface::settings`
and so on) so assembly code keeps naming an interface by its own name; the
`iface` parent exists only to give the fourteen interface files one home.

The rule the layout enforces: **a module that is used by exactly one portal
interface lives inside that interface's vertical**. The ScreenCast session
registry and restore-token store move from `src/session.rs` and `src/persist.rs`
into `src/screencast/`, because deleting ScreenCast must delete them and
nothing else. Conversely, an interface module that starts importing a sibling
means the shared piece belongs in a crate or in `companion/`, not that a
cross-import should be added.

### 4. `files.rs` is dissolved

`src/files.rs` is titled "portal capture persistence" but holds three
unrelated things with three unrelated consumers:

- capture cache resolution and PNG staging (`cache_dir`, `write_capture`) —
  Screenshot only;
- atomic write and owned-directory removal (`write_atomic`,
  `remove_owned_dir`) — several callers;
- `file://` URI rendering and parsing (`file_uri`, `path_from_file_uri`) —
  Email, OpenURI, and FileChooser.

The capture cache folds into `screenshot`. The atomic-write helper becomes
`fsx::atomic` and the URI helpers become `uri`. Neither becomes a crate: they
have no isolation property and no interface worth versioning, and inventing a
crate for "small helper functions" is the junk drawer under a different name.

### 5. Workspace-wide lint policy replaces per-crate convention

Every crate declares `lints.workspace = true` and inherits a single
`[workspace.lints]` table. Crates with no `unsafe` (the wire projection's
pure modules, `atrium-portal-apps`, `atrium-portal-request`, and the prompter
contract) carry `#![forbid(unsafe_code)]`. `atrium-portal-cast` and the daemon
do not, so the absence of `forbid` is a meaningful signal about where review
effort belongs.

## Alternatives

- **A crate per portal interface.** Rejected: 17 members, no independent
  consumers, no isolation property, and every version bump would rewrite the
  whole workspace. The interfaces already do not import each other; the layout
  rule in §3 enforces that without a build-graph cost.
- **Rename `atrium-portal-runtime` to `core` and move shared state into it.**
  Rejected: see Context. `session` and `persist` are used by one interface, and
  a generic name attracts unrelated code.
- **Split the wire projection's test server into its own crate.** Rejected:
  `testing.rs` builds on the crate's private codec and schema types, so
  extraction would force those internals public. A `test-server` feature plus
  the resolver-3 dev/build feature separation already prevents it from
  reaching production builds.
- **Keep `files.rs` as is.** Rejected: it is already the pattern this ADR
  bans at crate scope, reproduced at module scope.

## Consequences

- A change to the PipeWire stack, or an audit of its `unsafe`, is scoped to
  one crate that the rest of the workspace does not build against for tests.
- Freedesktop resolution gains a unit-test and fuzz surface with no daemon
  process and no D-Bus.
- The daemon crate's dependency on the native PipeWire library disappears, so
  interface work no longer requires the system binding to be present.
- `atrium-portal-runtime`'s rename touches every import site and
  `scripts/version-consistency.sh`; the crate is `publish = false`, so no
  external consumer is affected.
- The layout rule is reviewable: "does this module have a second consumer
  outside one interface?" is a yes/no question, unlike "does this belong in
  core?".
