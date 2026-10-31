# atrium-portal-request

The `org.freedesktop.impl.portal.Request` lifecycle for the atrium portal.

Every portal backend method receives a `handle` object path from the frontend.
The method registers an object at exactly that path for the duration of the
call, removes it before replying, and answers cancellation (`Request.Close`)
with response code 1. This crate is that lifecycle and nothing else.

## Contents

| Item | Role |
|---|---|
| `RequestTracker` | Which request paths are active and which the user closed |
| `register` / `finish` | Export and withdraw the request object |
| `dispatch` | Register, hand one job to a worker, await its reply, finish |
| `sync` | Poison-tolerant locking for the bookkeeping above |

## The lock policy is part of the contract

`sync` is not a general utility grab bag. Its module documentation records a
process-wide decision: the daemon is D-Bus-activated and resident, so a
panicking worker must not poison a lock into a permanent crash loop. The
helpers recover the inner state and log once per recovery. They must not be
used where a partially applied multi-field mutation would corrupt; keep the
crash (or restructure the state) there instead.

## Scope

The crate is a `publish = false` workspace member and is not a stable
interface. It was named `atrium-portal-runtime` until [ADR-0025] fixed the
workspace topology: `runtime` is a generic container name that invites
unrelated code, and this crate has exactly one responsibility. ScreenCast's
session registry and restore-token store live with that interface instead,
because nothing outside ScreenCast uses them.

[ADR-0025]: ../../docs/adr/0025-workspace-crate-topology.md
