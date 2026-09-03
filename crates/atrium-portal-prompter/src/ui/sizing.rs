//! Content-adaptive window sizing for the small prompt dialogs
//! (confirm/secret/launcher-edit): measure the actual text with the same
//! flux-text engine lens renders through, then pick the smallest window
//! that fits the content without wrapping, clamped to the design tokens'
//! min/max envelope.
//!
//! The mechanism is the lens headless context ([`lens::Ui::headless`]):
//! no GPU, no window, but the full text pipeline, so `measure_text`
//! returns the metrics the dialog will actually render with. The
//! dialogs already run their layout through lens; measuring through
//! the same engine (instead of estimating glyph averages) keeps the
//! size honest when fonts change.
//!
//! Permission-style dialogs are the motivating case: a two-line consent
//! body should not sit inside a fixed 460×220 frame with a scrolling
//! one-line body, and a long localized body should grow the window
//! up to the token cap and only then wrap.
//!
//! The file chooser keeps its large default window; its inner panes
//! already reflow against the live window size.

use std::cell::RefCell;
use std::rc::Rc;

use lens::Ui;

use super::style::metrics;

/// Measure one logical line, reporting its width in logical pixels.
/// The headless context falls back to the monospace estimator when no
/// font backend is available (CI containers), which is still a strictly
/// better estimate than a character-count heuristic.
///
/// Measurement runs inside a `Ui::frame` envelope because
/// `Frame::measure_text` is the shaped, themed path lens itself uses.
fn measure_line(ui: &Rc<RefCell<Ui>>, text: &str, size: f32) -> f32 {
    let input = lens::Input::new((4096.0, 4096.0), 0.016);
    ui.borrow_mut()
        .frame(&input, |f| f.measure_text(text, size).width)
}

/// Greedy word wrap that mirrors `lens_label_wrapped`'s whitespace
/// breaking closely enough for size selection: returns the number of
/// lines the text needs at the given wrap width.
fn wrapped_line_count(
    text: &str,
    size: f32,
    wrap_width: f32,
    measure: impl Fn(&str, f32) -> f32,
) -> usize {
    let mut lines = 0usize;
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if !current.is_empty() && measure(&candidate, size) > wrap_width {
            lines += 1;
            current = word.to_owned();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() || lines == 0 {
        lines += 1;
    }
    lines
}

/// The natural window size for a small prompt dialog.
///
/// `title` is the dialog heading (never wrapped), `body` the wrapped
/// body text, and `extra_rows` the heights of additional fixed
/// content between them and the footer (a text field, a checkbox, a
/// detail label — anything with a known token height).
///
/// Width is chosen so the body fits *without* wrapping when it can
/// (clamped to [`metrics::MAX_TEXT_WINDOW_W`]); beyond the cap the text
/// wraps at the cap and the height grows by the wrapped line count.
/// Height adds up the token rhythm: root padding, heading, gap, body
/// lines, extra rows, and the footer control row.
#[must_use]
pub fn prompt_window_size(title: &str, body: &str, extra_rows: &[f32], gaps: f32) -> (i32, i32) {
    let Ok(ui) = Ui::headless() else {
        // No text engine at all: the historical fixed size, so a
        // measurement failure never shrinks a dialog below its layout.
        return (460, 240);
    };
    let ui = Rc::new(RefCell::new(ui));
    prompt_window_size_with(title, body, extra_rows, gaps, |text, size| {
        measure_line(&ui, text, size)
    })
}

/// The testable core of [`prompt_window_size`] with the measurer
/// injected.
fn prompt_window_size_with(
    title: &str,
    body: &str,
    extra_rows: &[f32],
    gaps: f32,
    measure: impl Fn(&str, f32) -> f32,
) -> (i32, i32) {
    let title_w = measure(title, metrics::FONT_TITLE);
    // The footer's fixed footprint: the two standard buttons plus their
    // gap and the root padding, so a short body never wins over the
    // controls it sits above.
    let footer_w =
        metrics::BUTTON_WIDTH + metrics::SPACE_S + metrics::ACCEPT_WIDTH + 2.0 * metrics::SPACE_L;

    // Preferred width: everything on single lines, clamped.
    let body_w = measure(body, metrics::FONT_BODY);
    let preferred = title_w.max(body_w).max(footer_w);
    let width = preferred.clamp(metrics::MIN_WINDOW_W, metrics::MAX_TEXT_WINDOW_W);

    // Height: the wrapped line count at the chosen width.
    let inner_w = width - 2.0 * metrics::SPACE_L;
    let body_lines = wrapped_line_count(body, metrics::FONT_BODY, inner_w.max(1.0), &measure);
    let body_h = body_lines as f32 * metrics::FONT_BODY * LINE_HEIGHT_FACTOR;

    let mut height = 2.0 * metrics::SPACE_L // root padding (top+bottom)
        + metrics::FONT_TITLE * LINE_HEIGHT_FACTOR // heading
        + body_h
        + extra_rows.iter().sum::<f32>()
        + gaps; // spacing between the stacked sections
    height = height.clamp(metrics::MIN_WINDOW_H, metrics::MAX_TEXT_WINDOW_H);

    (width as i32, height as i32)
}

/// Lens line boxes run taller than the point size (ascent+descent+
/// leading); 1.4 is the stack's effective line height for the body and
/// heading faces. Only used for size selection, never for layout.
const LINE_HEIGHT_FACTOR: f32 = 1.4;

/// The window size for the confirm dialog: title + wrapped body +
/// the footer's control row.
pub fn confirm_size(title: &str, body: &str) -> (i32, i32) {
    let gaps = 2.0 * metrics::SPACE_M; // heading→body, body→footer
    prompt_window_size(title, body, &[metrics::CONTROL_HEIGHT], gaps)
}

/// The window size for the secret dialog: title + optional reason +
/// the password field + the footer's control row.
pub fn secret_size(title: &str, reason: Option<&str>) -> (i32, i32) {
    let body = reason.unwrap_or("");
    let gaps = 3.0 * metrics::SPACE_M; // heading→reason, reason→field, field→footer
    let rows = [metrics::FIELD_HEIGHT, metrics::CONTROL_HEIGHT];
    prompt_window_size(title, body, &rows, gaps)
}

/// The window size for the launcher editor: title + intro line + web
/// app target + icon note + name row + the footer's control row. The
/// intro and notes are short (an app id, a URL, a themed icon name),
/// so they join the wrapped body measurement as one string; the name
/// row keeps its fixed field height.
pub fn launcher_edit_size(
    title: &str,
    intro: &str,
    target: Option<&str>,
    icon_label: Option<&str>,
) -> (i32, i32) {
    let mut body = String::from(intro);
    if let Some(target) = target.filter(|t| !t.is_empty()) {
        body.push('\n');
        body.push_str(target);
    }
    if let Some(icon_label) = icon_label.filter(|l| !l.is_empty()) {
        body.push('\n');
        body.push_str(icon_label);
    }
    let gaps = 3.0 * metrics::SPACE_M; // heading→body, body→name, name→footer
    let rows = [metrics::FIELD_HEIGHT, metrics::CONTROL_HEIGHT];
    prompt_window_size(title, &body, &rows, gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A measurer stub: 0.6 em per glyph, CJK and emoji count double.
    fn stub_measure(text: &str, size: f32) -> f32 {
        let units: f32 = text
            .chars()
            .map(|c| {
                if ('\u{3000}'..='\u{9FFF}').contains(&c) || ('\u{FF00}'..='\u{FF60}').contains(&c)
                {
                    1.0
                } else {
                    0.5
                }
            })
            .sum();
        units * size
    }

    #[test]
    fn short_content_fits_without_wrapping() {
        let (w, h) = prompt_window_size_with(
            "Allow access?",
            "Let Terminal read your Documents folder",
            &[metrics::CONTROL_HEIGHT],
            2.0 * metrics::SPACE_M,
            stub_measure,
        );
        // Width grows past the floor for the one-line body but is capped.
        assert!(w >= metrics::MIN_WINDOW_W as i32 && w <= metrics::MAX_TEXT_WINDOW_W as i32);
        assert!(h >= metrics::MIN_WINDOW_H as i32);
    }

    #[test]
    fn width_caps_and_long_bodies_wrap() {
        let (w1, h1) = prompt_window_size_with("T", "short", &[], 0.0, stub_measure);
        let long = "word ".repeat(200);
        let (w2, h2) = prompt_window_size_with("T", &long, &[], 0.0, stub_measure);
        assert_eq!(w2, metrics::MAX_TEXT_WINDOW_W as i32, "width must cap");
        assert!(h2 > h1, "long body grows the window");
        assert!(h2 <= metrics::MAX_TEXT_WINDOW_H as i32, "height must cap");
        assert!(w1 >= metrics::MIN_WINDOW_W as i32);
    }

    #[test]
    fn footer_never_wins_below_the_floor() {
        // An empty body still leaves room for the two buttons.
        let (w, _h) = prompt_window_size_with("T", "", &[], 0.0, stub_measure);
        assert!(
            w >= (metrics::BUTTON_WIDTH
                + metrics::SPACE_S
                + metrics::ACCEPT_WIDTH
                + 2.0 * metrics::SPACE_L) as i32
        );
    }

    #[test]
    fn wrapped_count_handles_empty_and_multiline() {
        assert_eq!(wrapped_line_count("", 14.0, 400.0, stub_measure), 1);
        assert_eq!(
            wrapped_line_count("one two three", 14.0, 400.0, stub_measure),
            1
        );
        assert!(
            wrapped_line_count("one two three", 14.0, 40.0, stub_measure) > 1,
            "narrow wrap width forces multiple lines"
        );
    }

    #[test]
    fn real_measurement_path_returns_sane_sizes() {
        // Goes through the real headless lens context; in CI without
        // fonts the monospace fallback still yields nonzero widths.
        let (w, h) = confirm_size("Share Screen", "Allow the app to see your screen?");
        assert!(w > 0 && h > 0);
        let (w2, _h2) = secret_size("Unlock Keyring", None);
        assert!(w2 > 0);
    }
}
