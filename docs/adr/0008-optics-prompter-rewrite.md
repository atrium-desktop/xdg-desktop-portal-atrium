# ADR-0008: Prompter dialogs on the optics (iris/lens) stack

- Status: Superseded by [ADR-0021](0021-headless-portal-and-optics-retirement.md)
- Date: 2026-08-10
- Complements: [ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md)

This ADR is written after the fact. The rewrite it records shipped in
`v0.0.7` (see the [changelog](../../CHANGELOG.md)); this document exists
so the decision and its boundaries have the same durable record as the
other architectural decisions. Nothing in it changes any accepted ADR.

## Context

ADR-0004 established the one-shot prompter process boundary: all Portal UI
that owns no compositor resource runs in a supervised
`aegis-portal-prompter` child speaking a versioned stdin/stdout contract.
The initial implementation (`v0.0.2`–`v0.0.6`) rendered those dialogs with
GTK4.

GTK4 was a foreign toolkit in an optics desktop. Its dialogs could not
follow the aegis design language or track the system light/dark preference
consistently, and every build carried a second toolkit's development
files, theming machinery, and release cadence for three dialogs. The
optics stack (iris/lens) — the desktop's own UI stack, resolved from the
tagged `ming2k/optics` release — had become available to render the same
surfaces.

## Decision

The prompter dialogs are implemented on the optics (iris/lens) stack
instead of GTK4, styled after the aegis design language and following the
system light/dark preference.

The process boundary from ADR-0004 is unchanged: FileChooser, Account
confirmation, and Secret password requests keep the same versioned
stdin/stdout process contract, and the rewrite does not change the
contract version.

The build no longer requires GTK 4 development files. It requires the
flux, lens, and iris C libraries from the tagged `ming2k/optics` release,
with all tagged optics dependencies sharing one release tag.

Prompts map as independent windows because iris cannot yet import an
exported `wayland:` parent handle. The contract's `parent_window` field
still crosses the pipe, so parenting can return without a contract change
once the optics stack supports it.

## Alternatives

- **Keep the GTK4 prompter.** Rejected because it keeps a second UI
  toolkit in the stack permanently — its development files, theming, and
  release cadence — for dialogs the desktop's own stack now renders in the
  aegis design language.
- **Render the prompts in compositor chrome.** Rejected by ADR-0004:
  these surfaces own no compositor resource, and routing them through the
  compositor widens the runtime authority boundary.
- **Gate the switch on parent-handle import in iris.** Rejected because
  independent prompt windows are an acceptable degradation; the contract
  already carries `parent_window`, so no wire change is needed when
  parenting arrives.

## Consequences

- Portal builds depend on the optics C libraries (flux, lens, iris) from
  the tagged `ming2k/optics` release; GTK 4 development files are no
  longer required anywhere in the workspace.
- Prompt windows are independent toplevels rather than children of the
  requesting window until iris grows exported-handle import.
- The rewrite left the process contract wire-compatible, so backend and
  prompter releases did not need lockstep for the toolkit change itself;
  later contract versions (3, 4) extended the same prompter with new
  prompt kinds — the application chooser, the launcher editor, and the
  confirmation deny label — and with the notification daemon mode that
  ADR-0007's full-stack ownership builds on.
- Joint development against a sibling optics checkout uses the opt-in
  local override (`.cargo/optics-local.toml`) without making the checkout
  mandatory for CI; the prompter binary re-emits the `-sys` crates' rpath
  metadata so it finds the chosen optics libraries at runtime.
