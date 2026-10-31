# ADR-0026: Prompter process retirement and companion prompt ownership

- Status: Accepted
- Date: 2026-10-31

## Context

ADR-0021 made the portal headless and retired optics, but it kept the
prompter's *process* contract alive: `atrium-portal-prompter` was preserved as
"a lightweight, headless data-contract library", and `src/prompter.rs` kept
spawning a process named `atrium-portal-prompter` to serve it. Its stated
reason for keeping the crate was compatibility — "preserves backwards
compatibility for existing crate imports, test fixtures, and the version
consistency verification suite".

That decision left the tree in a state where the advertised behaviour and the
built artefact disagree. Verified against the current tree:

- `crates/atrium-portal-prompter/Cargo.toml` declares no `[[bin]]`. The crate
  has no entry point, so `cargo build -p atrium-portal-prompter` produces no
  executable.
- `src/prompter.rs` still resolves an executable (`ATRIUM_PORTAL_PROMPTER`,
  a sibling `atrium-portal-prompter`, `/usr/libexec/...`, `/usr/lib/...`) and
  fails every request with "atrium-portal-prompter was not found" when it is
  absent.
- `src/notification.rs` spawns that same executable with
  `--notification-daemon` on the first `AddNotification`.
- `README.md` and `docs/how-to/install-production.md` still instruct packagers
  to build `-p atrium-portal-prompter`.

So on a conforming install every consent dialog (Account, Access, Background,
Wallpaper), the application chooser (AppChooser, OpenURI), the launcher editor
(DynamicLauncher), the interactive ScreenCast source choice, and the entire
Notification portal resolve to a missing-binary error. Only FileChooser works,
because it resolves `arca --chooser-prompt` on a separate path.

Independently, the companion components grew the prompts that the retired
process was still being asked to render. Verified against the sibling
repositories:

- **Tessera** implements `PickTarget`, `PickConfirm`, `PickApp`, and
  `PromptSecret` as first-class, capability-gated interactive ops (protocol
  versions 6, 16, 14, and 15 respectively). `PromptSecret` even carries a
  `ResourceGrantId`.
- **Tessera** owns notifications natively: `Command::Notify` with an
  `external_id` field documented as "the Notification portal's per-application
  id, so an external withdrawal can be matched back to this entry", plus
  `Request::GetNotifications` and `Command::DismissNotification`.
- **arca** implements `--chooser-prompt` and is already the FileChooser
  backend.
- **sigil** owns the Secret vault and its unlock prompt (ADR-0020).

The legacy process contract is therefore a second implementation of dialogs
the desktop already renders, reachable only on a fallback path that has no
binary behind it.

## Decision

### 1. The prompter process contract is deleted, not kept for compatibility

`atrium-portal-prompter` is dissolved. `src/prompter.rs` (process
supervision) and `src/notification.rs`'s daemon transport are deleted.
Nothing in this repository spawns a `--chooser-prompt`-style child except
arca.

The "compatibility" that ADR-0021 preserved has no holder: the crate is
`publish = false`, the repository is the only consumer, and the interfaces it
was "compatible" with are being re-owned in this ADR. A compatibility argument
requires a party whose compatibility is at stake; there is none.

### 2. Every interactive prompt has exactly one owning component

| Portal surface | Owner | Mechanism |
|---|---|---|
| FileChooser | arca | `arca --chooser-prompt` (JSON over stdio) |
| AppChooser, OpenURI | Tessera | `PickApp` → `AppPickResult` |
| Account, Access, Background, Wallpaper consent | Tessera | `PickConfirm` → `ConfirmPickResult` |
| ScreenCast source/target selection | Tessera | `PickTarget` → `PickResult` |
| Secret unlock | sigil | sigil's own prompt (ADR-0020) |
| Notification | Tessera | `Command::Notify` / `DismissNotification` with `external_id` |
| DynamicLauncher | — (retired) | see §3 |

The backend grows no dialogs of its own. A prompt the desktop cannot render is
a prompt the portal does not advertise.

### 3. DynamicLauncher is retired from the advertised interface set

`PrepareInstall` is not a picker or a consent question: it is a form (a
pre-filled, editable launcher name validated against the request, plus an
echoed icon) and it materialises a `.desktop` file. No companion owns a form
surface, and the Tessera `PickConfirm` documentation names `DynamicLauncher`
only as a *consent* caller, which the portal can still reach for its
`request_install`-style confirmation, not for the edit.

Rather than keep a form-shaped prompt alive on a component that does not exist,
the interface is removed from `contrib/xdg-desktop-portal/portals/atrium.portal`
and its implementation is deleted. A future launcher editor belongs to the
component that owns launcher installation, and can be reintroduced with its own
ADR and a real owner. Advertising an interface whose only implementation
returns "prompter not found" is worse than not advertising it: the frontend
falls back to another backend or reports a truthful capability gap.

### 4. The wire projection grows only what these owners need

Three ops are added to the projection. Each is gated at the handshake on the
version the compositor actually negotiated, so a pre-feature peer is never
sent an op it cannot parse:

| Op | Introduced in | Capability | Gate constant |
|---|---|---|---|
| `PickApp` → `AppPicked` | protocol 14 | `control` + scoped `PickApp` | `ipc::PICK_APP_PROTOCOL` |
| `PickConfirm` → `ConfirmPicked` | protocol 16 | `control` + scoped `PickConfirm` | `ipc::PICK_CONFIRM_PROTOCOL` |
| `Do { Command::Notify }` / `DismissNotification` | predates the floor | `control` + scoped `Notify` | none (always available) |

The gate is enforced twice: the backend checks the negotiated version before
sending (`ipc::request_consent`, `ipc::request_app_choice`), and the
independent test server refuses a post-gate op on an older connection the way a
real peer would — as an unknown request variant. Three tests pin the behaviour:
a protocol-15 peer cannot be asked for consent, a protocol-13 peer cannot be
asked to pick an application, and a protocol-16 peer serves both.

`PROTOCOL_VERSION` stays **32** and the floor stays **24**. Neither moves: the
projection's newest op was already protocol 32, and the *capability* question
is answered by the per-op gate rather than by the headline number. A compositor
negotiating to 24 is a supported configuration that serves settings, capture,
selection, wallpaper, and notifications, and answers 2 for the two prompts it
predates.

`PromptSecret` is **not** projected: `atrium-portal-secret` reaches the vault
through sigil's socket (ADR-0020), the portal scope is not granted
`PromptSecret`, and Tessera deliberately withholds it from production clients
(Tessera ADR-0112). Adding an op no caller may invoke would be dead surface.

### 5. Failure is a truthful portal response, never a silent hang

When the negotiated protocol predates an owner's op, or the compositor refuses
or is unreachable, the interface answers with the portal's failure code (2) and
logs the reason. It does not fall back to a process that does not exist, and it
does not answer success for a prompt the user never saw.

### 6. The capability losses this accepts, stated plainly

Distributing prompts to owners that render *less* than the retired prompter
retires some behaviour. Each loss is a deliberate trade, not an oversight, and
each is recorded here so a future reader knows it was considered:

- **Notification actions are gone.** The backend no longer emits
  `ActionInvoked`. The compositor queue stores summary, body, app id, and the
  caller's `external_id`; it renders no buttons and reports no action back, so
  the signal — which the D-Bus interface requires the portal to emit when an
  action is invoked — can never fire truthfully. Emitting it with a fabricated
  payload would be worse than not emitting it.
- **`RemoveNotification` cannot withdraw the card.** The queue addresses an
  entry by its compositor-assigned `u64` id, and the portal scope carries
  `Notify` and `DismissNotification` but **not** `ObserveNotifications`, so
  `Command::Notify` returns no id and the backend can never learn it. The portal
  stops tracking the id (so the spec's per-app cap behaves) and logs that the
  card stays until the compositor's retention window elapses.
  **Feasible remedy, not adopted here:** `Request::GetNotifications` is gated
  by `ObserveNotifications`, and an observation token would let the backend
  match `external_id` back to the compositor id. That is a wire-projection
  addition plus a scope change in Tessera, so it belongs in its own ADR; it is
  not a reason to keep the legacy process alive.
- **AppChooser loses embedded controls and the default-application write.** The
  compositor picker renders a candidate list and nothing else, so the "Remember
  this choice" checkbox is not shown, and the `mimeapps.list` default that
  checkbox drove is no longer written. `last_choice` is still forwarded so the
  picker can pre-highlight the row. The frontend remains free to apply its own
  default.
- **Consent loses the deny label and the modality hint.** `PickConfirm` carries
  one affirmative label; dismiss is the deny path, and the compositor owns
  modality for its own chrome. Both hints are accepted and logged rather than
  rejected, so a frontend that sends them is not failed over cosmetics.

## Alternatives

- **Ship a single `atrium-portal-prompter` binary that dispatches to arca and
  the compositor.** Rejected: it recreates a process boundary, a supervision
  thread, and a contract library to reach two mechanisms Rust can call
  directly (a Unix socket and a child's stdio). It also re-centralises the
  prompt ownership this ADR distributes, and would need its own version gate
  to stay in step with `PickApp`/`PickConfirm`.
- **Keep the DataLauncher editor as a headless D-Bus-only echo (accept the
  request, return the requested name unchanged).** Rejected: the portal
  contract has the user *review and confirm* the installed name; echoing the
  input would report an install the user never approved.
- **Keep `atrium-portal-prompter`'s types as the portal's own request model,
  even with no process.** Rejected: the types are shaped around a JSON wire
  (byte-array paths, a `version` envelope, `deny_unknown_fields`) that ceases
  to exist. Keeping them would be a wire format with no wire, and
  `atrium-portal-apps` plus the projection's schema types already serve the
  in-process need.
- **Gate on `PROTOCOL_VERSION` instead of a per-op floor.** Rejected: a
  compositor negotiating down to 24 is a supported configuration, so the
  binary must degrade to "this prompt is unavailable here", not refuse to
  start.

## Consequences

- Consent, application choice, source choice, notifications, and Secret unlock
  work on a conforming install — the current tree does not.
- `README.md`, `docs/how-to/install-production.md`,
  `docs/dev/portal-ui-testing.md`, `docs/reference/prompter-process-contract.md`,
  `scripts/version-consistency.sh`, and `contrib/` lose their prompter
  references; `docs/reference/prompter-process-contract.md` is deleted.
- `DynamicLauncher` disappears from the advertised interface list, which is a
  user-visible capability change and must be called out in `CHANGELOG.md`.
- The FileChooser path keeps `atrium-arca-contract` as the only remaining
  process contract in the repository — one contract, one owner, one version
  constant.
- Tessera support for protocol 14/15/16 becomes an effective runtime
  requirement for the affected prompts; older compositors get a documented
  failure instead of a missing binary.
