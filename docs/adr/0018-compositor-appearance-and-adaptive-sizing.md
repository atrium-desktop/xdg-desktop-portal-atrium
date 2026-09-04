# ADR-0018: Compositor-owned appearance, adaptive dialog sizing

- Status: Superseded by [ADR-0021](0021-headless-portal-and-optics-retirement.md)
- Date: 2026-08-20

## Context

Two long-standing gaps in the prompter's visual contract traced to the
same root cause: the prompter process had **no channel to the
compositor's desktop preferences**, and every dialog's window geometry
was a **hard-coded tuple at its `run()` call site**.

1. *Appearance.* The backend already holds the compositor's
   `DesktopPreferences` (Aegis IPC `GetSettings`, protocol 24+) in a
   `SettingsStore` and projects it to the standardized
   `org.freedesktop.appearance` D-Bus keys. The prompter, however,
   resolved dark mode through `iris::system_prefers_dark()` — a
   gsettings/GNOME query that never consults Aegis. On an Aegis session
   where `color_scheme` is published only over `aegis.sock`, the portal's
   own dialogs could show the wrong scheme while every GTK app on the
   desktop followed the compositor. Accent colour, contrast, and reduced
   motion never reached the dialogs at all.

2. *Sizing.* The permission-style dialogs (confirm, secret, launcher
   editor) used fixed windows — 460×220, 440×240, 460×240 — sized for
   English one-liners. A short consent body left the window half empty;
   a long localized body wrapped early or clipped. Meanwhile the file
   chooser kept its large default with no minimum, so a user could
   resize it below its layout.

The same round asked for the dialogs to follow the desktop's material
language (Apple's "liquid glass" as seen in Finder: translucent elevated
chrome over the content plane). The optics stack has a real liquid-glass
material — `prism` — but it is a **compositor-side** effect: the
compositor applies it only to its own chrome components
(`Chrome::liquid_glass_regions`); client toplevels are composited
opaque-premultiplied with no backdrop blur, and iris clears every frame
to an opaque theme colour. True backdrop glass for portal dialogs would
require a compositor protocol change (per-window blur/glass requests),
which is outside this repository's boundary (ADR-0004).

## Decision

- **Contract v6 adds an `appearance` snapshot to every
  `PrompterRequest`** (and the notification stream grows to v2 with a
  `set_appearance` command). The snapshot is a deliberate local
  projection — `color_scheme`, `accent_color`, `high_contrast`,
  `reduced_motion` — so the prompter crate stays independent of
  `aegis-portal-ipc`. The backend stamps it in one place
  (`prompter::invoke`) from its `SettingsStore`, which now threads into
  every prompt-owning worker. An explicit scheme beats the platform
  query; `system` defers to it; the accent restyles the selection wash;
  high contrast deepens text and strokes; reduced motion reaches lens.

- **Window sizing is measured, not hard-coded.** A new `ui::sizing`
  module measures the actual title/body text through the same headless
  lens context the dialogs render with (falling back to the monospace
  estimator where no font backend exists) and picks the smallest window
  that fits without wrapping, clamped to design-token bounds
  (`MIN_WINDOW_W`/`MAX_TEXT_WINDOW_W`/`MAX_TEXT_WINDOW_H`). The
  confirm, secret, and launcher-editor dialogs are effectively
  content-fixed (their minimum equals their initial size, via
  `iris_window_set_min_size`); the chooser dialogs get explicit
  minima that keep their layouts usable.

- **The Finder read is translated into what the stack can honestly
  draw:** in-window material layering. The file chooser's places rail
  and preview plate render as translucent washes (`card_surface`/
  `popover`-mirrored tokens) with hairlines and popover/card radii over
  the opaque content plane, and the overwrite modal dims with the
  design system's scrim colour instead of lens's default black.
  Per-window backdrop blur is recorded as future compositor work, not
  simulated: a client-side fake (blurring the window's own pixels)
  cannot refract what is behind it and would read as noise.

## Alternatives

- **Have the prompter read `aegis.sock` itself.** Rejected: the
  prompter owns no compositor connection by design (ADR-0004), and the
  backend's store is the single authoritative cache with change
  watching already implemented.
- **Resolve appearance through the public Settings portal D-Bus
  interface.** Rejected: the prompter would become a D-Bus client of
  the frontend built on this very backend — a cycle — and the
  one-shot process model makes per-request stamping cheaper than a
  bus round trip.
- **Measure with a character-count heuristic.** Rejected: it breaks
  with the first non-Latin locale; the headless measurement reuses the
  real text pipeline at no new dependency cost.
- **Simulate backdrop blur client-side.** Rejected as dishonest glass:
  without the pixels behind the window there is nothing to refract.

## Consequences

- The process contract version and notification stream version both
  bump, so backend and prompter must be deployed together (the version
  check fails closed on mismatch, as before).
- `prompter::invoke` gains a settings parameter; every call site
  passes it, so no future prompt kind can silently miss the appearance.
- The notification daemon is re-skinned live when preferences change
  (the settings watcher pushes `set_appearance` to the shared daemon
  manager); one-shot dialogs pick the snapshot up at spawn, which is
  their lifetime.
- The style module's bool-based atoms are retired in favour of
  palette-driven ones (`*_for(&Palette)`), so high contrast and accent
  overrides flow to every surface that renders text or buttons.
- Real liquid glass for portal windows remains open compositor work:
  a per-toplevel blur/material request protocol. When it lands, the
  material layering added here composes with it (the translucent bands
  already read correctly over any backdrop).
