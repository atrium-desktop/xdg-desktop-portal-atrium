//! The DynamicLauncher PrepareInstall dialog: the proposed launcher name
//! in a text field (read-only when the request forbids editing), the web
//! app's URL and a short icon note when present, and Cancel plus Install
//! buttons. Enter in the field installs, Escape or the window's close
//! button cancels. The name field is a plain lens text field like the file
//! chooser's save-name entry; the app-owned `edit.rs` surfaces exist for
//! fields that need programmatic caret moves, which this one never does.
//!
//! The icon renders as its label only: the icon arrives as a themed name
//! or a variant tag on the wire, not as bytes the dialog could turn into
//! a texture, so the backend echoes it back in the portal results (the
//! same call as the app chooser's name-only rows). Decoding *files* is
//! the file chooser preview pane's job (ADR-0017).

use atrium_portal_prompter::{
    LauncherEditRequest, LauncherEditResponse, PromptAppearance, PromptResult,
};
use lens::{Frame, Input, TextBuf, key};

use super::sizing;
use super::style::{self, ThemeInput, metrics};
use super::{
    WindowChrome, close_window, display_size, escape_pressed, focus_widget, key_pressed,
    run_window_with_chrome, window_title,
};

/// The name field's widget id (one dialog per prompter process).
const NAME_FIELD: &str = "launcher-name";

struct State {
    request: LauncherEditRequest,
    appearance: ThemeInput,
    name: TextBuf,
    /// The lens text field owned keyboard input last frame.
    name_field_focused: bool,
    /// Focus the name field on the first frame.
    name_focus_pending: bool,
    done: Option<LauncherEditResponse>,
}

pub fn run(
    request: LauncherEditRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = window_title(&request.title, Some(&request.app_id));
    let editable = request.editable_name;
    let intro = format!(
        "The application '{}' will be added to your applications.",
        request.app_id
    );
    let size = sizing::launcher_edit_size(
        &request.title,
        &intro,
        request.target.as_deref(),
        request.icon_label.as_deref(),
    );
    let state = State {
        name: TextBuf::new(1024, &request.name),
        name_field_focused: false,
        name_focus_pending: editable,
        request,
        appearance: ThemeInput::resolve(appearance),
        done: None,
    };
    let state = run_window_with_chrome(
        &title,
        WindowChrome::fixed_to(size, appearance),
        state,
        build,
    )?;
    // Closing the window without answering is a cancellation, matching the
    // other dialogs' delete-event semantics.
    let response = state.done.unwrap_or(LauncherEditResponse::Cancelled);
    Ok(PromptResult::LauncherEdit(response))
}

/// The name an install would save; empty means not installable.
fn current_name(state: &State) -> Option<String> {
    let name = state.name.as_str().trim().to_owned();
    (!name.is_empty()).then_some(name)
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    if escape_pressed(input) {
        finish(state, LauncherEditResponse::Cancelled);
        return;
    }
    // Enter in the name field installs, mirroring the file chooser's save
    // name field.
    if state.name_field_focused && key_pressed(input, key::RETURN) {
        if let Some(name) = current_name(state) {
            finish(state, LauncherEditResponse::Saved { name });
        }
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
            f.label_wrapped(
                &format!(
                    "The application '{}' will be added to your applications.",
                    state.request.app_id
                ),
                width.max(120.0),
            );
            if let Some(target) = state.request.target.clone() {
                f.label_wrapped(&format!("Web app: {target}"), width.max(120.0));
            }
            if let Some(icon_label) = state.request.icon_label.clone() {
                f.label_wrapped(&format!("Icon: {icon_label}"), width.max(120.0));
            }
            f.pop_style();

            // ---- name --------------------------------------------------
            f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                f.label("Name:");
                f.flex(1.0);
                if state.request.editable_name {
                    f.textfield_placeholder(NAME_FIELD, &mut state.name, "Launcher name");
                    let response = f.response();
                    state.name_field_focused = response.focused;
                    if state.name_focus_pending {
                        focus_widget(f, NAME_FIELD);
                        state.name_focus_pending = false;
                    }
                } else {
                    f.label(&state.request.name.clone());
                }
            });

            f.flex(1.0);
            f.spacer(0.0);

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
                    finish(state, LauncherEditResponse::Cancelled);
                    return;
                }

                f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                let name = current_name(state);
                if let Some(name) = name {
                    if f.button("Install") {
                        finish(state, LauncherEditResponse::Saved { name });
                    }
                } else {
                    // Build the disabled-looking button anyway so the
                    // layout does not jump when a name appears.
                    f.push_style(style::disabled_button_style_for(
                        &state.appearance.palette(),
                    ));
                    f.button("Install");
                    f.pop_style();
                }
            });
        });
}

fn finish(state: &mut State, response: LauncherEditResponse) {
    state.done = Some(response);
    close_window();
}
