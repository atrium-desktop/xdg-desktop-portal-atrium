# ADR-0028: Supervised interface workers and hermetic test processes

- Status: Accepted
- Date: 2026-10-31

## Context

Two production defects surfaced while bringing v0.0.34 to release readiness.
Both had the same shape: a failure that the test suite could not see, and that
accumulated silently until the machine or the daemon was unusable.

### A panic bricks an interface for the life of the process

Every portal interface hands its work to a dedicated thread fed by a bounded
`mpsc::sync_channel`, because a D-Bus method must answer promptly while a
consent prompt or a capture may block for as long as the user takes. The
worker owned both the receiving half and the job loop.

If the loop panicked, the thread unwound and its `Receiver` was dropped. The
`SyncSender` held by the interface struct then had no receiver, so every later
`try_send` failed and `dispatch` translated that into
`Failed("<worker> worker is gone")`. Nothing restarted the worker. The daemon
is D-Bus-activated and resident, so "until the process restarts" means "until
the session ends": one panic permanently disabled one interface — including
Access, Account, and the file chooser.

### Leaked test processes degrade the machine

A full `cargo nextest run --workspace` left four orphaned `dbus-daemon`
processes behind on every invocation. Repeated runs accumulated them; at the
point this was investigated the machine had 66 buses and 48 `wireplumber`
processes wedged in uninterruptible sleep (`D`) inside the ALSA probe
(`snd_pcm_substream_proc_status_read`). Those processes cannot be killed, even
with `SIGKILL`, because the kernel never delivers a signal to a task blocked in
uninterruptible I/O. They held the audio device, so the isolated PipeWire stack
a screencast test starts could no longer come up and every `media` test timed
out after 15 s. The suite passed or failed depending on how many runs preceded
it.

Three separate causes:

1. `atrium-portal-request`'s unit tests spawn their own `dbus-daemon` with no
   cleanup at all. This was the actual leak: none of the four orphans in a
   workspace run came from the integration harness, which does clean up.
2. Both `PrivateBus` helpers relied only on `Drop`. A test killed for
   exceeding its timeout never runs `Drop`, so the bus survived.
3. The isolated PipeWire, WirePlumber, and GStreamer children had the same gap.

The three `public_frontend_*` tests had a related, smaller problem: they waited
10 s for `org.freedesktop.portal.Desktop` and failed when it never appeared.
A private `dbus-daemon` in CI has no service-activation helper, so the frontend
blocks on `StartServiceByName` for `PermissionStore` and `Documents` and then
aborts with "No document portal". The test could never have passed in that
environment, but it spent its timeout proving it.

## Decision

### 1. Interface workers run under a supervisor

`atrium-portal-request` owns `Supervisor<J>`: a bounded queue whose worker is
restarted after a panic. Every one of the daemon's eleven interface workers is
converted (`capture`, `screencast`, `file chooser`, `account`, `access`,
`app chooser`, `open uri`, `background`, `wallpaper`, `print`, and the restore
gc).

The worker receives `&Receiver<J>` and keeps its own loop, so per-interface
state — concurrency counters, the `CastJob` self-send handle — survives exactly
as it did before. `Receiver::recv` needs only `&self`, which is what lets the
supervisor hand the *same* queue back to a restarted worker instead of losing
the channel with the panicked thread.

**The in-flight request is not retried.** Its caller has already been told the
worker died, and replaying a prompt or a capture could double-publish a stream
or a screenshot. Recovery is forward-only: the next request is served
normally, the panicking one answers its failure.

The panic is logged at `error` level. It is never silent, because a worker that
panics repeatedly is a real defect even though the interface stays up.

### 2. A killed test must not leak a process

Every process a test starts is armed with `PR_SET_PDEATHSIG` **in the child,
between `fork` and `exec`** (that is what `CommandExt::pre_exec` is for;
calling `prctl` in the parent would arm the parent instead). This is the only
teardown that survives `SIGKILL`, because the kernel — not the dead parent —
delivers it. `Drop` remains, but it is now the tidy path rather than the only
path.

The child also checks `getppid() == 1` right after arming: if the parent died in
the window between `fork` and `prctl`, the death signal already fired for
nobody and will never arrive again, so the child exits immediately instead of
lingering as an orphan.

### 3. An un-runnable test environment is a skip, not a timeout

A test that cannot pass must say so in milliseconds, with the reason. Two
checks encode this:

- `frontend_prerequisites` probes the bus with a `StartServiceByName` for a name
  nobody serves. A bus with an activation helper answers promptly; one without
  hangs, and the probe bounds that at two seconds. This decides before the
  frontend is even spawned.
- `audio_stack_is_wedged` scans `/proc` for a process in `D` state waiting on
  an `snd_*` channel. Such a process cannot be killed and holds the audio
  device, so the isolated PipeWire stack cannot start.

Both report a skip under ordinary runs and a hard failure when
`ATRIUM_PORTAL_REQUIRE_E2E` / `ATRIUM_PORTAL_REQUIRE_PIPEWIRE_E2E` is set, so CI
stays strict where the environment is controlled.

## Alternatives

- **Restart the whole process on any worker panic.** Rejected: it drops every
  unrelated in-flight request, tears down live ScreenCast sessions, and makes a
  single interface's bug into a session-wide outage. The supervisor contains it.
- **Retry the panicking request after restart.** Rejected: see §1. Replaying a
  prompt can double-grant a permission and replaying a capture can publish two
  streams for one request.
- **Wrap each job in `catch_unwind` inside the worker.** Rejected: the panic
  unwinds the worker's own stack, so any local state it was carrying is already
  lost. Supervision has to own the boundary, not the job.
- **Rely on `Drop` plus a cleanup sweep.** Rejected: it cannot help a process
  killed with `SIGKILL`, which is exactly what a test timeout produces.
- **Detect leaked processes in CI instead of preventing them.** Rejected: it
  treats the symptom. The leaks also poisoned the developer machine, where no
  CI check runs.

## Consequences

- A panicking interface worker costs one failed request, not the interface.
- The supervisor costs one thread and a `catch_unwind` per worker call, which is
  negligible next to a prompt or a capture.
- Worker functions now take `&Receiver<J>`; a worker that needs to own its
  receiving half would have to be restructured, which is the intended signal
  that the supervisor should own it.
- Test processes no longer outlive the test binary on any exit path, so a
  developer machine no longer degrades across runs.
- The two environment probes make the suite's skips truthful and greppable;
  a green run no longer hides an environment that could not have run the test.
