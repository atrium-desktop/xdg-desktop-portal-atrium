//! The yes/no confirmation dialog (portal Account/Access consent flows):
//! a centered heading, wrapped body text, and Cancel plus one affirmative
//! button. Escape or the window's close button cancels.
//!
//! The window is content-adaptive ([`super::sizing`]): measured from the
//! actual heading and body text, so a one-line permission request gets a
//! compact window and a long localized body grows up to the design cap
//! before wrapping. The dialog renders with the compositor appearance
//! snapshot from the request (contract v6).

use atrium_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptAppearance, PromptResult};
use lens::{Frame, Input};

use super::sizing;
use super::style::{self, ThemeInput, metrics};
use super::{
    WindowChrome, close_window, display_size, escape_pressed, run_window_with_chrome, window_title,
};

struct State {
    request: ConfirmRequest,
    appearance: ThemeInput,
    done: Option<ConfirmResponse>,
}

pub fn run(
    request: ConfirmRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = window_title(&request.title, None);
    let size = sizing::confirm_size(&request.title, &request.body);
    let state = State {
        appearance: ThemeInput::resolve(appearance),
        request,
        done: None,
    };
    let state = run_window_with_chrome(
        &title,
        WindowChrome::fixed_to(size, appearance),
        state,
        build,
    )?;
    // Closing the window without answering is a cancellation, matching the
    // former GTK dialog's delete-event semantics.
    let response = state.done.unwrap_or(ConfirmResponse::Cancelled);
    Ok(PromptResult::Confirm(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    if escape_pressed(input) {
        finish(state, ConfirmResponse::Cancelled);
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

            f.push_style(style::muted_style_for(&state.appearance.palette()));
            f.label_wrapped(&state.request.body, width.max(120.0));
            f.pop_style();

            f.flex(1.0);
            f.spacer(0.0);

            f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                f.flex(1.0);
                f.spacer(0.0);

                f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                f.push_style(style::secondary_button_style_for(
                    &state.appearance.palette(),
                ));
                let deny = state
                    .request
                    .deny_label
                    .as_deref()
                    .map(style::plain_label)
                    .unwrap_or("Cancel");
                let cancel = f.button(deny);
                f.pop_style();
                if cancel {
                    finish(state, ConfirmResponse::Cancelled);
                    return;
                }

                f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                let accept = state
                    .request
                    .accept_label
                    .as_deref()
                    .map(style::plain_label)
                    .unwrap_or("Continue");
                if f.button(accept) {
                    finish(state, ConfirmResponse::Confirmed);
                }
            });
        });
}

fn finish(state: &mut State, response: ConfirmResponse) {
    state.done = Some(response);
    close_window();
}
