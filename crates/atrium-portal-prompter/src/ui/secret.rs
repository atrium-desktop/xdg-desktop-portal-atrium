//! The secret password prompt: a centered heading, an optional reason line,
//! and a masked edit field with Cancel / Unlock buttons.
//!
//! lens has no password-masking text widget, so the field is an app-owned
//! editing surface (the pattern lens's caret/paste API is designed for):
//! the dialog owns the real secret and its caret, renders bullet glyphs
//! itself, and consumes text and editing keys straight from the per-frame
//! input snapshot. The secret accumulates in a fixed-capacity, page-locked
//! [`SecretBuffer`] (256 bytes of UTF-8 — input past the cap does not
//! append, visible as the bullet count stopping), is zeroized on every
//! clear path, and never reaches the clipboard. IME compositions are
//! masked too, so a preedit never echoes the secret's content.

use atrium_portal_prompter::{PromptAppearance, PromptResult, SecretRequest, SecretResponse};
use lens::{Frame, Input, key};

use super::edit;
use super::secret_buffer::SecretBuffer;
use super::sizing;
use super::style::{self, ThemeInput, metrics};
use super::{
    WindowChrome, close_window, display_size, escape_pressed, key_pressed, preedit,
    run_window_with_chrome, window_title,
};

struct State {
    request: SecretRequest,
    appearance: ThemeInput,
    secret: SecretBuffer,
    /// Caret as a byte index into the secret, always on a char boundary.
    caret: usize,
    /// Whether the password surface owns typing. Starts focused, matching
    /// the former GTK dialog's `grab_focus`.
    focused: bool,
    done: Option<SecretResponse>,
}

pub fn run(
    request: SecretRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = window_title(&request.title, None);
    let size = sizing::secret_size(&request.title, request.reason.as_deref());
    let state = State {
        appearance: ThemeInput::resolve(appearance),
        request,
        secret: SecretBuffer::new(),
        caret: 0,
        focused: true,
        done: None,
    };
    let state = run_window_with_chrome(
        &title,
        WindowChrome::fixed_to(size, appearance),
        state,
        build,
    )?;
    let response = state.done.unwrap_or(SecretResponse::Cancelled);
    Ok(PromptResult::Secret(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    if escape_pressed(input) {
        finish(state, SecretResponse::Cancelled);
        return;
    }

    let width = display_size(input).0 - 2.0 * metrics::SPACE_L;
    f.col()
        .gap(metrics::SPACE_M)
        .pad(metrics::SPACE_L)
        .show_flat(|f| {
            f.push_style(style::title_style());
            f.label(&state.request.title);
            f.pop_style();

            if let Some(reason) = state
                .request
                .reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                f.push_style(style::muted_style_for(&state.appearance.palette()));
                f.label_wrapped(reason, width.max(120.0));
                f.pop_style();
            }

            let composition = if state.focused { preedit(input) } else { None };
            let response = edit::edit_surface(
                f,
                &state.appearance,
                edit::EditSurface {
                    id: "secret-field",
                    text: state.secret.as_str(),
                    caret: state.caret,
                    placeholder: "Password",
                    focused: state.focused,
                    preedit: composition.as_ref(),
                    masked: true,
                },
            );

            // Focus follows the pointer: clicking the field focuses it,
            // clicking anywhere else unfocuses it.
            let left = lens::sys::lens_mouse_button::LENS_MOUSE_LEFT as usize;
            if response.clicked {
                state.focused = true;
            } else if input.as_raw().mouse_pressed[left] && !response.hovered {
                state.focused = false;
            }

            f.flex(1.0);
            f.spacer(0.0);

            f.row()
                .gap(metrics::SPACE_S)
                .items_center()
                .show_flat(|f| {
                    f.flex(1.0);
                    f.spacer(0.0);

                    f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                    f.push_style(style::secondary_button_style_for(
                        &state.appearance.palette(),
                    ));
                    let cancel = f.button("Cancel");
                    f.pop_style();
                    if cancel && state.done.is_none() {
                        finish(state, SecretResponse::Cancelled);
                        return;
                    }

                    f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                    if f.button("Unlock") && state.done.is_none() {
                        submit(state);
                    }
                });
        });

    if state.focused {
        // The app-owned field consumes all text input; keep lens's own
        // focus empty so a Tab-focused button cannot also fire on Return.
        f.clear_focus();
        edit_secret(state, f, input);
    }
}

/// Apply this frame's input to the owned secret.
fn edit_secret(state: &mut State, f: &mut Frame, input: &Input) {
    edit::edit_keys(&mut state.secret, &mut state.caret, f, input);
    if key_pressed(input, key::RETURN) && state.done.is_none() {
        submit(state);
    }
}

fn submit(state: &mut State) {
    // The one bounded copy out of the locked buffer: the response contract
    // takes an owned String, and the buffer is zeroed as the handoff
    // completes, leaving the response the only live copy.
    let value = state.secret.as_str().to_owned();
    state.secret.clear();
    finish(state, SecretResponse::Secret { value });
}

fn finish(state: &mut State, response: SecretResponse) {
    state.done = Some(response);
    close_window();
}

/// Headless interaction tests: the dialog's `build` runs on a headless
/// lens `Ui` with synthetic inputs, exercising the real keyboard and paste
/// routing into the locked buffer and the submit/cancel contract — no
/// windowing system involved.
#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::ui::secret_buffer::CAPACITY;
    use lens::{Input, Ui, key};

    fn fresh_state() -> State {
        State {
            request: SecretRequest {
                title: "Unlock Keyring".to_owned(),
                reason: None,
            },
            appearance: ThemeInput::resolve(None),
            secret: SecretBuffer::new(),
            caret: 0,
            focused: true,
            done: None,
        }
    }

    fn frame(ui: &mut Ui, state: &mut State, input: &Input) {
        ui.frame(input, |f| build(state, f, input));
    }

    /// Idle frames so retained state settles.
    fn settle(ui: &mut Ui, state: &mut State) {
        let input = Input::new((440.0, 240.0), 0.016);
        for _ in 0..3 {
            frame(ui, state, &input);
        }
    }

    fn tap(ui: &mut Ui, state: &mut State, key: i32) {
        let mut input = Input::new((440.0, 240.0), 0.016);
        input.push_key(key, true, false);
        frame(ui, state, &input);
    }

    fn type_text(ui: &mut Ui, state: &mut State, text: &str) {
        let mut input = Input::new((440.0, 240.0), 0.016);
        input.set_text(text);
        frame(ui, state, &input);
    }

    #[test]
    fn typing_edits_the_secret_and_the_caret() {
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state();
        settle(&mut ui, &mut state);

        type_text(&mut ui, &mut state, "pwé");
        assert_eq!(state.secret.as_str(), "pwé");
        assert_eq!(state.caret, 4);

        tap(&mut ui, &mut state, key::BACKSPACE);
        assert_eq!(state.secret.as_str(), "pw");
        // The deleted char's bytes are zeroed, not left in the allocation.
        assert!(state.secret.raw_bytes()[2..].iter().all(|&byte| byte == 0));

        // Caret movement stays on char boundaries; insert lands mid-text.
        tap(&mut ui, &mut state, key::LEFT);
        type_text(&mut ui, &mut state, "é");
        assert_eq!(state.secret.as_str(), "péw");

        // Forward delete removes whole characters.
        tap(&mut ui, &mut state, key::HOME);
        tap(&mut ui, &mut state, key::DELETE);
        assert_eq!(state.secret.as_str(), "éw");
        tap(&mut ui, &mut state, key::DELETE);
        assert_eq!(state.secret.as_str(), "w");
    }

    #[test]
    fn paste_inserts_through_the_real_input_path() {
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state();
        settle(&mut ui, &mut state);

        ui.paste("hunter2");
        frame(&mut ui, &mut state, &Input::new((440.0, 240.0), 0.016));
        assert_eq!(state.secret.as_str(), "hunter2");
        assert_eq!(state.caret, 7);
    }

    #[test]
    fn input_past_the_cap_does_not_append() {
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state();
        settle(&mut ui, &mut state);

        ui.paste(&"x".repeat(300));
        frame(&mut ui, &mut state, &Input::new((440.0, 240.0), 0.016));
        assert_eq!(state.secret.as_str().len(), CAPACITY);
        // Typing past the cap is ignored — the visibly bounded behavior.
        type_text(&mut ui, &mut state, "y");
        assert_eq!(state.secret.as_str().len(), CAPACITY);
        assert!(!state.secret.as_str().contains('y'));
    }

    #[test]
    fn return_submits_the_secret_and_clears_the_buffer() {
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state();
        settle(&mut ui, &mut state);

        type_text(&mut ui, &mut state, "sécret");
        tap(&mut ui, &mut state, key::RETURN);
        match &state.done {
            Some(SecretResponse::Secret { value }) => assert_eq!(value, "sécret"),
            other => panic!("expected a secret response, got {other:?}"),
        }
        // The handoff zeroed the buffer; the response holds the only copy.
        assert!(state.secret.raw_bytes().iter().all(|&byte| byte == 0));
    }

    #[test]
    fn escape_cancels() {
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state();
        settle(&mut ui, &mut state);
        tap(&mut ui, &mut state, key::ESCAPE);
        assert!(matches!(state.done, Some(SecretResponse::Cancelled)));
    }
}
