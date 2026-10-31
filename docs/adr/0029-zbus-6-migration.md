# ADR-0029: zbus 6 migration and the connection driver

- Status: Accepted
- Date: 2026-10-31

## Context

`cargo deny check` reported three unmaintained advisories against the
dependency graph:

| Advisory | Crate |
|---|---|
| RUSTSEC-2026-0370 | `zvariant` |
| RUSTSEC-2026-0371 | `zbus_names` |
| RUSTSEC-2026-0372 | `zvariant_derive` |

None is a vulnerability: all three say the same thing — zbus 6.0 absorbed these
crates, so as standalone packages they receive no further fixes. The portal
reached them only through zbus 5, so the remedy is the zbus 6 upgrade. Leaving
them ignored would have made CI green while recording a standing exception that
only grows more dangerous as the packages age.

zbus 6 could not be adopted incrementally, because it makes two breaking changes
that reach the daemon's core:

1. **The blocking API is gone.** `zbus::blocking`, every `*ProxyBlocking`, and
   the `blocking-api` feature have no equivalent. This daemon is
   thread-per-interface with synchronous methods, and it used the blocking API
   in 13 places across 7 files.
2. **`Value` became `Variant`**, `zvariant` became the deprecated `zvariant`
   alias of `zbus`, and `OwnedValue` followed to `OwnedVariant`. The workspace
   used these types in 84 places.

zbus 6 also requires **Rust 1.89**; the workspace MSRV was 1.88.

## Decision

### 1. The MSRV moves to 1.89

This is a hard requirement of zbus 6, not a preference. The workspace now
declares `rust-version = "1.89"`, CI pins `1.89.0`, and the install and release
documentation match. A dependency whose minimum exceeds the declared MSRV would
otherwise make `cargo check` fail for exactly the users the MSRV promises.

### 2. Sync methods hand work to a connection driver

zbus 6's `block_on` has one contract that shapes the design: a connection's work
runs **on the thread inside `block_on`**, between polls of the future, and
`block_on` must not be called from inside a zbus task — an interface method. A
per-call `block_on` in an interface method would therefore wait for the very
connection it is running on.

`atrium-portal-request::driver` makes this explicit:

- [`driver::spawn`] starts one thread per connection that sits in `block_on` for
  the life of the process. That thread is what keeps the object server,
  signals, and reply delivery moving.
- [`Driver::call`] sends a closure to that thread and waits for its result, so a
  synchronous interface method never drives the connection itself.
- `Driver::call` asserts it is not on the driver thread, because that call can
  never return.

Assembly (`run()`) installs every interface, requests the bus name, and pings
the bus through `Driver::call`. The main loop keeps the process tied to the
connection exactly as before.

### 2a. Nothing served may block or `block_on` the connection runtime

The rule above forbids `block_on` in an interface method. The same reasoning
forbids every other unbounded wait there, because the method runs **on the
connection's runtime**: anything that parks that task parks the thread that
also reads the socket and dispatches every other interface's methods. A hung
method does not merely fail its own caller — the whole backend stops answering
while still owning its bus name, so clients hang instead of failing cleanly.

Three shapes are therefore required, and all three bindings the interfaces
that can block use them:

- **Await, never block.** A method that must reach another service (logind for
  `Inhibit`) awaits the call. The future yields to the runtime, so the
  connection keeps serving. `Inhibit` does this directly.
- **Dispatch to a worker, never wait inline.** A method whose work is unbounded
  in wall-clock time because it waits on a human (`RetrieveSecret`, which waits
  for sigil's unlock prompt; `FileChooser`, `ScreenCast`, `Print`) exports the
  `Request` object, sends a job over its supervised bounded queue, and awaits a
  one-slot reply. The blocking happens on the worker's thread, which owes
  nothing to the connection. `atrium_portal_request::dispatch` is that shape.
- **`try_send`, never `send`.** A bounded queue's `send` blocks when full.
  `Session::Close` was the one method that used it; a full screencast queue
  would stall the runtime. Full queues answer or defer, and never wait inline.

A blocking section that genuinely cannot move — writing to a caller-supplied
descriptor — must additionally be bounded, so a non-draining reader costs one
request rather than the daemon. Because supervision only restarts a worker that
*panics*, a worker that blocks is blocked for the session: interfaces whose work
can block indefinitely (`RetrieveSecret`) therefore run each request on its own
bounded task thread rather than serving the queue serially.

Two further rules follow from the same premise:

- **Bound what the method waits on, in both directions.** `Driver::call` bounds
  the enqueue as well as the answer: a wedged driver is precisely one whose
  queue is not draining, so an unbounded `send` on a full queue would make the
  answer timeout unreachable.
- **A bounded queue needs a non-blocking producer.** A deferral queue behind a
  wedged consumer is only safe if producing into it cannot block and cannot grow
  without bound.

### 2b. Every wait on another process is bounded

Portals talk to services that can be wedged: the compositor, sigil, logind,
`lp`, `arca`. An unbounded read on any of them converts their hang into this
daemon's hang. Each client carries a timeout and reports a typed error when it
fires, so the failure is attributed (`atrium-portal-secret`'s socket timeout,
`atrium-portal-ipc`'s `RPC_TIMEOUT`, `atrium-portal-cast`'s `IPC_TIMEOUT`).
Where a timeout is long by design — sigil's, which outlasts a user typing a
password — the work must also run off the connection runtime (2a).

### 3. `Value` → `Variant` is applied everywhere, not aliased

The workspace imports `Variant`, `OwnedVariant`, `Structure`, `Dict`, `Array`,
and `DynamicType` from `zbus` directly. `zbus::zvariant` is a deprecated alias
that disappears in 7.0, so depending on it would only defer the same rename
while emitting warnings in the meantime.

`StructureBuilder` is imported as `zbus::wire::StructureBuilder`: the type is
still public but is no longer in the root re-export list.

### 4. Tests get a client-side facade, not a second driver

The integration tests are clients: they connect, call, and wait for signals.
For a client, one `block_on` per call is correct — the future both issues the
call and polls the connection for the reply. `tests/common/bus.rs` is a thin
adapter (`connect`, `proxy`, `call`, `try_call`, `next_signal`) so the tests
read as synchronous code without inventing a second driver they do not need.

The asymmetry is deliberate and is the reason both modules document it: the
daemon *serves*, so it needs a driver thread; the tests *call*, so they do not.

### 5. `fetch_update` stays until the MSRV allows `try_update`

`try_update` is the non-deprecated spelling but is stable only since 1.95. At
MSRV 1.89 the deprecated `fetch_update` is what compiles, so the one call site
carries a local `#[allow(deprecated)]` with the reason recorded.

## Alternatives

- **Ignore the three advisories in `deny.toml`.** Rejected: it records a
  permanent exception for packages that will never be maintained again, and the
  exception grows riskier as they age. It also hides the next advisory behind
  the same list.
- **Stay on zbus 5 and vendor a patched `zvariant`.** Rejected: forking three
  crates to avoid a version bump costs far more than the bump, and the fork
  inherits the maintenance burden the advisory is about.
- **`block_on` per method call.** Rejected: it deadlocks by construction — the
  future waits for a reply that only the connection it is blocking can deliver.
- **Move the whole daemon to async.** Rejected: it would rewrite every interface
  and worker, and the daemon's blocking work (a chooser child process, a
  PipeWire loop, synchronous file I/O) does not fit an async runtime. The
  driver thread contains the async surface to one module.
- **Keep `zbus::zvariant` imports.** Rejected: they warn today and vanish in
  7.0. The rename is mechanical; deferring it only schedules the same work.

## Consequences

- `cargo deny check` reports `advisories ok, bans ok, licenses ok, sources ok`
  with no ignore list.
- The workspace has one D-Bus dependency family instead of four
  (`zbus` replaces `zbus`, `zvariant`, `zvariant_derive`, `zbus_names`), so the
  dependency graph shrinks.
- MSRV is now 1.89. Distributions packaging Rust 1.88 must move forward.
- `run()` installs interfaces through the driver, so a future interface addition
  follows the same `driver.call` shape rather than a bare `object_server().at`.
- `atrium-portal-request` gains `driver` and `libc` (for the thread id used by
  the re-entry assertion), keeping the async boundary in one place.
- The test facade must grow with the daemon's client needs; that is one small
  module rather than an async rewrite of eleven test files.
