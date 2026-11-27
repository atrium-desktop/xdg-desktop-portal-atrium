# Test Process Hygiene

Every process a test starts must die with the test binary, on **every** exit
path — including the ones where the test itself never gets to clean up. This
page records why that is not automatic, and what to do instead. The decision
record is [ADR-0028](../adr/0028-supervised-workers-and-test-isolation.md).

## Why `Drop` is not enough

The obvious approach — a guard struct whose `Drop` kills the child — covers the
ordinary path and only that path. It silently fails in exactly the cases that
matter:

| Exit path | `Drop` runs? |
|---|---|
| Test returns normally | yes |
| Test panics | yes (unwinding) |
| Test exceeds its timeout and is killed | **no** |
| Test binary aborts | **no** |
| Test binary is `SIGKILL`ed by CI | **no** |

A test timeout is not an exotic case: `nextest` kills a test at
`slow-timeout`, and a loaded machine reaches it. Every such kill used to leave
a `dbus-daemon` (or a `pipewire`, or a `wireplumber`) behind.

Those orphans are not merely untidy. A leaked `wireplumber` blocks in
uninterruptible sleep inside the ALSA probe; the kernel will not deliver any
signal to it, not even `SIGKILL`. It then holds the audio device, so the next
test that needs to start an isolated PipeWire stack cannot, and it times out
too. The suite degrades run by run until the machine is rebooted. This actually
happened: 66 leaked buses and 48 wedged session managers.

The isolated stack now runs WirePlumber under a dedicated `atrium-portal-test`
profile with `hardware.audio`, `hardware.bluetooth`, and
`hardware.video-capture` disabled, plus private config and state directories.
It never opens a sound card, Bluetooth adapter, or camera, so neither a leaked
nor a concurrent test stack can wedge the audio driver, and a driver wedged by
something else cannot stall the suite.

## The rule

Arm `PR_SET_PDEATHSIG` in the child, between `fork` and `exec`:

```rust
use std::os::unix::process::CommandExt as _;

unsafe {
    command.pre_exec(|| {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The parent may have died between `fork` and `prctl`; the death
        // signal then fired for nobody and will never arrive again.
        if libc::getppid() == 1 {
            libc::_exit(1);
        }
        Ok(())
    });
}
```

Three details that are easy to get wrong:

- **It must run in the child.** `CommandExt::pre_exec` is the hook for exactly
  this. Calling `prctl` on the parent arms the *parent*, which does nothing for
  the child.
- **The `getppid() == 1` check is not optional.** Without it there is a race
  window in which the orphan lives forever.
- **It must be async-signal-safe.** A `pre_exec` closure runs between `fork`
  and `exec`; only async-signal-safe calls are sound there. Do not allocate,
  lock, or log inside it.

Keep the `Drop` guard as well. It is the tidy path, it reaps the child instead
of leaving it for `init`, and it keeps the death signal from firing during
normal teardown.

The shared helpers already do this: `common::die_with_parent` in
`crates/xdg-desktop-portal-atrium/tests/common/mod.rs`, and its counterpart in
`crates/atrium-portal-request/src/lib.rs`. **Use them; do not re-derive it.**

## Checklist for a new test fixture

When a test spawns anything long-lived (`dbus-daemon`, a compositor fake, a
portal backend, `pipewire`, `wireplumber`, `gst-launch-1.0`, `arca`):

- [ ] Call `die_with_parent(&mut command)` before `spawn`.
- [ ] Keep the child in a `Drop` guard so the normal path reaps it.
- [ ] Put the child's socket/directory under `tempfile::tempdir()` or a uniquely
      named temp directory — never a fixed path, never `$HOME`.
- [ ] Bind to ephemeral resources (`:0`, unique socket paths), never a fixed
      port or socket.
- [ ] If the fixture needs a system service, add a fast precondition probe and
      **skip with a reason** when it is unavailable (see below), instead of
      waiting out a timeout.

## Environment preconditions: skip, do not time out

A test that cannot possibly pass must say so in milliseconds and name the
reason. The probe exists because one real timeout is worse than a thousand
skips: it burns the whole budget and reports a failure that says nothing about
the code.

- `frontend_prerequisites(&bus)` — decides whether `xdg-desktop-portal` can
  claim its bus name at all. The frontend first activates `PermissionStore`
  and `Documents`; a `dbus-daemon` built with
  `--disable-traditional-activation` accepts those requests and never
  answers, so the frontend waits out its timeout and exits with "No document
  portal". The probe activates the same two services itself, bounded at five
  seconds: an error reply means a service is not installed, no reply means
  the bus cannot activate. The private bus runs with a sandboxed home and
  runtime directory, so activated services never touch the user's real
  permission store or `$XDG_RUNTIME_DIR/doc`.

It reports a skip normally and a hard failure when
`ATRIUM_PORTAL_REQUIRE_E2E` / `ATRIUM_PORTAL_REQUIRE_PIPEWIRE_E2E` is set, so CI
stays strict where the environment is controlled and local runs stay honest.

**Do not** turn a missing dependency into a silent pass, and do not let it
become a timeout. The three states must stay distinguishable in the output:
`pass`, `skip — <reason>`, `fail`.

## When a worker panics

Interface workers run under
`atrium_portal_request::Supervisor` (ADR-0028). If you add an interface:

```rust
let supervisor = Supervisor::<MyJob>::new("my interface");
let jobs = supervisor.sender();          // hand to the interface and `dispatch`

spawn_worker("atrium-portal-my-interface", move || {
    supervisor.serve(|rx| my_interface::my_worker(rx, Arc::clone(&tracker)))
})?;
```

The worker takes `&Receiver<MyJob>` and keeps its own loop, so its state
(concurrency counters and the like) lives across jobs. A panic costs the
in-flight request only; the next request is served by a restarted worker.

Never retry the panicking request inside the worker: its caller has already
been told it failed, and replaying a prompt or a capture can double-grant a
permission or publish two streams for one request.

## Verifying

The machine-level check is part of the release gate:

```sh
before=$(pgrep -x dbus-daemon | wc -l)
cargo nextest run --workspace
after=$(pgrep -x dbus-daemon | wc -l)
test "$before" -eq "$after" || echo "LEAKED $((after - before)) bus(es)"
```

Run it two or three times: a leak that only appears under a full parallel run
will not show up when a single test binary is executed alone.
