# ADR-0027: The arca FileChooser contract crate

- Status: Accepted
- Date: 2026-10-31

## Context

ADR-0026 retires the prompter process. One out-of-process prompt survives, and
it is owned by another application: `arca --chooser-prompt`. Arca is the file
manager, it already implements the mode, and its own ADR record describes it as
"standalone prompter process invoked by desktop portal daemons over stdio;
reads a JSON request envelope from stdin, displays a focused modal picker
window, emits a single JSON response envelope to stdout, and terminates".

After ADR-0026 that contract is the only process contract in this repository,
and the file chooser needs from it: the request envelope (title, mode,
filters, choices, multiple/directory flags, byte-array paths), the response
envelope (selected paths or cancel), and a validation pass that refuses a
response the request did not authorise before it becomes a portal result.

Two things argue for a crate rather than a module:

- **It is a wire contract with a version constant**, verified by
  `scripts/version-consistency.sh` against literal fixtures and by the arca
  repository's own acceptance tests. A module would put it back inside a
  15k-line binary that links PipeWire.
- **The portal's own tests must speak it without linking the daemon.** The
  integration harness drives a fixture prompter over the real contract.

Two things argue against reusing `atrium-portal-prompter`'s name or contents:

- Everything that is not FileChooser — `ConfirmRequest`, `ChooseAppRequest`,
  `ChooseSourceRequest`, `LauncherEditRequest`, `SecretRequest`, the
  `PrompterRequest` envelope, `PromptAppearance`, and the entire
  `notify` module — has no process left to speak to it (ADR-0026).
- `PROCESS_CONTRACT_VERSION` is currently 6 and versions the *whole* legacy
  envelope. Arca implements the FileChooser subset. Keeping version 6 as the
  arca contract version would claim arca supports seven prompt kinds it does
  not, and would make the version number unreadable: it would move when
  unrelated kinds change.

## Decision

### 1. `atrium-arca-contract` replaces `atrium-portal-prompter`

The crate is named for the boundary it describes, not for the daemon that once
used it. Its single responsibility is the arca FileChooser stdio contract.

```
crates/atrium-arca-contract/
├── src/lib.rs        # envelope, request, response, validation, version
└── README.md
```

Contents, and nothing else:

- `ARCA_CONTRACT_VERSION` (see §2) and the envelope that carries it.
- `FileChooserRequest`: `app_id`, `title`, `parent_window`, `mode`,
  `multiple`, `directory`, `filters`, `current_filter`, `current_folder`,
  `choices`.
- `FileChooserResponse`: `Selected { paths }` and `Cancelled`, plus
  `validate_for(&request)`.
- `FileChooserMode`, `FilterRule`, `FilterRuleKind`, `FileFilter`, `Choice`,
  and `BytePath`.

`BytePath` is kept for the reason it exists: the FileChooser portal's paths are
`ay` on the D-Bus surface, so a non-UTF-8 filename must round-trip the stdio
contract unchanged. It is the one type in the crate the portal converts to and
from `PathBuf` at the boundary and never inspects as text.

### 2. The contract version restarts at 1 and covers FileChooser only

A format version identifies one thing: the shape of the bytes on the two
pipes. The FileChooser bytes are currently produced with
`PROCESS_CONTRACT_VERSION = 6` only because that constant was shared with six
prompt kinds that no longer exist. Restarting at 1 with the file-chooser-only
meaning is a clean break: nothing deployed can be speaking version 6 to arca,
because the binary that produced 6 was never built after ADR-0021.

`ARCA_CONTRACT_VERSION` and the arca repository's own version constant are
cross-checked by `scripts/version-consistency.sh`, which currently reads the
legacy path and the legacy constant name.

### 3. The crate owns no behaviour beyond validation

It does not spawn a process, resolve an executable, or hold a socket. Executable
resolution and child supervision stay in the daemon
(`src/companion/arca.rs`), because only the daemon knows its own install
layout, its cancellation closure, and its concurrency caps. The contract crate
is `#![forbid(unsafe_code)]` and depends only on `serde` and `serde_json`.

Validation stays in the crate, not in the daemon, for the reason it does today:
`validate_for` is the one place that can prove a response was authorised by its
request (absolute paths, cardinality for the mode, a returned filter that was
offered, a chosen option that was offered), and that proof must travel with
the contract wherever the contract is used.

## Alternatives

- **Keep the types in `atrium-portal-prompter` and delete only the unused
  kinds.** Rejected: the crate name would then describe a component that no
  longer exists, and every future reader would look for the prompter binary.
  Renaming is cheap here precisely because the crate is `publish = false`.
- **Put the contract in `atrium-portal-apps`.** Rejected: the two are
  different boundaries. `atrium-portal-apps` is in-process resolution of local
  desktop files with no version constant; this is a versioned inter-process
  wire contract. Merging them would put a process format in a crate whose tests
  are pure functions.
- **No crate: a `file_chooser` module inside the daemon.** Rejected: the
  portal's integration tests need to speak the contract, and the daemon links
  the native PipeWire library and is not usable as a test-only dependency.
- **Bump the shared envelope to version 7.** Rejected: there is no shared
  envelope left to version, and continuing to bump a constant that describes
  one prompt would misreport what changed.

## Consequences

- One process contract, one version constant, one owner.
- `scripts/version-consistency.sh` reads `crates/atrium-arca-contract/src/lib.rs`
  and `ARCA_CONTRACT_VERSION`, so the crate rename is a coordinated change with
  the arca repository's version check.
- The daemon's FileChooser path imports `atrium_arca_contract` and keeps its
  supervision, cancellation, and cap logic where the daemon can test it.
- `docs/reference/prompter-process-contract.md` is replaced by
  `docs/reference/arca-chooser-contract.md`, describing the file-chooser-only
  contract and its version.
