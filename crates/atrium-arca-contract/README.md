# atrium-arca-contract

The `arca --chooser-prompt` FileChooser stdio contract.

Arca is the file manager and renders the FileChooser dialog. The portal
backend writes one JSON request to arca's standard input and reads one JSON
response from its standard output; this crate is the shared definition of both
envelopes.

## Contents

| Type | Role |
|---|---|
| `ARCA_CONTRACT_VERSION` | Version of the bytes on the two pipes |
| `MAX_REQUEST_BYTES` | Cap on the request line arca reads: envelope plus trailing newline (1 MiB) |
| `FileChooserRequest` | The request envelope, with `validate()` |
| `FileChooserResponse` | `Selected` / `Cancelled` / `Failed`, with `validate_for()` |
| `FileChooserMode` | `OpenFile`, `OpenDirectory`, `SaveFile`, `SaveFiles` |
| `FileFilter`, `FilterRule`, `FilterRuleKind` | Portal `(sa(us))` filters |
| `Choice` | Portal `a(ssa(ss)s)` embedded controls |
| `BytePath` | A path as native Unix bytes |

## Paths are bytes

The FileChooser portal's path arguments are `ay` on the D-Bus surface, so a
filename that is not valid UTF-8 must still round-trip. `BytePath` carries the
native bytes and converts to `PathBuf` at the boundary; nothing in the contract
interprets a path as text.

## Validation travels with the contract

`FileChooserResponse::validate_for` is the one place that can prove a response
was authorised by its request: selected paths are absolute and match the
mode's cardinality, a returned filter was offered, and answered choice values
match the offered controls in order. The chooser is a fault boundary, not a
trusted source, so this check lives beside the types it checks.

For `SaveFiles`, Arca resolves each suggested basename into a unique child path
inside the user-selected folder and returns all `request.files.len()` final
paths; `validate_for` enforces exact path cardinality on the backend side.

## Version

`ARCA_CONTRACT_VERSION` is `1`. It covers FileChooser only: `scripts/version-consistency.sh`
verifies this repository's smoke-test payloads against it, and `arca` defines
the matching `ARCA_CONTRACT_VERSION = 1`. See
[ADR-0027](../../docs/adr/0027-arca-chooser-contract-crate.md) for why the
legacy prompter envelope's version 6 was retired.

## Boundaries

`#![forbid(unsafe_code)]`; depends only on `serde` (`serde_json` in tests). The
crate spawns nothing, resolves no executable, and holds no socket: supervision
lives in the daemon's `src/companion/arca.rs`.
