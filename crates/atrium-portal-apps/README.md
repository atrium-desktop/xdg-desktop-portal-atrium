# atrium-portal-apps

Freedesktop Desktop Entry and MIME Apps resolution for the atrium portal.

The crate answers the three questions the AppChooser, OpenURI, and
DynamicLauncher interfaces ask about local applications:

- which applications declare a content type,
- which one is the user's default for it, and
- how to launch one by expanding the `Exec` field codes.

Everything is hand-rolled against the Desktop Entry and MIME Apps
specifications so the portal stays free of GLib.

## Layout

| Module | Responsibility |
|---|---|
| `desktop` | `.desktop` parsing, the `mimeinfo.cache` and `globs2` indexes |
| `mimeapps` | `mimeapps.list` Added/Removed/Default precedence and writes |
| `exec` | `Exec` word splitting, field-code expansion, detached launch |

`AppDirs` stitches the three together and owns the XDG directory set.

## Boundaries

`.desktop` and association files are scanned from `$XDG_DATA_HOME` and each
`$XDG_DATA_DIRS` entry; nearer directories shadow farther ones by desktop id.
Writes are limited to `$XDG_CONFIG_HOME/mimeapps.list` and are atomic (temp
file plus rename). Inputs are bounded: files past a fixed byte cap are skipped,
candidate lists are truncated to a screenful, and desktop ids must be plain
file names.

The crate performs no input/output beyond those files, holds no `unsafe`
(`#![forbid(unsafe_code)]`), and has no dependencies. `AppDirs::fixture`
builds a resolution tree under a caller-supplied root so tests never read the
host system.

## Scope

The portal's owner is the daemon in `crates/xdg-desktop-portal-atrium`; this
crate is a `publish = false` workspace member and is not a stable interface.
See [ADR-0025](../../docs/adr/0025-workspace-crate-topology.md).
