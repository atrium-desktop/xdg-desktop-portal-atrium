//! The ScreenCast source chooser: a single-selection list of the capture
//! sources the backend offers (the whole desktop, individual outputs, the
//! interactive window pick), with a persistence checkbox only when the
//! backend offered one, plus Cancel and Share buttons. The first row
//! starts selected, arrow keys move the selection, Enter or a
//! double-click accepts the highlighted row, and Escape or the window's
//! close button cancels. The highlighted option's description renders
//! under the list.

use atrium_portal_prompter::{
    ChooseSourceRequest, ChooseSourceResponse, PromptAppearance, PromptResult,
};
use lens::{Frame, Input, key};

use super::style::{self, ThemeInput, metrics};
use super::{
    WindowChrome, close_window, display_size, escape_pressed, focus_widget, key_pressed,
    run_window_with_chrome, window_title,
};

/// The listing table's id. One dialog runs per prompter process, so a
/// static id suffices.
const LIST_ID: &str = "choose-source-list";

struct State {
    request: ChooseSourceRequest,
    appearance: ThemeInput,
    /// The highlighted row; always a valid index while the dialog runs
    /// (validation guarantees at least one option).
    selected: i32,
    remember: bool,
    /// Focus the list on the first frame.
    list_focus_pending: bool,
    done: Option<ChooseSourceResponse>,
}

pub fn run(
    request: ChooseSourceRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = window_title(&request.title, Some(&request.app_id));
    let state = State {
        request,
        appearance: ThemeInput::resolve(appearance),
        selected: 0,
        remember: false,
        list_focus_pending: true,
        done: None,
    };
    let state = run_window_with_chrome(
        &title,
        WindowChrome::resizable((480, 320), (420, 280), appearance),
        state,
        build,
    )?;
    // Closing the window without answering is a cancellation, matching the
    // other dialogs' delete-event semantics.
    let response = state.done.unwrap_or(ChooseSourceResponse::Cancelled);
    Ok(PromptResult::ChooseSource(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    if escape_pressed(input) {
        finish(state, None);
        return;
    }

    if key_pressed(input, key::RETURN) {
        accept(state);
        return;
    }
    if key_pressed(input, key::UP) && state.selected > 0 {
        state.selected -= 1;
    }
    if key_pressed(input, key::DOWN)
        && ((state.selected + 1) as usize) < state.request.options.len()
    {
        state.selected += 1;
    }

    let width = display_size(input).0 - 2.0 * metrics::SPACE_L;
    f.col()
        .gap(metrics::SPACE_S)
        .pad(metrics::SPACE_L)
        .flex(1.0)
        .show_flat(|f| {
            f.push_style(style::title_style());
            f.label(&state.request.title);
            f.pop_style();

            f.push_style(style::muted_style_for(&state.appearance.palette()));
            f.label_wrapped("Choose what to share", width.max(120.0));
            f.pop_style();

            // ---- source list ------------------------------------------
            if state.list_focus_pending {
                focus_widget(f, LIST_ID);
                state.list_focus_pending = false;
            }
            f.flex(1.0);
            let options = &state.request.options;
            let current_selected = state.selected;
            f.scroll(LIST_ID, |f| {
                f.col().gap(2.0).show_flat(|f| {
                    for (idx, option) in options.iter().enumerate() {
                        let is_selected = idx as i32 == current_selected;
                        if f.selectable(&option.label, is_selected) {
                            state.selected = idx as i32;
                        }
                    }
                });
            });

            // The highlighted option's detail, when the backend gave one.
            if let Some(description) = state
                .request
                .options
                .get(state.selected.max(0) as usize)
                .and_then(|option| option.description.as_deref())
            {
                f.push_style(style::muted_style_for(&state.appearance.palette()));
                f.label_wrapped(description, width.max(120.0));
                f.pop_style();
            }

            // ---- persistence checkbox ----------------------------------
            if state.request.remember_offered {
                f.checkbox("Remember this selection", &mut state.remember);
            }

            // ---- footer buttons -----------------------------------------
            f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                f.flex(1.0);
                f.spacer(0.0);

                f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                f.push_style(style::secondary_button_style_for(
                    &state.appearance.palette(),
                ));
                let cancel = f.button("Cancel");
                f.pop_style();
                if cancel {
                    finish(state, None);
                    return;
                }

                f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                if f.button("Share") {
                    accept(state);
                }
            });
        });
}

/// Answer with the highlighted row. The selected index always names an
/// offered option; `validate_for` on the backend double-checks anyway.
fn accept(state: &mut State) {
    let Some(option) = state.request.options.get(state.selected.max(0) as usize) else {
        return;
    };
    let response = ChooseSourceResponse::Selected {
        source: option.id.clone(),
        remember: state.remember && state.request.remember_offered,
    };
    finish(state, Some(response));
}

/// `None` cancels; `Some(response)` answers.
fn finish(state: &mut State, response: Option<ChooseSourceResponse>) {
    state.done = Some(response.unwrap_or(ChooseSourceResponse::Cancelled));
    close_window();
}
