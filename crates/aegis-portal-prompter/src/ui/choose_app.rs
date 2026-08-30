//! The AppChooser dialog: a single-selection list of application names with
//! the request's embedded choices (the "remember this choice" checkbox)
//! underneath, plus Cancel and Select buttons. The first row starts
//! selected (the backend orders the list so the portal's `last_choice`
//! hint leads), arrow keys move the selection, Enter or a double-click
//! accepts the highlighted row, and Escape or the window's close button
//! cancels.
//!
//! Rows render names only: the lens table's per-cell icons come from its
//! built-in glyph set, and there is no glyph for "an application", let
//! alone a themed-icon lookup. The icon names still ride the process
//! contract (`AppChoice::icon`) so an icon-capable dialog needs no version
//! bump.

use aegis_portal_prompter::{ChooseAppRequest, ChooseAppResponse, PromptAppearance, PromptResult};
use lens::{Band, Frame, Input, PlaceMode, PlaceOpts, key};

use super::style::ThemeInput;
use super::style::{self, metrics};
use super::{
    WindowChrome, close_window, display_size, escape_pressed, focus_widget, key_pressed,
    run_window_with_chrome, window_title,
};

/// The listing table's id. One dialog runs per prompter process, so a
/// static id suffices.
const LIST_ID: &str = "choose-app-list";

/// One embedded choice's live value (mirrors the FileChooser dialog's).
enum ChoiceState {
    Bool(bool),
    Options(i32),
}

struct State {
    request: ChooseAppRequest,
    appearance: ThemeInput,
    /// The highlighted row; always a valid index while the dialog runs
    /// (validation guarantees at least one candidate).
    selected: i32,
    choices: Vec<ChoiceState>,
    /// Focus the list on the first frame.
    list_focus_pending: bool,
    done: Option<ChooseAppResponse>,
}

pub fn run(
    request: ChooseAppRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = window_title(&request.title, Some(&request.app_id));
    let choices = request
        .choices
        .iter()
        .map(|choice| {
            if choice.options.is_empty() {
                ChoiceState::Bool(choice.selected == "true")
            } else {
                let index = choice
                    .options
                    .iter()
                    .position(|(id, _)| id == &choice.selected)
                    .unwrap_or(0) as i32;
                ChoiceState::Options(index)
            }
        })
        .collect();
    let state = State {
        request,
        appearance: ThemeInput::resolve(appearance),
        selected: 0,
        choices,
        list_focus_pending: true,
        done: None,
    };
    let state = run_window_with_chrome(
        &title,
        WindowChrome::resizable((480, 360), (420, 300), appearance),
        state,
        build,
    )?;
    // Closing the window without answering is a cancellation, matching the
    // other dialogs' delete-event semantics.
    let response = state.done.unwrap_or(ChooseAppResponse::Cancelled);
    Ok(PromptResult::ChooseApp(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    if escape_pressed(input) && !choices_popup_open(state, f) {
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
    if key_pressed(input, key::DOWN) && ((state.selected + 1) as usize) < state.request.apps.len() {
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
            f.label_wrapped(
                &format!(
                    "Choose an application to open content of type {}",
                    state.request.content_type
                ),
                width.max(120.0),
            );
            f.pop_style();

            // ---- application list --------------------------------------
            if state.list_focus_pending {
                focus_widget(f, LIST_ID);
                state.list_focus_pending = false;
            }
            f.flex(1.0);
            let apps = &state.request.apps;
            let current_selected = state.selected;
            f.scroll(LIST_ID, |f| {
                f.col().gap(2.0).show_flat(|f| {
                    for (idx, app) in apps.iter().enumerate() {
                        let is_selected = idx as i32 == current_selected;
                        if f.selectable(&app.name, is_selected) {
                            state.selected = idx as i32;
                        }
                    }
                });
            });

            // ---- embedded choices ---------------------------------------
            for index in 0..state.request.choices.len() {
                choice_row(state, f, index);
            }

            // ---- footer buttons ------------------------------------------
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
                    if cancel {
                        finish(state, None);
                        return;
                    }

                    f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                    if f.button("Select") {
                        accept(state);
                    }
                });
        });
}

/// Whether an embedded choice's dropdown popup is open (it swallows
/// Escape), mirroring the FileChooser dialog.
fn choices_popup_open(state: &State, f: &mut Frame) -> bool {
    state
        .request
        .choices
        .iter()
        .any(|choice| f.place_is_open(&format!("choice-{}", choice.id)))
}

/// One embedded choice: a boolean checkbox, or a labeled dropdown of
/// option labels (same rendering as the FileChooser dialog's choices).
fn choice_row(state: &mut State, f: &mut Frame, index: usize) {
    let choice = state.request.choices[index].clone();
    match &mut state.choices[index] {
        ChoiceState::Bool(value) => {
            f.checkbox(&choice.label, value);
        }
        ChoiceState::Options(selected) => {
            let popup_id = format!("choice-{}", choice.id);
            f.row()
                .gap(metrics::SPACE_S)
                .items_center()
                .show_flat(|f| {
                    f.label(&choice.label);
                    let labels: Vec<&str> = choice
                        .options
                        .iter()
                        .map(|(_, label)| label.as_str())
                        .collect();
                    let current_label = labels.get((*selected).max(0) as usize).copied().unwrap_or("");
                    if f.button(current_label) {
                        f.place_toggle(&popup_id);
                    }
                    let rect = f.response().rect;
                    let palette = state.appearance.palette();
                    f.place(
                        &popup_id,
                        &PlaceOpts {
                            mode: PlaceMode::Anchored,
                            band: Band::Popup,
                            rect,
                            transient: true,
                            ..Default::default()
                        },
                        |f| {
                        f.col()
                            .bg(palette.surface)
                            .border(palette.border)
                            .border_width(1.0)
                            .radius(metrics::RADIUS)
                            .pad(4.0)
                            .show_flat(|f| {
                                for (idx, &label) in labels.iter().enumerate() {
                                    if f.selectable(label, idx as i32 == *selected) {
                                        *selected = idx as i32;
                                        f.place_close(&popup_id);
                                    }
                                }
                            });
                    });
                });
        }
    }
}

fn collect_choices(state: &State) -> Vec<(String, String)> {
    state
        .request
        .choices
        .iter()
        .zip(&state.choices)
        .map(|(choice, value)| {
            let selected = match value {
                ChoiceState::Bool(value) => value.to_string(),
                ChoiceState::Options(index) => choice
                    .options
                    .get((*index).max(0) as usize)
                    .map(|(id, _)| id.clone())
                    .unwrap_or_default(),
            };
            (choice.id.clone(), selected)
        })
        .collect()
}

/// Answer with the highlighted row. The selected index always names an
/// offered app; `validate_for` on the backend double-checks anyway.
fn accept(state: &mut State) {
    let Some(app) = state.request.apps.get(state.selected.max(0) as usize) else {
        return;
    };
    let response = ChooseAppResponse::Selected {
        app: app.id.clone(),
        choices: collect_choices(state),
    };
    finish(state, Some(response));
}

/// `None` cancels; `Some(response)` answers.
fn finish(state: &mut State, response: Option<ChooseAppResponse>) {
    state.done = Some(response.unwrap_or(ChooseAppResponse::Cancelled));
    close_window();
}
