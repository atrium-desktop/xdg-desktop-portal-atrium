# Arca Chooser Contract

The `FileChooser` portal is rendered by [Arca](https://github.com/atrium-desktop/arca),
the desktop's file manager. The backend spawns one `arca --chooser-prompt`
process per request, writes one JSON request to its standard input, and reads
one JSON response from its standard output.

This page summarizes the contract; the authoritative definition is
`crates/atrium-arca-contract/src/lib.rs`, and ADR-0027 records why the contract
restarts its version at 1.

## Envelope

Both directions are one JSON document:

```json
{"version": 1, "request": { ... }}
```

```json
{"version": 1, "response": { ... }}
```

`version` is `atrium_arca_contract::ARCA_CONTRACT_VERSION` (currently `1`).
It versions the bytes on the two pipes and covers the FileChooser surface
only. A peer that does not recognize the version must refuse the exchange
rather than guess: the backend rejects a mismatched reply, and arca refuses a
mismatched request.

`scripts/version-consistency.sh` cross-checks this constant against the arca
repository and against the smoke-test payloads in
[Portal UI Testing](../dev/portal-ui-testing.md).

## Request

| Field | Type | Meaning |
|---|---|---|
| `mode` | `open_file` \| `open_directory` \| `save_file` \| `save_files` | Which chooser operation |
| `app_id` | string | The requesting application |
| `title` | string | Dialog heading |
| `accept_label` | string \| null | Overrides the affirmative button |
| `modal` | bool | Whether the dialog is modal |
| `parent_window` | string \| null | The frontend's parent-window handle |
| `multiple` | bool | Multi-selection for the open modes |
| `current_folder` | byte path \| null | Starting directory |
| `current_name` | string \| null | Pre-filled `SaveFile` name |
| `current_file` | byte path \| null | Pre-selected file |
| `filters` | filter array | Label plus typed `glob`/`mime` rules |
| `current_filter` | filter \| null | The selected filter |
| `choices` | choice array | Embedded controls (`(ssa(ss)s)` shape) |
| `files` | byte-path array | Suggested basenames for `save_files` |

### Paths are bytes

Every path is a JSON array of native Unix bytes (`BytePath`), never a string.
The FileChooser portal's path arguments are `ay` on the D-Bus surface, so a
filename that is not valid UTF-8 must survive the round trip unchanged.

## Response

| Status | Fields | Meaning |
|---|---|---|
| `selected` | `paths`, `current_filter`, `choices` | The user confirmed |
| `cancelled` | — | The user dismissed the dialog |
| `failed` | `message` | The chooser could not complete |

`selected` paths are absolute byte paths. `current_filter` echoes one of the
offered filters, and `choices` answers the offered controls in order.

### Validation

The backend refuses an answer its request did not authorize, with response
code 2:

- paths are absolute and NUL-free;
- the path count matches the mode (`save_file` and non-multiple opens take
  exactly one; `save_files` takes one per suggested basename);
- a returned filter was offered;
- answered choice values match the offered controls in order.

`save_files` is special: the chooser returns the chosen *folder* and the
contract's `finish_paths` derives the final paths, making each suggested
basename unique and avoiding names that already exist.

## Process Discipline

The backend, not the contract crate, owns supervision
(`crates/xdg-desktop-portal-atrium/src/companion/arca.rs`):

- one child per request; the dialog's lifetime is the request's lifetime, so
  there is no wall-clock timeout;
- `Request.Close` kills the child and answers code 1;
- a concurrency cap refuses excess dialogs with code 2;
- the child's `stderr` is forwarded live and its tail is folded into a
  failure message, so a loader refusal (exit 127) names the missing library
  in the D-Bus error.

Executable resolution order: `ATRIUM_FILE_CHOOSER_PROMPTER`, a sibling
`arca` next to the daemon, `PATH`, then `/usr/local/bin/arca` and
`/usr/bin/arca`.

## No other process contract

This is the repository's only out-of-process prompt. Every other prompt is a
compositor op reached directly over the wire projection; see
[ADR-0026](../../docs/adr/0026-prompter-process-retirement.md).
