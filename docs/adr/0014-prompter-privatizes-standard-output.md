# ADR-0014: The Prompter Privatizes Its Standard Output

- Status: Superseded by [ADR-0021](0021-headless-portal-and-optics-retirement.md)
- Date: 2026-08-16

## Context

[ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md) established
the one-shot prompter as a supervised child speaking a versioned JSON
contract over stdin/stdout, and
[ADR-0008](0008-optics-prompter-rewrite.md) moved its dialogs onto the
optics (iris/lens) C stack without changing that boundary. The contract
assumes the child's stdout carries exactly one JSON document — but the
optics libraries share the process and its stdio. iris printed a one-line
"first frame presented" diagnostic to stdout, and C stdio is fully
buffered on pipes, so those bytes stayed in the C buffer until process
exit and landed *after* the Rust-written JSON response. The backend reads
the pipe to EOF and parses strictly, so every prompter response — open,
save, password, or chooser — failed with "trailing characters", surfacing
to applications as a hard portal failure: a browser's download-location
prompt returned nothing, and no download ever started.

The class of bug matters more than the instance: any current or future C
dependency can write to stdout, and the failure stays silent until the
strict parse trips at the worst possible moment.

## Decision

Two layers, neither sufficient alone:

1. **Library diagnostics never go to stdout.** In optics, every iris
   platform backend (Wayland, Cocoa, Win32) sends its diagnostics to
   stderr, matching the convention flux's `flux_console_logger` already
   documents. This removes the known writers.
2. **The prompter claims the wire.** At startup, before any library code
   runs, the prompter duplicates fd 1 into a private descriptor
   (`Wire::acquire` in `crates/aegis-portal-prompter/src/wire.rs`) and
   re-aliases fd 1 onto fd 2 for the rest of the process lifetime. Both
   process contracts — the one-shot response and the notification event
   stream — write through the private descriptor only. Any later write to
   stdout, a C `printf` or a Rust `println!`, lands on the journal as a
   diagnostic instead of corrupting the wire.

The backend keeps its strict whole-buffer parse: a corrupted wire must
fail loudly, not be papered over.

## Alternatives

- **Fix the one `printf` and move on.** Leaves the class open: the next
  library, driver, or vendored dependency that prints to stdout reopens
  it, again with no signal until parse time.
- **Tolerant parse (first line wins).** Turns real corruption — a torn
  write, two responses — into silently wrong portal results. The strict
  parse is what surfaced this bug; weakening it trades a loud failure for
  invisible ones.
- **A dedicated passed fd for the response.** A third channel beyond the
  stdin/stdout contract complicates the spawn protocol for no gain over
  privatizing stdout in place.

## Consequences

- The optics change is confined to the iris platform backends
  (`app_wayland.c`, `app_cocoa.m`, `app_win32.c`); no API change, and no
  behavior change for consumers whose stdout is a terminal.
- `Wire::acquire` runs first in `main()` for both one-shot and
  `--notification-daemon` modes; on `dup`/`dup2` failure it degrades to
  the old shared-stdout behavior with a stderr note rather than aborting
  startup.
- Rust `println!` in the prompter now lands on the journal; the binary
  has no legitimate stdout writes outside the protocol writers.
- Manual testing from a terminal is unaffected: the wire is still the
  terminal's stdout, and diagnostics share the screen via stderr.
