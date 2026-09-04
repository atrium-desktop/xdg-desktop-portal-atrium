//! App-owned single-line editing surfaces. lens's textfield keeps its
//! caret in widget state with no host API to move it, so a programmatic
//! buffer change (pre-filled path, Tab completion) would strand the caret
//! at a stale offset; owning the string and caret sidesteps that. The
//! secret prompt established the pattern: the dialog renders the text and
//! caret itself and consumes text and editing keys from the per-frame
//! input snapshot.
//!
//! These surfaces are full IME citizens, mirroring lens's textfield: the
//! in-progress composition (preedit) renders inline underlined with the
//! caret inside it, `delete_surrounding_text` requests are applied, and
//! the caret rectangle is reported every focused frame so the IME's
//! candidate window anchors at the caret (`zwp_text_input_v3`
//! `set_cursor_rectangle`, fed by `lens_set_caret_rect`).
//!
//! Platform limits inherited from iris (documented, not worked around):
//! text-input is enabled per surface, not per field, so there is no
//! per-field content purpose; commit text caps at 31 bytes and the preedit
//! at 63 bytes per frame; surrounding text is never reported to the IME.
//! Raising any of these needs an optics release.
//!
//! The store behind a surface is pluggable ([`EditBuffer`]): `String`
//! grows to fit, while the secret prompt's fixed page-locked
//! [`SecretBuffer`](super::secret_buffer::SecretBuffer) takes only what
//! fits and never reallocates.

use lens::{Align, Color, Frame, Input, Rect, key};

use super::style::{self, metrics};
use super::{Preedit, command_held, committed_text, ime_delete, key_down, key_pressed};

/// The mask glyph drawn per typed character on secret surfaces.
pub const MASK: &str = "•";

/// The text store behind an app-owned edit surface: read as `&str`,
/// inserted into at a byte index, removed from by byte range.
/// Implementations keep the text valid UTF-8; a bounded store may take
/// only part of an insertion, so [`EditBuffer::insert_str`] reports how
/// many bytes were actually inserted and the caret advances by exactly
/// that.
pub trait EditBuffer {
    /// The current text.
    fn as_str(&self) -> &str;
    /// Insert `s` at byte `index` (a char boundary); returns the number of
    /// bytes inserted — a bounded store takes only the char-boundary
    /// prefix of `s` that fits.
    fn insert_str(&mut self, index: usize, s: &str) -> usize;
    /// Remove the byte range (both ends on char boundaries).
    fn remove_range(&mut self, range: std::ops::Range<usize>);
}

impl EditBuffer for String {
    fn as_str(&self) -> &str {
        self
    }

    fn insert_str(&mut self, index: usize, s: &str) -> usize {
        String::insert_str(self, index, s);
        s.len()
    }

    fn remove_range(&mut self, range: std::ops::Range<usize>) {
        self.replace_range(range, "");
    }
}

/// The caret bar drawn between text runs.
pub fn render_caret(f: &mut Frame, color: Color) {
    f.row()
        .width(metrics::CARET_W)
        .height(metrics::CARET_H)
        .bg(color)
        .empty();
}

/// Insert text at the caret, dropping control characters (single line).
/// The caret advances by what the store actually took: a bounded store
/// drops input past its capacity.
pub fn insert<T: EditBuffer>(text: &mut T, caret: &mut usize, input: &str) {
    let clean: String = input.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return;
    }
    let inserted = text.insert_str(*caret, &clean);
    *caret += inserted;
}

pub fn delete_backward<T: EditBuffer>(text: &mut T, caret: &mut usize) {
    let start = prev_boundary(text.as_str(), *caret);
    if start < *caret {
        text.remove_range(start..*caret);
        *caret = start;
    }
}

pub fn delete_forward<T: EditBuffer>(text: &mut T, caret: &mut usize) {
    let end = next_boundary(text.as_str(), *caret);
    if end > *caret {
        text.remove_range(*caret..end);
    }
}

pub fn prev_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

pub fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .chars()
        .next()
        .map_or(text.len(), |c| index + c.len_utf8())
}

/// Apply the IME's `delete_surrounding_text` request: byte counts before
/// and after the caret, widened outward to whole characters (a partial
/// character cannot be deleted). Mirrors lens's textfield.
pub fn apply_ime_delete<T: EditBuffer>(text: &mut T, caret: &mut usize, before: u32, after: u32) {
    if before > 0 && *caret > 0 {
        let mut start = caret.saturating_sub(before as usize);
        while start > 0 && !text.as_str().is_char_boundary(start) {
            start -= 1;
        }
        if start < *caret {
            text.remove_range(start..*caret);
            *caret = start;
        }
    }
    if after > 0 {
        let mut end = (*caret + after as usize).min(text.as_str().len());
        while end < text.as_str().len() && !text.as_str().is_char_boundary(end) {
            end += 1;
        }
        if end > *caret {
            text.remove_range(*caret..end);
        }
    }
}

/// The display runs one edit surface frame is composed of. With an active
/// IME composition the caret lives inside the preedit at its own cursor;
/// `pre_before` and `pre_after` split the preedit there.
#[derive(Debug, PartialEq, Eq)]
pub struct Runs<'a> {
    pub before: &'a str,
    pub pre_before: &'a str,
    pub pre_after: &'a str,
    pub after: &'a str,
    pub has_preedit: bool,
}

/// Split the surface text (already split at the caret) and an optional
/// preedit (`text`, `cursor`) into render runs.
pub fn compose<'a>(before: &'a str, after: &'a str, preedit: Option<(&'a str, usize)>) -> Runs<'a> {
    match preedit {
        Some((text, cursor)) if !text.is_empty() => Runs {
            before,
            pre_before: &text[..cursor],
            pre_after: &text[cursor..],
            after,
            has_preedit: true,
        },
        _ => Runs {
            before,
            pre_before: "",
            pre_after: "",
            after,
            has_preedit: false,
        },
    }
}

/// One app-owned single-line edit surface to render this frame.
pub struct EditSurface<'a> {
    /// The widget id (also the caret-rect measurement anchor).
    pub id: &'a str,
    /// The full buffer; `caret` must be a byte index on a char boundary.
    pub text: &'a str,
    pub caret: usize,
    /// Shown when the buffer is empty and no composition is in progress.
    pub placeholder: &'a str,
    /// Whether the surface owns typing (draws the caret and the focus ring,
    /// and reports the caret rectangle).
    pub focused: bool,
    /// This frame's IME composition, if any.
    pub preedit: Option<&'a Preedit>,
    /// Render [`MASK`] per character instead of the text (secret prompts);
    /// the preedit is masked too so a composition never echoes.
    pub masked: bool,
}

/// Render one edit surface and, while it is focused, report its caret
/// rectangle so the IME candidate window tracks the caret. Returns the
/// row's response (click/hover state for focus-follows-pointer).
pub fn edit_surface(
    f: &mut Frame,
    appearance: &style::ThemeInput,
    surface: EditSurface<'_>,
) -> lens::Response {
    let palette = appearance.palette();

    let (before, after) = surface.text.split_at(surface.caret);
    // Masked surfaces measure and render bullets; the real text and the
    // composition content never reach a label.
    let masked;
    let runs = if surface.masked {
        masked = (
            MASK.repeat(before.chars().count()),
            MASK.repeat(after.chars().count()),
            surface
                .preedit
                .map(|preedit| MASK.repeat(preedit.text.chars().count())),
        );
        let cursor = surface.preedit.map_or(0, |preedit| {
            preedit.text[..preedit.cursor].chars().count() * MASK.len()
        });
        compose(
            &masked.0,
            &masked.1,
            masked.2.as_deref().map(|text| (text, cursor)),
        )
    } else {
        compose(
            before,
            after,
            surface
                .preedit
                .map(|preedit| (preedit.text.as_str(), preedit.cursor)),
        )
    };

    let empty = before.is_empty() && after.is_empty() && !runs.has_preedit;
    let focused = surface.focused;
    let id = surface.id;
    let (response, ()) = f
        .row()
        .height(metrics::FIELD_HEIGHT)
        .pad(metrics::SPACE_S)
        .items_center()
        .bg(palette.field)
        .border(if surface.focused {
            palette.accent
        } else {
            palette.border
        })
        .border_width(1.0)
        .rounded(metrics::RADIUS)
        .id(id)
        .show(|f| {
            if empty {
                if focused {
                    render_caret(f, palette.text);
                }
                f.push_style(style::muted_style_for(&palette));
                f.label(surface.placeholder);
                f.pop_style();
                return;
            }
            if !runs.before.is_empty() {
                f.label(runs.before);
            }
            if runs.has_preedit {
                // The composition: accent text underlined, caret inside at the
                // preedit cursor — the same reading lens's textfield gives.
                f.col().gap(0.0).cross(Align::Stretch).show_flat(|f| {
                    f.row().gap(0.0).items_center().show_flat(|f| {
                        f.push_style(style::accent_text_style_for(&palette));
                        if !runs.pre_before.is_empty() {
                            f.label(runs.pre_before);
                        }
                        if focused {
                            render_caret(f, palette.text);
                        }
                        if !runs.pre_after.is_empty() {
                            f.label(runs.pre_after);
                        }
                        f.pop_style();
                    });
                    // The underline stretches to the text row's width.
                    f.row().height(1.0).bg(palette.accent).empty();
                });
            } else if focused {
                render_caret(f, palette.text);
            }
            if !runs.after.is_empty() {
                f.label(runs.after);
            }
        });

    if focused && let Some(bounds) = f.node_bounds(id) {
        let visual_before = format!("{}{}", runs.before, runs.pre_before);
        let x =
            bounds.x + metrics::SPACE_S + f.measure_text(&visual_before, metrics::FONT_BODY).width;
        f.set_caret_rect(Rect {
            x,
            y: bounds.y + (bounds.h - metrics::CARET_H) / 2.0,
            w: metrics::CARET_W,
            h: metrics::CARET_H,
        });
    }
    response
}

/// Apply a frame of input to an owned buffer, in protocol order: the IME's
/// surrounding-text deletion, committed text (typed characters and IME
/// results), editing keys (Backspace/Delete, caret arrows/Home/End), and
/// Ctrl+V paste. The caller keeps Return/Tab and focus policy. On a
/// bounded store, input past the capacity is dropped (see [`EditBuffer`]).
pub fn edit_keys<T: EditBuffer>(text: &mut T, caret: &mut usize, f: &mut Frame, input: &Input) {
    let (before, after) = ime_delete(input);
    apply_ime_delete(text, caret, before, after);
    let committed = committed_text(input);
    if !committed.is_empty() {
        insert(text, caret, &committed);
    }
    if key_down(input, key::BACKSPACE) {
        delete_backward(text, caret);
    }
    if key_down(input, key::DELETE) {
        delete_forward(text, caret);
    }
    if key_down(input, key::LEFT) {
        *caret = prev_boundary(text.as_str(), *caret);
    }
    if key_down(input, key::RIGHT) {
        *caret = next_boundary(text.as_str(), *caret);
    }
    if key_down(input, key::HOME) {
        *caret = 0;
    }
    if key_down(input, key::END) {
        *caret = text.as_str().len();
    }
    if command_held(input) && key_pressed(input, 'v' as i32) {
        f.request_paste();
    }
    if let Some(paste) = f.take_paste() {
        insert(text, caret, &paste);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_moves_the_caret_with_the_text() {
        let mut text = String::from("ab");
        let mut caret = 1;
        insert(&mut text, &mut caret, "xy\n");
        assert_eq!(text, "axyb");
        assert_eq!(caret, 3);
    }

    #[test]
    fn editing_is_char_boundary_safe() {
        let mut text = String::from("aé中");
        let mut caret = text.len();
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "aé");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "a");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "");

        insert(&mut text, &mut caret, "é中");
        let mut caret = 0;
        delete_forward(&mut text, &mut caret);
        assert_eq!(text, "中");
        delete_forward(&mut text, &mut caret);
        assert_eq!(text, "");
    }

    #[test]
    fn caret_movement_clamps_to_the_ends() {
        let text = String::from("aé");
        assert_eq!(prev_boundary(&text, 0), 0);
        assert_eq!(next_boundary(&text, text.len()), text.len());
        assert_eq!(next_boundary(&text, 1), 3);
        assert_eq!(prev_boundary(&text, 3), 1);
    }

    #[test]
    fn ime_delete_removes_whole_characters() {
        let mut text = String::from("ab中文cd");
        let mut caret = 5; // between 中 and 文
        apply_ime_delete(&mut text, &mut caret, 3, 3);
        assert_eq!(text, "abcd");
        assert_eq!(caret, 2);
    }

    #[test]
    fn ime_delete_widens_a_partial_byte_count() {
        let mut text = String::from("中a");
        let mut caret = 3; // after 中
        // Two bytes into the three-byte 中: the whole character goes.
        apply_ime_delete(&mut text, &mut caret, 2, 0);
        assert_eq!(text, "a");
        assert_eq!(caret, 0);

        let mut text = String::from("a中");
        let mut caret = 1;
        apply_ime_delete(&mut text, &mut caret, 0, 2);
        assert_eq!(text, "a");
        assert_eq!(caret, 1);
    }

    #[test]
    fn ime_delete_clamps_to_the_buffer() {
        let mut text = String::from("ab");
        let mut caret = 1;
        apply_ime_delete(&mut text, &mut caret, 100, 100);
        assert_eq!(text, "");
        assert_eq!(caret, 0);
        // A zero request changes nothing.
        let mut text = String::from("ab");
        let mut caret = 1;
        apply_ime_delete(&mut text, &mut caret, 0, 0);
        assert_eq!(text, "ab");
        assert_eq!(caret, 1);
    }

    #[test]
    fn compose_without_preedit_splits_at_the_caret() {
        let runs = compose("ab", "cd", None);
        assert_eq!(
            runs,
            Runs {
                before: "ab",
                pre_before: "",
                pre_after: "",
                after: "cd",
                has_preedit: false,
            }
        );
    }

    #[test]
    fn compose_places_the_caret_inside_the_preedit() {
        let runs = compose("ab", "cd", Some(("shi", 1)));
        assert_eq!(
            runs,
            Runs {
                before: "ab",
                pre_before: "s",
                pre_after: "hi",
                after: "cd",
                has_preedit: true,
            }
        );
        // An empty preedit string is no composition.
        assert!(!compose("ab", "cd", Some(("", 0))).has_preedit);
    }
}
