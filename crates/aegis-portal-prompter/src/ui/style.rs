//! The aegis product look, mirrored locally: palette, theme factory, layout
//! metrics, and the shared style atoms for prompt surfaces. The portal build
//! graph stays independent of the Aegis repository, so the token *values*
//! are duplicated here instead of imported from `aegis-design`.
//!
//! Every dimension a dialog sets must come from [`metrics`]: heights,
//! widths, spacing, radii, font sizes, and icon sizes are design tokens, not
//! call-site literals, so controls stay on one rhythm across the dialogs.
//!
//! ## Appearance resolution
//!
//! [`ThemeInput::resolve`] turns the backend's compositor appearance
//! snapshot (contract v6) plus the platform colour-scheme query into the
//! [`ThemeInput`] every surface renders with: the resolved scheme, the
//! accent override, and the accessibility flags. A missing snapshot falls
//! back to `iris::system_prefers_dark()` — the portal backend may have had
//! no compositor IPC when the request was composed.

use aegis_portal_prompter::PromptAppearance;
use lens::{Color, Style, Theme};

/// The spatial and typographic tokens every prompt surface shares.
///
/// Spacing follows a 4 px grid; control heights pair so the location
/// toolbar keeps one height whether it shows breadcrumbs (28 px chips
/// centered in 36 px) or the 36 px location field.
pub mod metrics {
    // ---- spacing (4 px grid) ------------------------------------------
    /// Tightest rhythm: the gap between list rows.
    pub const SPACE_XXS: f32 = 2.0;
    /// Chip interiors and other compact padding.
    pub const SPACE_XS: f32 = 4.0;
    /// The default gap between controls in one row or section.
    pub const SPACE_S: f32 = 8.0;
    /// The root column's padding and gap.
    pub const SPACE_M: f32 = 12.0;
    /// Breathing room around small dialogs (confirm/secret root padding).
    pub const SPACE_L: f32 = 16.0;

    // ---- heights -------------------------------------------------------
    /// Single-line text fields (location, save name, folder, secret). The
    /// toolbar row is pinned to this height so swapping breadcrumbs for the
    /// location field never moves the rest of the dialog.
    pub const FIELD_HEIGHT: f32 = 36.0;
    /// Push buttons and toolbar icon buttons.
    pub const CONTROL_HEIGHT: f32 = 32.0;
    /// Directory-listing rows (minimum; content can grow).
    pub const ROW_HEIGHT: f32 = 32.0;
    /// Compact sidebar rows.
    pub const SIDEBAR_ROW_HEIGHT: f32 = 28.0;
    /// Sidebar section header height.
    pub const SIDEBAR_HEADER_HEIGHT: f32 = 20.0;
    /// Breadcrumb chips, centered inside the FIELD_HEIGHT toolbar.
    pub const CRUMB_HEIGHT: f32 = 28.0;
    /// The text caret bar inside app-owned edit surfaces.
    pub const CARET_W: f32 = 1.5;
    pub const CARET_H: f32 = 18.0;

    // ---- widths --------------------------------------------------------
    /// The places sidebar.
    pub const SIDEBAR_WIDTH: f32 = 180.0;
    /// The preview pane (ADR-0017), mirroring the sidebar's rhythm.
    pub const PREVIEW_WIDTH: f32 = 224.0;
    /// Below this window width the preview pane collapses (browsing keeps
    /// the full width); the default 1100-wide window clears it with room
    /// for the listing.
    pub const PREVIEW_MIN_WINDOW_W: f32 = 760.0;
    /// The preview image box's height allowance inside the pane; the pane
    /// clips around the aspect-fitted image.
    pub const PREVIEW_IMAGE_HEIGHT: f32 = 300.0;
    /// The quiet secondary action (Cancel).
    pub const BUTTON_WIDTH: f32 = 88.0;
    /// The default action (Open/Save/Replace/Unlock).
    pub const ACCEPT_WIDTH: f32 = 96.0;
    /// A breadcrumb chip's name is truncated to this measured width.
    pub const CRUMB_MAX_W: f32 = 160.0;

    // ---- adaptive sizing -------------------------------------------------
    /// The smallest width any prompt window takes however short its text:
    /// below this the two-button footer (88 + 96 + gaps + padding) wraps.
    pub const MIN_WINDOW_W: f32 = 360.0;
    /// The largest width a text-sized (confirm/secret/launcher) dialog
    /// grows to; longer content wraps instead. Mirrors a comfortable
    /// measure (~70 latin characters at body size).
    pub const MAX_TEXT_WINDOW_W: f32 = 560.0;
    /// The smallest height any prompt window takes: heading + body + field
    /// + footer at the token rhythm, with room for one wrapped line.
    pub const MIN_WINDOW_H: f32 = 200.0;
    /// The largest height a text-sized dialog grows to; longer bodies
    /// scroll inside the window rather than growing past most laptop
    /// screens' working height.
    pub const MAX_TEXT_WINDOW_H: f32 = 640.0;

    // ---- radius --------------------------------------------------------
    /// One corner radius for every prompt control and row, matching the
    /// theme's corner radius.
    pub const RADIUS: f32 = 8.0;
    /// Compact corner radius for sidebar items.
    pub const RADIUS_SM: f32 = 6.0;
    /// The radius of in-window material bands (the file chooser's toolbar),
    /// mirroring `aegis-design`'s `radii.popover` (12).
    pub const RADIUS_PANEL: f32 = 12.0;
    /// The radius of the file chooser's preview plate, mirroring
    /// `aegis-design`'s `radii.card` (16).
    pub const RADIUS_CARD: f32 = 16.0;

    // ---- font sizes ----------------------------------------------------
    /// Body text; applied to the theme explicitly.
    pub const FONT_BODY: f32 = 14.0;
    /// The dialog heading.
    pub const FONT_TITLE: f32 = 17.0;
    /// Hints, errors, and the typeahead readout.
    pub const FONT_SMALL: f32 = 12.5;

    // ---- icons ---------------------------------------------------------
    /// Row and toolbar glyphs.
    pub const ICON: f32 = 16.0;
    /// The root breadcrumb's drive glyph.
    pub const ICON_SMALL: f32 = 14.0;
}

/// One aegis scheme's prompt-surface tokens (values mirror
/// `aegis-design`'s `Colors`, except `danger`, which has no counterpart
/// there and extends the palette locally for inline error text).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub surface: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub border: Color,
    pub hover: Color,
    pub active: Color,
    pub field: Color,
    /// Inline error text (location/folder rejection). Local extension; see
    /// the struct docs.
    pub danger: Color,
    /// The translucent material band color for elevated in-window chrome
    /// (the Finder-style toolbar and places rail): a light wash over the
    /// opaque surface. Mirrors `aegis-design`'s `card_surface`.
    pub material: Color,
    /// The hairline that delineates a material band, slightly stronger
    /// than `border` so stacked translucent bands stay separable.
    pub material_border: Color,
    /// The scrim behind pinned in-window modals. Mirrors
    /// `aegis-design`'s `scrim`.
    pub scrim: Color,
}

/// The dark aegis appearance (`Design::dark`).
pub fn dark() -> Palette {
    Palette {
        surface: Color::rgba(25, 28, 40, 255),
        text: Color::rgba(244, 246, 252, 255),
        text_muted: Color::rgba(183, 188, 207, 255),
        accent: Color::rgba(102, 156, 255, 255),
        border: Color::rgba(255, 255, 255, 42),
        hover: Color::rgba(255, 255, 255, 24),
        active: Color::rgba(102, 156, 255, 56),
        field: Color::rgba(255, 255, 255, 18),
        danger: Color::rgba(255, 124, 120, 255),
        material: Color::rgba(255, 255, 255, 14),
        material_border: Color::rgba(255, 255, 255, 56),
        scrim: Color::rgba(8, 10, 18, 118),
    }
}

/// The light aegis appearance (`Design::light`).
pub fn light() -> Palette {
    Palette {
        surface: Color::rgba(243, 245, 249, 255),
        text: Color::rgba(29, 33, 44, 255),
        text_muted: Color::rgba(99, 105, 123, 255),
        accent: Color::rgba(43, 101, 232, 255),
        border: Color::rgba(28, 32, 44, 32),
        hover: Color::rgba(28, 32, 44, 12),
        active: Color::rgba(43, 101, 232, 44),
        field: Color::rgba(28, 32, 44, 10),
        danger: Color::rgba(198, 47, 42, 255),
        material: Color::rgba(255, 255, 255, 96),
        material_border: Color::rgba(28, 32, 44, 44),
        scrim: Color::rgba(28, 32, 44, 104),
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark { self::dark() } else { light() }
}

/// The resolved appearance one dialog renders with: the palette plus the
/// accessibility knobs that change rendering, not just color.
///
/// Built through [`ThemeInput::resolve`], never hand-assembled, so every
/// dialog applies the same precedence rules (snapshot over platform,
/// explicit scheme over `System`, snapshot accent over palette accent).
#[derive(Debug, Clone, Copy)]
pub struct ThemeInput {
    dark: bool,
    palette: Palette,
}

impl ThemeInput {
    /// Resolve the appearance the dialog should render with. `None`
    /// (backend had no compositor snapshot) falls back to the platform
    /// query iris performs; an explicit `System` in the snapshot defers
    /// to the same platform resolution.
    #[must_use]
    pub fn resolve(snapshot: Option<&PromptAppearance>) -> Self {
        Self::from_parts(snapshot, iris::system_prefers_dark())
    }

    /// The testable core of [`ThemeInput::resolve`]: the platform
    /// dark-preference is injected, never queried, so tests (and the
    /// notification daemon, which resolves once per batch) stay
    /// deterministic.
    #[must_use]
    fn from_parts(snapshot: Option<&PromptAppearance>, platform_dark: bool) -> Self {
        use aegis_portal_prompter::PromptColorScheme;
        let (dark, snapshot) = match snapshot {
            Some(appearance) => {
                let dark = match appearance.color_scheme {
                    PromptColorScheme::System => platform_dark,
                    PromptColorScheme::Dark => true,
                    PromptColorScheme::Light => false,
                };
                (dark, appearance)
            }
            None => (platform_dark, &PromptAppearance::default()),
        };
        let mut palette = palette(dark);
        if let Some(accent) = snapshot.accent_color {
            palette.accent = Color::rgba(accent.red, accent.green, accent.blue, 255);
            // The active wash is the accent at the palette's own overlay
            // alpha (22%), so a user accent restyles selection too.
            let alpha = if dark { 56 } else { 44 };
            palette.active = Color::rgba(accent.red, accent.green, accent.blue, alpha);
        }
        if snapshot.high_contrast {
            palette = high_contrast(palette, dark);
        }
        Self { dark, palette }
    }

    /// Whether the resolved palette is the dark one.
    pub fn dark(&self) -> bool {
        self.dark
    }

    /// The palette tokens to render with.
    pub fn palette(&self) -> Palette {
        self.palette
    }
}

/// Boost a palette toward WCAG-grade contrast: near-black/near-white text,
/// stronger borders, fields, and hover washes. Keeps the surface and
/// accent hues so the scheme identity survives.
fn high_contrast(base: Palette, dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color::rgba(255, 255, 255, 255),
            text_muted: Color::rgba(232, 236, 246, 255),
            border: Color::rgba(255, 255, 255, 120),
            hover: Color::rgba(255, 255, 255, 56),
            field: Color::rgba(255, 255, 255, 40),
            material_border: Color::rgba(255, 255, 255, 140),
            scrim: Color::rgba(0, 0, 0, 190),
            ..base
        }
    } else {
        Palette {
            text: Color::rgba(10, 12, 18, 255),
            text_muted: Color::rgba(38, 43, 58, 255),
            border: Color::rgba(28, 32, 44, 110),
            hover: Color::rgba(28, 32, 44, 36),
            field: Color::rgba(28, 32, 44, 30),
            material_border: Color::rgba(28, 32, 44, 120),
            scrim: Color::rgba(28, 32, 44, 170),
            ..base
        }
    }
}

/// The lens theme for prompt surfaces: the aegis palette over the matching
/// lens base (which supplies caret, selection, and focus-ring defaults),
/// driven by a resolved [`ThemeInput`].
pub fn theme_for(input: &ThemeInput) -> Theme {
    let palette = input.palette();
    let base = if input.dark() {
        Theme::dark()
    } else {
        Theme::light()
    };
    base.with_bg(palette.surface)
        .with_fg(palette.text)
        .with_accent(palette.accent)
        .with_border(palette.border)
        .with_hover(palette.hover)
        .with_active(palette.active)
        .with_corner_radius(metrics::RADIUS)
        .with_font_size(metrics::FONT_BODY)
}

/// The dialog heading.
pub fn title_style() -> Style {
    Style::new().with_font_size(metrics::FONT_TITLE)
}

/// Secondary (muted) text like a dialog body or hint.
pub fn muted_style_for(palette: &Palette) -> Style {
    Style::new().with_fg(palette.text_muted)
}

/// Small muted text: hints and the typeahead readout.
pub fn small_muted_style_for(palette: &Palette) -> Style {
    Style::new()
        .with_fg(palette.text_muted)
        .with_font_size(metrics::FONT_SMALL)
}

/// Small error text for inline rejection messages.
pub fn error_style_for(palette: &Palette) -> Style {
    Style::new()
        .with_fg(palette.danger)
        .with_font_size(metrics::FONT_SMALL)
}

/// Accent text (the IME composition inside edit surfaces).
pub fn accent_text_style_for(palette: &Palette) -> Style {
    Style::new().with_fg(palette.accent)
}

/// The quiet secondary action next to the accented default. Setting only
/// `bg` lets lens derive the hover and pressed surfaces.
pub fn secondary_button_style_for(palette: &Palette) -> Style {
    Style::new().with_bg(palette.hover)
}

/// The inert primary action while the dialog state is not acceptable:
/// muted text on the quiet secondary surface, no accent.
pub fn disabled_button_style_for(palette: &Palette) -> Style {
    Style::new()
        .with_bg(palette.hover)
        .with_fg(palette.text_muted)
}

/// The translucent material band options for elevated in-window chrome
/// (the Finder-style toolbar and places rail): a light wash plus a
/// hairline, rounded like a popover. Drawn with [`band_layout_opts`].
pub fn band_layout_opts(dark: bool) -> lens::LayoutOpts {
    let palette = palette(dark);
    lens::LayoutOpts {
        bg: palette.material,
        radius: metrics::RADIUS_PANEL,
        border: palette.material_border,
        // A material band takes a hairline, thicker than a control border.
        border_width: 1.0,
        ..Default::default()
    }
}

/// The scrim color for pinned in-window modals (overwrite confirmation):
/// dimming the base tree, not transparency, keeps the modal legible over
/// both schemes.
pub fn modal_backdrop(dark: bool) -> Color {
    palette(dark).scrim
}

/// Strip the GTK mnemonic underscore from a button label (`"_Share"` →
/// `"Share"`); lens has no mnemonic concept.
pub fn plain_label(label: &str) -> &str {
    label.trim_start_matches('_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_portal_prompter::{PromptAccent, PromptAppearance, PromptColorScheme};

    fn appearance(scheme: PromptColorScheme) -> PromptAppearance {
        PromptAppearance {
            color_scheme: scheme,
            accent_color: None,
            high_contrast: false,
            reduced_motion: false,
        }
    }

    #[test]
    fn snapshot_scheme_beats_platform_query() {
        let light = ThemeInput::from_parts(Some(&appearance(PromptColorScheme::Light)), true);
        assert!(!light.dark());
        let dark = ThemeInput::from_parts(Some(&appearance(PromptColorScheme::Dark)), false);
        assert!(dark.dark());
    }

    #[test]
    fn system_scheme_and_missing_snapshot_follow_platform() {
        assert!(
            !ThemeInput::from_parts(Some(&appearance(PromptColorScheme::System)), false).dark()
        );
        assert!(ThemeInput::from_parts(Some(&appearance(PromptColorScheme::System)), true).dark());
        assert!(!ThemeInput::from_parts(None, false).dark());
        assert!(ThemeInput::from_parts(None, true).dark());
    }

    #[test]
    fn accent_override_recolors_accent_and_active_wash() {
        let snapshot = PromptAppearance {
            color_scheme: PromptColorScheme::Dark,
            accent_color: Some(PromptAccent {
                red: 255,
                green: 120,
                blue: 10,
            }),
            high_contrast: false,
            reduced_motion: false,
        };
        let palette = ThemeInput::from_parts(Some(&snapshot), true).palette();
        assert_eq!(palette.accent, Color::rgba(255, 120, 10, 255));
        // The active wash is the accent at the dark palette's 22% alpha.
        assert_eq!(palette.active, Color::rgba(255, 120, 10, 56));
    }

    #[test]
    fn high_contrast_deepens_text_and_strokes() {
        let normal = ThemeInput::from_parts(Some(&appearance(PromptColorScheme::Dark)), true);
        let boosted = ThemeInput::from_parts(
            Some(&PromptAppearance {
                high_contrast: true,
                ..appearance(PromptColorScheme::Dark)
            }),
            true,
        )
        .palette();
        let plain = normal.palette();
        assert_ne!(boosted.text, plain.text);
        assert_ne!(boosted.border, plain.border);
        assert_eq!(boosted.surface, plain.surface, "scheme identity survives");
    }

    #[test]
    fn contrast_flag_restyles_the_palette() {
        let input = ThemeInput::from_parts(
            Some(&PromptAppearance {
                high_contrast: true,
                ..appearance(PromptColorScheme::System)
            }),
            false,
        );
        assert!(input.palette().text != palette(false).text);
    }
}
