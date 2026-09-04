# ADR-0017: The FileChooser previews images in the prompter

- Status: Superseded by [ADR-0021](0021-headless-portal-and-optics-retirement.md)
- Date: 2026-08-19

## Context

The FileChooser dialog is the portal's most-visited surface, and picking a
photo is its most common job: `image/*` filters dominate what applications
ask for. Every mainstream file chooser (GTK, KDE, macOS, Windows) previews
the image under the cursor because looking at a picture is the only way to
tell photos apart. The optics rewrite (ADR-0008) left the prompter with a
text-only listing.

Until now the prompter deliberately decoded nothing. Three code comments
(`lib.rs` on `LauncherEditRequest`, `ui/launcher_edit.rs`,
`ui/choose_app.rs`) recorded the rationale — "the dialog cannot decode
arbitrary image bytes into a lens texture" — while the launcher and app
chooser still render icons as plain labels. Those rationales conflated two
things the stack actually distinguishes:

- **Drawing a raster is a supported host path.** `lens_image` borrows a
  host-owned `flux_image` for the frame, and `flux_image_create` uploads
  tightly packed pixel bytes. The lens binding documents the exact
  contract; the prompter simply never used it.
- **Decoding encoded files was the missing piece** — no decoder crate was
  in the dependency graph, and reaching the device required the iris
  lifecycle callbacks the prompter never registered.

So the gap was an implementation choice, not a framework limit, and the
chooser's users paid for it every time they picked between `IMG_2043.jpg`
and `IMG_2044.jpg`.

## Decision

The FileChooser shows a preview pane to the right of the directory listing
for the file under the listing cursor, when that file's format has a
mature, cheap decode: PNG, JPEG, GIF (first frame), WebP, and BMP.
Non-previewable targets collapse the pane entirely — browsing text files
and folder picking keep the full width — and windows narrower than
`PREVIEW_MIN_WINDOW_W` hide the pane too. The pane is presentation only:
it never changes the selection, the accept result, or the wire contract
(`PROCESS_CONTRACT_VERSION` stays at 5; nothing crosses the pipe).

The prompter decodes in its own process, on a worker thread, under strict
caps:

- **Format gate by extension** (`preview_format`): the cheap check runs
  before any I/O; unsupported types never open the file.
- **Decode caps**: 16,382 px per edge (the `image` crate's strict limit),
  96 MiB decoder allocation cap, 64 MiB file size cap, checked before the
  reader opens.
- **Downsample before upload**: at most 672 px per texture edge (the pane
  is 224 logical px; 3× HiDPI stays crisp), aspect preserved, Exif
  orientation applied first. The upload is premultiplied
  `FLUX_FORMAT_RGBA8_UNORM`, matching the canvas image pipeline's
  sampling contract (`canvas_image.frag` decodes gamma-encoded 8-bit RGBA
  as premultiplied sRGB).
- **One decode per file**: results are keyed by path and mtime; stale or
  superseded decodes are dropped, and a bounded LRU (24 textures) keeps
  revisit-to-a-folder instant without unbounded GPU memory.

The device question is settled by borrowing, not opening: the dialogs run
through `run_window_with_lifecycle`, which registers iris's
`run_with_lifecycle` start/stop hooks, captures `StartHost::device()` as a
non-owning `DevicePtr`, and releases every texture in `stop` before iris
destroys the device (ADR-0045's deterministic release point). The prompter
never constructs a `flux_device` of its own; all texture FFI lives in the
`ui/mod.rs` unsafe island the crate already funnels raw calls through.

The decoder is the `image` crate with **only** the raster features enabled
(`png`, `jpeg`, `gif`, `webp`, `bmp`) — never `default-features`, which
would drag in rayon, ravif, exr, and the AVIF/TIFF stacks. All added
crates (image and its codec dependencies) are MIT/Apache-2.0-licensed
crates.io packages inside the `cargo-deny` allow-list, declared once in
the workspace dependency table per the repository's pinning policy.

## Alternatives

- **Keep the dialog text-only.** Rejected: picking photos blind is the
  chooser's worst daily friction, and every peer file chooser previews.
- **Decode in the backend and ship pixels across the pipe.** Rejected:
  it would grow the process contract (version bump, lockstep releases)
  for a purely presentational feature, and the prompter already owns the
  user-facing surface.
- **Ask the compositor to decode (like wallpaper does).** Rejected: the
  wallpaper split exists because the compositor already owns wallpaper
  rendering. A file preview is dialog chrome; routing it through
  compositor IPC would add protocol for no authority reason.
- **Full `image` default features.** Rejected: the decode surface should
  stay as small as the formats users actually preview.
- **Thumbnails through a shared cache (freedesktop `thumbnailer`).**
  Deferred: the freedesktop thumbnail cache is a compatibility win for
  huge folders, but it adds a second code path and an XDG spec's worth of
  naming rules. The in-memory LRU covers a dialog session's lifetime,
  which is the prompter's actual scope; the cache hook can come later
  without touching this decision's shape.

## Consequences

- The prompter's first image-decode dependency widens its supply-chain
  surface by ten crates (image, png, zune-jpeg, zune-core, gif, weezl,
  color_quant, image-webp, byteorder-lite, bytemuck, num-traits, and
  small helpers); `cargo-deny` and the committed lockfile govern them
  like every other dependency.
- The stale "cannot decode image bytes" rationales in `lib.rs`,
  `ui/launcher_edit.rs`, and `ui/choose_app.rs` are rescoped: the
  *launcher/app-chooser* statements remain true (those dialogs still
  render labels by design — their icons are contract fields, not files),
  but the *general* claim is gone.
- Dialogs that want textures now have the lifecycle wrapper and the
  `DevicePtr`/`TextureHandle` seam in `ui/mod.rs`; the launcher editor's
  icon label can adopt the same path in a later release without another
  architectural change.
- Decoding runs off the UI thread, so a slow or adversarial image costs
  a worker thread and a capped allocation, never frame stalls; a failed
  decode degrades to a quiet textual reason in the pane.
- The preselected file (`current_file`) now also takes the listing
  cursor, so the preview appears on the very first frame — a small UX
  correction this feature exposed.
