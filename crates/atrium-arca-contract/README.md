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

`FileChooserRequest::finish_paths` applies `SaveFiles` semantics: suggested
basenames become unique children of the selected folder, avoiding names that
already exist and names already claimed within the same request.

## Version

`ARCA_CONTRACT_VERSION` is `1`. It covers FileChooser only, and
`scripts/version-consistency.sh` cross-checks it against the arca repository.
See [ADR-0027](../../docs/adr/0027-arca-chooser-contract-crate.md) for why the
legacy prompter envelope's version 6 was not carried over.

## Boundaries

`#![forbid(unsafe_code)]`; depends only on `serde` and `serde_json`. The crate
spawns nothing, resolves no executable, and holds no socket: supervision lives
in the daemon's `src/companion/arca.rs`.
