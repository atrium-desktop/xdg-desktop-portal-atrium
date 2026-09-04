//! The FileChooser dialog: a lens-native file browser for the portal's
//! open/save modes, modeled on GTK's file chooser. A places
//! sidebar jumps to well-known folders, the breadcrumb bar navigates
//! ancestors, and Ctrl+L (or the pencil button, or typing `/`/`~`) opens
//! a type-a-path location field with Tab completion. The toolbar carries
//! back/forward history, parent, home, and a create-folder action.
//! Directory navigation is double-click based, arrow keys move the
//! listing's cursor (selection follows, Ctrl+Space toggles in multiple
//! mode), typing selects by name, Enter activates, Backspace/Alt+Up walks
//! up, Ctrl+H toggles dotfiles, Enter accepts, saving over an existing
//! file asks for confirmation, and Escape cancels (closing the window
//! cancels too). The file under the cursor previews in a pane beside the
//! listing when its format decodes cheaply (ADR-0017).
//!
//! The location and save-name fields are plain lens text fields; after a
//! programmatic rewrite (Tab completion, a pre-filled name) the caret is
//! moved through `Frame::textfield_set_caret` (optics ADR-0064). The
//! listing is a virtualized `Frame::table_ex` with per-cell icons and a
//! host-owned cursor/selection (optics ADR-0066).

mod model;
mod preview;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atrium_portal_prompter::{
    BytePath, FileChooserMode, FileChooserRequest, FileChooserResponse, FileFilter,
    PromptAppearance, PromptResult,
};
use lens::{Align, Band, Color, Frame, Input, PlaceMode, PlaceOpts, Rect, TextBuf, key, mods};
use model::{
    Entry, History, Place, PlaceIcon, PlaceSection, breadcrumbs, common_prefix, expand_tilde,
    list_dir, normalize_lexical, split_dir_tail, typeahead_index, valid_filename,
};
use preview::{PreviewPanel, PreviewState};

use super::style::{self, ThemeInput, metrics};
use super::{
    WindowChrome, back_icon, close_window, command_held, committed_text, computer_icon,
    display_size, draw_texture_centered, escape_pressed, focus_widget, forward_icon, home_icon,
    key_pressed, modifiers, more_icon, new_folder_icon, parent_icon, raw_icon,
    run_chrome_with_lifecycle, truncate_to_width, window_title,
};

/// Double-click window for "activate" (navigate/open) gestures.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long typeahead characters accumulate before the buffer resets.
const TYPEAHEAD_WINDOW: Duration = Duration::from_millis(1000);
/// The dropdown ids whose open popups swallow Escape before the dialog.
const FILTER_DROPDOWN: &str = "chooser-filter";
/// The overwrite-confirmation modal id.
const OVERWRITE_MODAL: &str = "chooser-overwrite";

struct State {
    request: FileChooserRequest,
    appearance: ThemeInput,
    dir: PathBuf,
    entries: Vec<Entry>,
    listing_error: Option<String>,
    selected: BTreeSet<PathBuf>,
    /// The save name (`SaveFile` only), a lens text field; programmatic
    /// rewrites flag `name_caret_end` so the caret lands at the end.
    name: TextBuf,
    /// Move the save-name caret to the end when the field next builds
    /// (caret setters only resolve in the field's own id scope).
    name_caret_end: bool,
    /// The save-name field owned keyboard input last frame.
    name_field_focused: bool,
    /// Focus the save-name field on this frame (save mode starts focused,
    /// matching GTK's name entry).
    name_focus_pending: bool,
    /// The filters offered in the footer dropdown (the request's filters,
    /// or the lone `current_filter` promoted to the only choice).
    filters: Vec<FileFilter>,
    filter_index: i32,
    choices: Vec<ChoiceState>,
    show_hidden: bool,
    last_click: Option<(PathBuf, Instant)>,
    /// The sidebar shortcuts to well-known folders, resolved once at start.
    places: Vec<Place>,
    /// The right-clicked sidebar place target for context menu.
    context_place: Option<Place>,
    /// Currently dragged directory path.
    drag_source: Option<PathBuf>,
    /// Mouse coordinates when dragging began.
    drag_start: (f32, f32),
    /// Whether mouse movement has passed the drag threshold.
    drag_active: bool,
    /// Screen rect of the more-menu button for anchored popup placement.
    more_btn_rect: Rect,
    /// Screen rect of the right-clicked sidebar place item.
    context_place_rect: Rect,
    /// The type-a-path field content (while `location_editing`), a lens
    /// text field like `name`.
    location: TextBuf,
    /// Move the location caret to the end when the field next builds.
    location_caret_end: bool,
    /// Whether the location bar is a text field instead of breadcrumbs.
    location_editing: bool,
    /// The location field owned keyboard input last frame.
    location_field_focused: bool,
    /// Why the last typed path was rejected, shown under the toolbar.
    location_error: Option<String>,
    /// The row the keyboard acts on — the listing table's cursor row,
    /// host-owned through the table's in/out cursor (clicks and typeahead
    /// write it, the table's arrow keys move it).
    focus_index: Option<usize>,
    /// Focus the listing table on this frame (startup and after
    /// navigation — GTK's default list focus).
    table_focus_pending: bool,
    /// Back/forward navigation stacks.
    history: History,
    /// Typeahead buffer and when the last character arrived.
    typeahead: (String, Option<Instant>),
    /// The save target awaiting overwrite confirmation, if any.
    confirm_overwrite: Option<PathBuf>,
    /// Open the overwrite modal on this frame.
    confirm_open: bool,
    /// The new-folder row is open.
    creating_folder: bool,
    folder_name: TextBuf,
    folder_error: Option<String>,
    /// Focus the new-folder field on this frame.
    folder_focus: bool,
    /// The new-folder lens text field owned keyboard input last frame.
    folder_field_focused: bool,
    /// Whether Ctrl/Super is held this frame (multi-select modifier),
    /// sampled at the top of every build.
    ctrl_held: bool,
    /// The preselected file's basename, resolved to a listing cursor row
    /// by the first reload (the entries are not loaded at construction).
    focus_pending: Option<std::ffi::OsString>,
    /// The preview pane (ADR-0017): decodes the file under the listing
    /// cursor off-thread and draws it to the right of the listing.
    preview: PreviewPanel,
    reload: bool,
    done: Option<FileChooserResponse>,
}

enum ChoiceState {
    Bool(bool),
    Options(i32),
}

pub fn run(
    request: FileChooserRequest,
    appearance: Option<&PromptAppearance>,
) -> Result<PromptResult, String> {
    let title = requested_title(&request);
    let title = window_title(&title, Some(&request.app_id));
    let mut state = State::new(request, ThemeInput::resolve(appearance));
    state.reload_entries();
    // The lifecycle hooks capture iris's device for the preview pane's
    // texture uploads and release its textures before the device goes
    // away; the default window grows to fit the pane beside the listing.
    let state = run_chrome_with_lifecycle(
        &title,
        WindowChrome::resizable((1100, 600), (760, 420), appearance),
        state,
        |state, device| state.preview.attach_device(device),
        build,
        |state| state.preview.release(),
    )?;
    let response = state.done.unwrap_or(FileChooserResponse::Cancelled);
    Ok(PromptResult::FileChooser(response))
}

/// The dialog title: the request's, or the mode's default when empty.
fn requested_title(request: &FileChooserRequest) -> String {
    if !request.title.is_empty() {
        return request.title.clone();
    }
    match request.mode {
        FileChooserMode::OpenFile if request.multiple => "Open Files",
        FileChooserMode::OpenFile => "Open File",
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => "Choose Folder",
        FileChooserMode::SaveFile => "Save File",
    }
    .to_owned()
}

/// The accept button's default label per mode.
fn default_accept_label(mode: FileChooserMode) -> &'static str {
    match mode {
        FileChooserMode::OpenFile => "Open",
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => "Select",
        FileChooserMode::SaveFile => "Save",
    }
}

impl State {
    fn new(request: FileChooserRequest, appearance: ThemeInput) -> State {
        let filters = if request.filters.is_empty() {
            request.current_filter.iter().cloned().collect()
        } else {
            request.filters.clone()
        };
        let filter_index = request
            .current_filter
            .as_ref()
            .and_then(|current| filters.iter().position(|filter| filter == current))
            .unwrap_or(0) as i32;

        let start_file = request.current_file.as_ref().map(BytePath::to_path_buf);
        let dir = start_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| request.current_folder.as_ref().map(BytePath::to_path_buf))
            .or_else(std::env::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let selected = start_file.into_iter().collect::<BTreeSet<PathBuf>>();
        // The preselected file also takes the listing cursor, so it is
        // visibly focused (and previewed) from the first frame.
        let focus_index = selected
            .iter()
            .next()
            .and_then(|path| path.file_name())
            .map(|name| name.to_os_string());

        let initial_name = match request.mode {
            FileChooserMode::SaveFile => request
                .current_name
                .clone()
                .or_else(|| {
                    request.current_file.as_ref().and_then(|file| {
                        file.to_path_buf()
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                })
                .unwrap_or_default(),
            _ => String::new(),
        };

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

        let save_mode = matches!(request.mode, FileChooserMode::SaveFile);
        State {
            name: TextBuf::new(1024, &initial_name),
            name_caret_end: save_mode,
            name_field_focused: false,
            name_focus_pending: save_mode,
            request,
            appearance,
            dir,
            entries: Vec::new(),
            listing_error: None,
            selected,
            filters,
            filter_index,
            choices,
            show_hidden: false,
            last_click: None,
            places: model::places(),
            context_place: None,
            drag_source: None,
            drag_start: (0.0, 0.0),
            drag_active: false,
            more_btn_rect: Rect::default(),
            context_place_rect: Rect::default(),
            location: TextBuf::new(1024, ""),
            location_caret_end: false,
            location_editing: false,
            location_field_focused: false,
            location_error: None,
            focus_index: None,
            focus_pending: focus_index,
            // In save mode the name entry takes the initial focus instead.
            table_focus_pending: !save_mode,
            history: History::default(),
            typeahead: (String::new(), None),
            confirm_overwrite: None,
            confirm_open: false,
            creating_folder: false,
            folder_name: TextBuf::new(1024, ""),
            folder_error: None,
            folder_focus: false,
            folder_field_focused: false,
            ctrl_held: false,
            preview: PreviewPanel::new(),
            reload: true,
            done: None,
        }
    }

    /// Pin a folder into user bookmarks (~/.config/gtk-3.0/bookmarks).
    fn pin_path(&mut self, path: PathBuf, custom_name: Option<String>) {
        if !path.is_dir() {
            return;
        }
        if self.places.iter().any(|p| p.path == path) {
            return;
        }
        let name = custom_name.unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        });
        self.places.push(Place {
            name,
            path,
            icon: PlaceIcon::Bookmark,
            section: PlaceSection::Pinned,
            custom: true,
        });
        let _ = model::save_bookmarks(None, &self.places);
    }

    /// Unpin a folder from user bookmarks.
    fn unpin_path(&mut self, path: &Path) {
        self.places.retain(|p| !(p.custom && p.path == path));
        let _ = model::save_bookmarks(None, &self.places);
    }

    /// The filter currently narrowing the file list, if any.
    fn active_filter(&self) -> Option<&FileFilter> {
        self.filters.get(self.filter_index.max(0) as usize)
    }

    /// Whether directories (not just files) are valid selections.
    fn dirs_selectable(&self) -> bool {
        matches!(
            self.request.mode,
            FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles
        )
    }

    /// Whether more than one path may be selected.
    fn multiple_allowed(&self) -> bool {
        matches!(
            self.request.mode,
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory
        ) && self.request.multiple
    }

    fn reload_entries(&mut self) {
        match list_dir(&self.dir, self.show_hidden, self.active_filter()) {
            Ok(entries) => {
                self.entries = entries;
                self.listing_error = None;
            }
            Err(error) => {
                self.entries = Vec::new();
                self.listing_error = Some(error);
            }
        }
        // The first listing resolves the preselected file's row: give it
        // the cursor so the selection (and the preview) is visible.
        if let Some(name) = self.focus_pending.take()
            && let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.path.file_name() == Some(name.as_os_str()))
        {
            self.focus_index = Some(index);
        }
        self.reload = false;
    }

    fn navigate(&mut self, dir: PathBuf) {
        if dir != self.dir {
            let from = std::mem::replace(&mut self.dir, dir.clone());
            self.history.push(&from, &dir);
        }
        self.after_navigate();
    }

    /// Move within the back/forward stacks (no new history entry).
    fn navigate_history(&mut self, target: PathBuf) {
        self.dir = target;
        self.after_navigate();
    }

    /// The shared aftermath of every directory change.
    fn after_navigate(&mut self) {
        self.selected.clear();
        self.last_click = None;
        self.location_editing = false;
        self.location_error = None;
        self.focus_index = None;
        // The listing table's id derives from the directory, so its
        // retained scroll position is per-directory; only the lens focus
        // needs re-granting — unless the save-name field keeps it.
        self.table_focus_pending = !self.name_field_focused;
        self.typeahead.0.clear();
        self.reload = true;
    }
}

/// The paths an accept would return, or `None` when the current state is
/// not acceptable. Pure so it is testable without a window.
fn accept_paths(
    mode: FileChooserMode,
    dir: &Path,
    selected: &BTreeSet<PathBuf>,
    save_name: &str,
) -> Option<Vec<PathBuf>> {
    match mode {
        FileChooserMode::OpenFile => {
            if selected.is_empty() {
                None
            } else {
                Some(selected.iter().cloned().collect())
            }
        }
        // Choosing a folder with nothing selected targets the folder being
        // browsed, matching GTK's SelectFolder.
        FileChooserMode::OpenDirectory => Some(if selected.is_empty() {
            vec![dir.to_path_buf()]
        } else {
            selected.iter().cloned().collect()
        }),
        FileChooserMode::SaveFile => {
            let name = save_name.trim();
            if valid_filename(name) {
                Some(vec![dir.join(name)])
            } else {
                None
            }
        }
        FileChooserMode::SaveFiles => Some(vec![dir.to_path_buf()]),
    }
}

fn accept_valid(state: &State) -> bool {
    accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        &state.name.as_str(),
    )
    .is_some()
}

fn accept(state: &mut State) {
    accept_checked(state, false);
}

/// Accept the current selection. In save mode an existing target first
/// asks for overwrite confirmation; `force` skips that check (the
/// confirmation modal's Replace button).
fn accept_checked(state: &mut State, force: bool) {
    let Some(paths) = accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        &state.name.as_str(),
    ) else {
        return;
    };
    if !force
        && state.request.mode == FileChooserMode::SaveFile
        && paths.first().is_some_and(|path| path.exists())
    {
        state.confirm_overwrite = Some(paths[0].clone());
        state.confirm_open = true;
        return;
    }
    let result = state
        .request
        .finish_paths(paths)
        .map(|paths| FileChooserResponse::Selected {
            paths: paths.into_iter().map(BytePath::from).collect(),
            current_filter: state.active_filter().cloned(),
            choices: collect_choices(state),
        });
    finish(
        state,
        result.unwrap_or_else(|message| FileChooserResponse::Failed { message }),
    );
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

fn finish(state: &mut State, response: FileChooserResponse) {
    state.done = Some(response);
    close_window();
}

/// Open the location field seeded with the current folder's path.
fn start_location_edit(state: &mut State) {
    let seed = state.dir.to_string_lossy().into_owned();
    open_location(state, seed);
}

/// Open the location field with `seed` as content, caret at the end.
fn open_location(state: &mut State, seed: String) {
    state.location_editing = true;
    state.location_error = None;
    state.location.set(&seed);
    state.location_caret_end = true;
}

/// Set the save name programmatically, caret to the end.
fn set_name(state: &mut State, name: &str) {
    state.name.set(name);
    state.name_caret_end = true;
}

/// Resolve the typed location: a directory navigates, an existing file
/// selects (or seeds the save name), anything else reports inline why the
/// path cannot be used.
fn go_location(state: &mut State) {
    let typed = state.location.as_str().trim().to_owned();
    if typed.is_empty() {
        return;
    }
    let expanded = expand_tilde(&typed, std::env::home_dir().as_deref());
    let full = if expanded.is_absolute() {
        expanded
    } else {
        state.dir.join(&expanded)
    };
    let full = normalize_lexical(&full);
    if full.is_dir() {
        state.navigate(full);
        return;
    }
    let Some(parent) = full.parent().filter(|parent| parent.is_dir()) else {
        state.location_error = Some(format!("No such location: {}", full.display()));
        return;
    };
    if state.request.mode == FileChooserMode::SaveFile {
        // Existing file or new name alike: offer the tail as the save name.
        if let Some(name) = full.file_name() {
            let name = name.to_string_lossy().into_owned();
            state.navigate(parent.to_path_buf());
            set_name(state, &name);
        }
    } else if full.is_file() {
        state.navigate(parent.to_path_buf());
        if state.request.mode == FileChooserMode::OpenFile {
            state.selected.clear();
            state.selected.insert(full);
        }
    } else {
        state.location_error = Some(format!("No such file: {}", full.display()));
    }
}

/// Tab-complete the location field against the typed directory's entries:
/// a single match completes in full (with a trailing `/` for folders),
/// several matches complete the longest common prefix.
fn complete_location(state: &mut State) {
    let typed = state.location.as_str().into_owned();
    if typed.is_empty() {
        return;
    }
    let (dir_part, tail) = split_dir_tail(&typed);
    let expanded = expand_tilde(dir_part, std::env::home_dir().as_deref());
    let base = if expanded.as_os_str().is_empty() {
        state.dir.clone()
    } else if expanded.is_absolute() {
        expanded
    } else {
        state.dir.join(expanded)
    };
    let base = normalize_lexical(&base);
    let Ok(entries) = list_dir(&base, tail.starts_with('.'), None) else {
        return;
    };
    let matches: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.name.starts_with(tail))
        .collect();
    let completed = match matches.as_slice() {
        [] => None,
        [only] => {
            let mut completed = format!("{dir_part}{}", only.name);
            if only.is_dir {
                completed.push('/');
            }
            Some(completed)
        }
        many => {
            let prefix = common_prefix(many.iter().map(|entry| entry.name.as_str()));
            (prefix.len() > tail.len()).then(|| format!("{dir_part}{prefix}"))
        }
    };
    if let Some(completed) = completed {
        state.location.set(&completed);
        state.location_caret_end = true;
    }
}

/// Create the typed folder inside the current directory and enter it.
fn create_folder(state: &mut State) {
    let name = state.folder_name.as_str().trim().to_owned();
    if !valid_filename(&name) {
        state.folder_error = Some("Enter a name without /".to_owned());
        return;
    }
    let target = state.dir.join(&name);
    match std::fs::create_dir(&target) {
        Ok(()) => {
            state.creating_folder = false;
            state.folder_error = None;
            state.navigate(target);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            state.folder_error = Some(format!("{name} already exists"));
        }
        Err(error) => {
            state.folder_error = Some(format!("Could not create {name}: {error}"));
        }
    }
}

/// Set the keyboard cursor on a row; selection follows unless Ctrl is
/// held (then only the cursor moves and Ctrl+Space toggles, like GTK).
fn focus_to(state: &mut State, index: usize) {
    state.focus_index = Some(index);
    let Some(entry) = state.entries.get(index).cloned() else {
        return;
    };
    if state.ctrl_held {
        return;
    }
    match state.request.mode {
        FileChooserMode::OpenFile if !entry.is_dir => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
        }
        FileChooserMode::SaveFile if !entry.is_dir => {
            set_name(state, &entry.name);
        }
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles if entry.is_dir => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
        }
        _ => {}
    }
}

/// Toggle the focused row's selection (Ctrl+Space in multiple mode).
fn toggle_focused(state: &mut State) {
    let Some(entry) = state
        .focus_index
        .and_then(|index| state.entries.get(index))
        .cloned()
    else {
        return;
    };
    if entry.is_dir && !state.dirs_selectable() {
        return;
    }
    if !entry.is_dir && state.request.mode != FileChooserMode::OpenFile {
        return;
    }
    if !state.selected.remove(&entry.path) {
        state.selected.insert(entry.path);
    }
}

/// The double-click semantics, shared with Enter: folders open, files
/// open directly (or seed the save name).
fn activate_entry(state: &mut State, entry: &Entry) {
    if entry.is_dir {
        state.navigate(entry.path.clone());
        return;
    }
    match state.request.mode {
        FileChooserMode::OpenFile => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
            accept(state);
        }
        FileChooserMode::SaveFile => {
            set_name(state, &entry.name);
        }
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => {}
    }
}

/// GTK's search-as-you-type: accumulate characters for a moment and move
/// the cursor to the first entry whose name starts with the buffer.
fn typeahead(state: &mut State, text: &str) {
    let now = Instant::now();
    let fresh = state
        .typeahead
        .1
        .is_some_and(|when| now.duration_since(when) < TYPEAHEAD_WINDOW);
    if !fresh {
        state.typeahead.0.clear();
    }
    state.typeahead.0.push_str(text);
    state.typeahead.1 = Some(now);
    let prefix = state.typeahead.0.clone();
    if let Some(index) = typeahead_index(&state.entries, &prefix) {
        focus_to(state, index);
    }
}

fn icon_tool_button_rect(
    f: &mut Frame,
    id: &str,
    palette: &style::Palette,
    enabled: bool,
    icon: impl Fn(&mut Frame),
) -> (bool, lens::Rect) {
    f.size_next(metrics::CONTROL_HEIGHT, metrics::CONTROL_HEIGHT);
    if !enabled {
        f.push_style(style::muted_style_for(palette));
    }
    let (response, ()) = f
        .row()
        .radius(metrics::RADIUS)
        .bg(palette.material)
        .border(palette.material_border)
        .items_center()
        .id(id)
        .show(|f| {
            f.centered(metrics::CONTROL_HEIGHT, metrics::CONTROL_HEIGHT, |f| {
                icon(f);
            });
        });
    if !enabled {
        f.pop_style();
    }
    (enabled && response.clicked, response.rect)
}

fn icon_nav_button(
    f: &mut Frame,
    id: &str,
    palette: &style::Palette,
    enabled: bool,
    icon: impl Fn(&mut Frame),
) -> bool {
    f.size_next(metrics::CONTROL_HEIGHT, metrics::CONTROL_HEIGHT);
    if !enabled {
        f.push_style(style::muted_style_for(palette));
    }
    let (response, ()) = f
        .row()
        .radius(metrics::RADIUS_SM)
        .items_center()
        .id(id)
        .show(|f| {
            f.centered(metrics::CONTROL_HEIGHT, metrics::CONTROL_HEIGHT, |f| {
                icon(f);
            });
        });
    if !enabled {
        f.pop_style();
    }
    enabled && response.clicked
}

fn button_with_icon(
    f: &mut Frame,
    id: &str,
    palette: &style::Palette,
    enabled: bool,
    icon: impl Fn(&mut Frame),
    label: &str,
) -> (bool, Rect) {
    if !enabled {
        f.push_style(style::muted_style_for(palette));
    }
    let (response, ()) = f
        .row()
        .height(metrics::CONTROL_HEIGHT)
        .radius(metrics::RADIUS)
        .bg(palette.material)
        .border(palette.material_border)
        .pad(metrics::SPACE_S)
        .gap(metrics::SPACE_S)
        .items_center()
        .id(id)
        .show(|f| {
            icon(f);
            if !label.is_empty() {
                f.label(label);
            }
        });
    if !enabled {
        f.pop_style();
    }
    (enabled && response.clicked, response.rect)
}

/// Whether a transient dropdown popup or context menu is open (it swallows Escape).
fn popup_open(state: &State, f: &mut Frame) -> bool {
    let filter_open = f.place_is_open(FILTER_DROPDOWN);
    let place_ctx_open = f.place_is_open("place-context");
    let more_menu_open = f.place_is_open("more-menu");
    filter_open
        || place_ctx_open
        || more_menu_open
        || state
            .request
            .choices
            .iter()
            .any(|choice| f.place_is_open(&format!("choice-{}", choice.id)))
}

/// All keyboard processing for the dialog, run before rendering so the
/// frame reflects the keys pressed this frame. The lens text fields and
/// the listing table own their keys while focused (the table reports
/// arrow/Return handling through its result); dropdowns and the overwrite
/// modal own Escape while open; what remains is dialog-level routing.
fn handle_keys(state: &mut State, f: &mut Frame, input: &Input) {
    let popup = popup_open(state, f);
    if escape_pressed(input) {
        if state.confirm_overwrite.is_some() {
            // The overwrite modal owns the first Escape.
            f.place_close(OVERWRITE_MODAL);
            state.confirm_overwrite = None;
        } else if !popup {
            if state.location_editing {
                state.location_editing = false;
                state.location_error = None;
            } else if state.creating_folder {
                state.creating_folder = false;
                state.folder_error = None;
            } else {
                finish(state, FileChooserResponse::Cancelled);
            }
        }
        return;
    }
    if command_held(input) && key_pressed(input, 'h' as i32) {
        state.show_hidden = !state.show_hidden;
        state.reload = true;
    }
    if command_held(input) && key_pressed(input, 'l' as i32) {
        start_location_edit(state);
    }
    if state.confirm_overwrite.is_some() {
        // The overwrite modal owns the remaining keys; Return confirms the
        // replacement (its default action), mirroring GTK.
        if key_pressed(input, key::RETURN) {
            f.place_close(OVERWRITE_MODAL);
            state.confirm_overwrite = None;
            accept_checked(state, true);
        }
        return;
    }
    if popup {
        // Dropdowns own the remaining keys.
        return;
    }
    if state.location_editing {
        // The location field owns the remaining keys while the mode is
        // open; Tab completes the typed path (Return resolves it through
        // the field's `clicked` response at build).
        if state.location_field_focused && key_pressed(input, key::TAB) {
            complete_location(state);
        }
        return;
    }
    if state.name_field_focused || state.folder_field_focused {
        // The lens text fields own the remaining keys while focused.
        return;
    }
    if modifiers(input) & mods::ALT != 0 && key_pressed(input, key::UP) {
        // Alt+Up walks up, mirroring GTK.
        if let Some(parent) = state.dir.parent().map(Path::to_path_buf) {
            state.navigate(parent);
        }
        return;
    }
    if key_pressed(input, key::UP) {
        let prev = match state.focus_index {
            Some(i) if i > 0 => i - 1,
            Some(_) => 0,
            None => 0,
        };
        if !state.entries.is_empty() {
            focus_to(state, prev);
        }
        return;
    }
    if key_pressed(input, key::DOWN) {
        let next = match state.focus_index {
            Some(i) if i + 1 < state.entries.len() => i + 1,
            Some(i) => i,
            None => 0,
        };
        if !state.entries.is_empty() {
            focus_to(state, next);
        }
        return;
    }
    if key_pressed(input, key::HOME) {
        if !state.entries.is_empty() {
            focus_to(state, 0);
        }
        return;
    }
    if key_pressed(input, key::END) {
        if !state.entries.is_empty() {
            focus_to(state, state.entries.len() - 1);
        }
        return;
    }
    if key_pressed(input, key::RETURN) {
        if let Some(index) = state.focus_index
            && let Some(entry) = state.entries.get(index).cloned()
        {
            activate_entry(state, &entry);
            return;
        }
        if accept_valid(state) {
            accept(state);
        }
        return;
    }
    if key_pressed(input, key::BACKSPACE)
        && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
    {
        state.navigate(parent);
        return;
    }
    if command_held(input) && key_pressed(input, ' ' as i32) && state.multiple_allowed() {
        toggle_focused(state);
        return;
    }
    let text = committed_text(input);
    if !text.is_empty() && !command_held(input) {
        // Typing `/` or `~` opens the location field, like GTK; anything
        // else searches the listing by name.
        if text.starts_with('/') || text.starts_with('~') {
            open_location(state, text);
        } else {
            typeahead(state, &text);
        }
    }
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));
    let palette = state.appearance.palette();
    state.ctrl_held = command_held(input);

    let cursor_pos = (input.as_raw().cursor.x, input.as_raw().cursor.y);
    let mouse_left_down = input.as_raw().mouse_down[0];
    let mouse_left_released = input.as_raw().mouse_released[0];

    if mouse_left_down && state.drag_source.is_some() {
        let dx = cursor_pos.0 - state.drag_start.0;
        let dy = cursor_pos.1 - state.drag_start.1;
        if dx * dx + dy * dy > 16.0 {
            state.drag_active = true;
        }
    } else if mouse_left_released || !mouse_left_down {
        if state.drag_active
            && cursor_pos.0 >= 0.0
            && cursor_pos.0 <= metrics::SIDEBAR_WIDTH + metrics::SPACE_M * 2.0
            && let Some(source) = state.drag_source.take()
        {
            state.pin_path(source, None);
        }
        state.drag_source = None;
        state.drag_active = false;
    }

    if state.reload {
        state.reload_entries();
    }
    handle_keys(state, f, input);
    if state.done.is_some() {
        return;
    }

    f.col()
        .gap(metrics::SPACE_S)
        .pad(metrics::SPACE_M)
        .cross(Align::Stretch)
        .flex(1.0)
        .show_flat(|f| {
            // ---- location toolbar --------------------------------------
            f.row()
                .gap(metrics::SPACE_M)
                .height(metrics::FIELD_HEIGHT)
                .items_center()
                .show_flat(|f| {
                    // Navigation controls group [ ←  →  ↑ ]
                    f.row()
                        .height(metrics::CONTROL_HEIGHT)
                        .rounded(metrics::RADIUS)
                        .bg(palette.material)
                        .border(palette.material_border)
                        .pad(2.0)
                        .gap(1.0)
                        .items_center()
                        .show_flat(|f| {
                            let current_dir = state.dir.clone();
                            if icon_nav_button(
                                f,
                                "go-back",
                                &palette,
                                state.history.back().is_some(),
                                |f| back_icon(f, metrics::ICON_SMALL),
                            ) && let Some(target) = state.history.go_back(&current_dir)
                            {
                                state.navigate_history(target);
                            }
                            if icon_nav_button(
                                f,
                                "go-forward",
                                &palette,
                                state.history.forward().is_some(),
                                |f| forward_icon(f, metrics::ICON_SMALL),
                            ) && let Some(target) = state.history.go_forward(&current_dir)
                            {
                                state.navigate_history(target);
                            }
                            if icon_nav_button(
                                f,
                                "go-parent",
                                &palette,
                                state.dir.parent().is_some(),
                                |f| parent_icon(f, metrics::ICON_SMALL),
                            ) && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
                            {
                                state.navigate(parent);
                            }
                        });

                    // Center path / breadcrumb container
                    f.row()
                        .flex(if state.location_editing { 1.0 } else { 0.0 })
                        .height(metrics::CONTROL_HEIGHT)
                        .rounded(metrics::RADIUS)
                        .bg(palette.material)
                        .border(palette.material_border)
                        .pad(metrics::SPACE_XS)
                        .gap(metrics::SPACE_S)
                        .items_center()
                        .show_flat(|f| {
                            if state.location_editing {
                                f.flex(1.0);
                                f.textfield_placeholder(
                                    "location-path",
                                    &mut state.location,
                                    "Type a path…",
                                );
                                if state.location_caret_end {
                                    f.textfield_set_caret("location-path", u32::MAX);
                                    state.location_caret_end = false;
                                }
                                let response = f.response();
                                state.location_field_focused = response.focused;
                                if !response.focused {
                                    focus_widget(f, "location-path");
                                }
                                if response.clicked {
                                    go_location(state);
                                }
                            } else {
                                breadcrumb(state, f);
                            }
                        });

                    if !state.location_editing {
                        f.flex(1.0);
                        f.spacer(0.0);
                    }

                    // Right toolbar actions: [ New Folder ] and [ ⋮ More ]
                    let (new_folder_clicked, _) = button_with_icon(
                        f,
                        "btn-new-folder",
                        &palette,
                        true,
                        |f| new_folder_icon(f, metrics::ICON_SMALL),
                        "New Folder",
                    );
                    if new_folder_clicked {
                        state.creating_folder = !state.creating_folder;
                        state.folder_focus = state.creating_folder;
                        state.folder_error = None;
                        state.folder_name.set("");
                    }

                    let (more_clicked, more_rect) =
                        icon_tool_button_rect(f, "more-menu-btn", &palette, true, |f| {
                            more_icon(f, metrics::ICON_SMALL)
                        });
                    state.more_btn_rect = more_rect;
                    if more_clicked {
                        f.place_toggle("more-menu");
                    }
                });
            if state.location_editing
                && let Some(error) = state.location_error.clone()
            {
                f.push_style(style::error_style_for(&palette));
                f.label_sized(&error, metrics::FONT_SMALL);
                f.pop_style();
            }

            // ---- new folder ---------------------------------------------
            if state.creating_folder {
                f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                    f.label("Folder name:");
                    f.size_next(260.0, metrics::FIELD_HEIGHT);
                    f.textfield_placeholder("folder-name", &mut state.folder_name, "New folder");
                    let response = f.response();
                    state.folder_field_focused = response.focused;
                    if state.folder_focus {
                        focus_widget(f, "folder-name");
                        state.folder_focus = false;
                    }
                    if response.clicked {
                        create_folder(state);
                    }
                    f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                    if f.button("Create") {
                        create_folder(state);
                    }
                });
                if let Some(error) = state.folder_error.clone() {
                    f.push_style(style::error_style_for(&palette));
                    f.label_sized(&error, metrics::FONT_SMALL);
                    f.pop_style();
                }
            }

            // ---- save name (SaveFile only) ------------------------------
            if state.request.mode == FileChooserMode::SaveFile {
                f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                    f.label("Name:");
                    f.flex(1.0);
                    f.textfield_placeholder("save-name", &mut state.name, "File name");
                    if state.name_caret_end {
                        f.textfield_set_caret("save-name", u32::MAX);
                        state.name_caret_end = false;
                    }
                    let response = f.response();
                    state.name_field_focused = response.focused;
                    if state.name_focus_pending {
                        focus_widget(f, "save-name");
                        state.name_focus_pending = false;
                    }
                    if response.clicked && accept_valid(state) {
                        accept(state);
                    }
                });
            }

            // ---- places sidebar + directory listing + preview pane ------
            f.row()
                .gap(metrics::SPACE_S)
                .flex(1.0)
                .cross(Align::Stretch)
                .show_flat(|f| {
                    // The places rail: clean, flat, and compact, separated from the browsing column by a hairline.
                    f.col().width(170.0).cross(Align::Stretch).show_flat(|f| {
                        let standard_places: Vec<(usize, Place)> = state
                            .places
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| p.section == PlaceSection::Standard)
                            .map(|(i, p)| (i, p.clone()))
                            .collect();

                        let pinned_places: Vec<(usize, Place)> = state
                            .places
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| p.section == PlaceSection::Pinned)
                            .map(|(i, p)| (i, p.clone()))
                            .collect();

                        if !standard_places.is_empty() {
                            sidebar_section_header(f, "PLACES", &palette);
                        }

                        f.scroll("chooser-places", |f| {
                            let content_w = 170.0 - 8.0;
                            f.col().width(content_w).gap(1.0).show_flat(|f| {
                                for (index, place) in &standard_places {
                                    place_row(state, f, *index, place);
                                }

                                if !pinned_places.is_empty() || state.drag_active {
                                    f.spacer(metrics::SPACE_XS);
                                    sidebar_section_header(f, "PINNED", &palette);
                                    for (index, place) in &pinned_places {
                                        place_row(state, f, *index, place);
                                    }
                                    if state.drag_active {
                                        drop_indicator(f, &palette);
                                    }
                                }
                            });
                        });
                    });
                    f.col()
                        .flex(1.0)
                        .cross(Align::Stretch)
                        .gap(metrics::SPACE_XS)
                        .show_flat(|f| {
                            if let Some(error) = state.listing_error.clone() {
                                f.push_style(style::error_style_for(&palette));
                                f.label_sized(&error, metrics::FONT_SMALL);
                                f.pop_style();
                            }
                            if state.entries.is_empty() && state.listing_error.is_none() {
                                f.push_style(style::small_muted_style_for(&palette));
                                f.label_sized("This folder is empty", metrics::FONT_SMALL);
                                f.pop_style();
                            }
                            // Clean table header matching design
                            f.row()
                                .height(24.0)
                                .items_center()
                                .pad(metrics::SPACE_XS)
                                .show_flat(|f| {
                                    f.push_style(style::small_muted_style_for(&palette));
                                    f.flex(1.0);
                                    f.label("Name ▾");
                                    f.size_next(100.0, 20.0);
                                    f.label("Size");
                                    f.size_next(160.0, 20.0);
                                    f.label("Modified");
                                    f.pop_style();
                                });
                            f.flex(1.0);
                            // The table's id derives from the directory, so
                            // its retained scroll position is
                            // per-directory (back/forward restores it).
                            let table_id = format!("chooser-list:{}", state.dir.display());
                            if state.table_focus_pending {
                                focus_widget(f, &table_id);
                                state.table_focus_pending = false;
                            }
                            let entries = &state.entries;
                            let selected = &state.selected;
                            let mut clicked_idx = None;
                            f.scroll(&table_id, |f| {
                                f.col().gap(1.0).cross(Align::Stretch).show_flat(|f| {
                                    for (idx, entry) in entries.iter().enumerate() {
                                        let is_selected = selected.contains(&entry.path);
                                        let icon = if entry.is_dir {
                                            lens::sys::lens_icon_id::LENS_ICON_FOLDER
                                        } else {
                                            lens::sys::lens_icon_id::LENS_ICON_FILE
                                        };
                                        f.row()
                                            .height(metrics::ROW_HEIGHT)
                                            .items_center()
                                            .show_flat(|f| {
                                                f.flex(1.0);
                                                if f.selectable_icon(&entry.name, icon, is_selected)
                                                {
                                                    clicked_idx = Some(idx);
                                                }
                                                f.size_next(100.0, metrics::ROW_HEIGHT);
                                                f.push_style(style::muted_style_for(&palette));
                                                f.label_sized(
                                                    &entry.size_display(),
                                                    metrics::FONT_SMALL,
                                                );
                                                f.pop_style();
                                                f.size_next(160.0, metrics::ROW_HEIGHT);
                                                f.push_style(style::muted_style_for(&palette));
                                                f.label_sized(
                                                    &entry.modified_display(),
                                                    metrics::FONT_SMALL,
                                                );
                                                f.pop_style();
                                            });
                                    }
                                });
                            });
                            if let Some(idx) = clicked_idx {
                                handle_click(state, idx);
                            }
                            if !state.typeahead.0.is_empty() {
                                f.push_style(style::small_muted_style_for(&palette));
                                f.label_sized(
                                    &format!("Search: {}", state.typeahead.0),
                                    metrics::FONT_SMALL,
                                );
                                f.pop_style();
                            }
                        });
                    // The preview pane (ADR-0017): the file under the
                    // listing cursor, when its format decodes cheaply.
                    preview_pane(state, f, input);
                });

            if state.request.mode == FileChooserMode::SaveFiles {
                f.push_style(style::small_muted_style_for(&palette));
                f.label_sized(
                    &format!("New files will be created in {}", state.dir.display()),
                    metrics::FONT_SMALL,
                );
                f.pop_style();
            }

            // ---- embedded choices ----------------------------------------
            for index in 0..state.request.choices.len() {
                choice_row(state, f, index);
            }

            // ---- footer: filter + buttons --------------------------------
            f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                if !state.filters.is_empty() {
                    let mut labels: Vec<&str> = state
                        .filters
                        .iter()
                        .map(|filter| filter.label.as_str())
                        .collect();
                    labels.truncate(16);
                    let current_label = labels
                        .get(state.filter_index.max(0) as usize)
                        .copied()
                        .unwrap_or("All files");
                    let filter_label = format!("{current_label} ▾");
                    if f.button(&filter_label) {
                        f.place_toggle(FILTER_DROPDOWN);
                    }
                    let filter_rect = f.response().rect;
                    f.place(
                        FILTER_DROPDOWN,
                        &PlaceOpts {
                            mode: PlaceMode::Anchored,
                            band: Band::Popup,
                            rect: filter_rect,
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
                                        if f.selectable(label, idx as i32 == state.filter_index) {
                                            state.filter_index = idx as i32;
                                            state.focus_index = None;
                                            state.reload = true;
                                            f.place_close(FILTER_DROPDOWN);
                                        }
                                    }
                                });
                        },
                    );
                }

                f.flex(1.0);
                f.spacer(0.0);

                f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                f.push_style(style::secondary_button_style_for(&palette));
                let cancel = f.button("Cancel");
                f.pop_style();
                if cancel {
                    finish(state, FileChooserResponse::Cancelled);
                    return;
                }

                f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                let label = state
                    .request
                    .accept_label
                    .as_deref()
                    .map(style::plain_label)
                    .unwrap_or_else(|| default_accept_label(state.request.mode));
                let valid = accept_valid(state);
                if valid && state.done.is_none() && f.button(label) {
                    accept(state);
                } else if !valid {
                    // Build the disabled-looking button anyway so the
                    // layout does not jump when it becomes valid.
                    f.push_style(style::disabled_button_style_for(&palette));
                    f.button(label);
                    f.pop_style();
                }
            });
        });

    // ---- overwrite confirmation ----------------------------------------
    let was_open = f.place_is_open(OVERWRITE_MODAL);
    if state.confirm_overwrite.is_some() && !was_open && !state.confirm_open {
        // Dismissed without an answer (click outside).
        state.confirm_overwrite = None;
    }
    if state.confirm_open {
        f.place_open(OVERWRITE_MODAL);
        state.confirm_open = false;
    }
    if state.confirm_overwrite.is_some() && f.place_is_open(OVERWRITE_MODAL) {
        let target_name = state
            .confirm_overwrite
            .as_ref()
            .and_then(|target| target.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        f.place(
            OVERWRITE_MODAL,
            &PlaceOpts {
                mode: PlaceMode::Centered,
                band: Band::Modal,
                transient: true,
                ..Default::default()
            },
            |f| {
                f.col()
                    .gap(metrics::SPACE_M)
                    .pad(metrics::SPACE_L)
                    .bg(palette.surface)
                    .border(palette.border)
                    .border_width(1.0)
                    .radius(metrics::RADIUS)
                    .min_width(340.0)
                    .show_flat(|f| {
                        f.push_style(style::title_style());
                        f.label("Replace File?");
                        f.pop_style();

                        f.label(&format!(
                            "A file named \"{target_name}\" already exists. \
                             Do you want to replace it?"
                        ));
                        f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                            f.flex(1.0);
                            f.spacer(0.0);
                            f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                            f.push_style(style::secondary_button_style_for(&palette));
                            let cancel = f.button("Cancel");
                            f.pop_style();
                            if cancel {
                                f.place_close(OVERWRITE_MODAL);
                                state.confirm_overwrite = None;
                            }
                            f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                            if f.button("Replace") {
                                f.place_close(OVERWRITE_MODAL);
                                state.confirm_overwrite = None;
                                accept_checked(state, true);
                            }
                        });
                    });
            },
        );
    }

    // ---- context menu for places ---------------------------------------
    f.place(
        "place-context",
        &PlaceOpts {
            mode: PlaceMode::Anchored,
            band: Band::Popup,
            rect: state.context_place_rect,
            transient: true,
            ..Default::default()
        },
        |f| {
            f.col()
                .gap(2.0)
                .pad(4.0)
                .bg(palette.surface)
                .border(palette.border)
                .border_width(1.0)
                .radius(metrics::RADIUS)
                .show_flat(|f| {
                    if let Some(target) = state.context_place.clone() {
                        if f.selectable("Open", false) {
                            state.navigate(target.path.clone());
                            f.place_close("place-context");
                        }
                        if f.selectable("Copy Path", false) {
                            f.copy(&target.path.to_string_lossy());
                            f.place_close("place-context");
                        }
                        if target.custom {
                            f.separator();
                            if f.selectable("Remove from Bookmarks", false) {
                                state.unpin_path(&target.path);
                                f.place_close("place-context");
                            }
                        }
                    }
                });
        },
    );

    // ---- context menu for more options ---------------------------------
    f.place(
        "more-menu",
        &PlaceOpts {
            mode: PlaceMode::Anchored,
            band: Band::Popup,
            rect: state.more_btn_rect,
            transient: true,
            ..Default::default()
        },
        |f| {
            f.col()
                .gap(2.0)
                .pad(4.0)
                .bg(palette.surface)
                .border(palette.border)
                .border_width(1.0)
                .radius(metrics::RADIUS)
                .show_flat(|f| {
                    if f.selectable("Show Hidden Files", state.show_hidden) {
                        state.show_hidden = !state.show_hidden;
                        state.reload = true;
                        f.place_close("more-menu");
                    }
                    if f.selectable("Type Path", false) {
                        start_location_edit(state);
                        f.place_close("more-menu");
                    }
                    if f.selectable("Reload", false) {
                        state.reload = true;
                        f.place_close("more-menu");
                    }
                    f.separator();
                    let current_dir = state.dir.clone();
                    let is_pinned = state.places.iter().any(|p| p.path == current_dir);
                    if is_pinned {
                        if f.selectable("Remove from Places", false) {
                            state.unpin_path(&current_dir);
                            f.place_close("more-menu");
                        }
                    } else if f.selectable("Add to Places", false) {
                        state.pin_path(current_dir, None);
                        f.place_close("more-menu");
                    }
                });
        },
    );

    // Backspace walks up one folder when no text field owns the key.
    if key_pressed(input, key::BACKSPACE)
        && !state.location_editing
        && !state.name_field_focused
        && !state.folder_field_focused
        && !popup_open(state, f)
        && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
    {
        state.navigate(parent);
    }
}

/// The ancestor chain as clickable chips, oldest first, truncated to the
/// last four components. The current folder is filled and inert; clicking
/// an ancestor navigates to it.
fn breadcrumb(state: &mut State, f: &mut Frame) {
    let palette = state.appearance.palette();
    let chain = breadcrumbs(&state.dir);
    let home = std::env::home_dir();
    let is_under_home = home.as_ref().is_some_and(|h| state.dir.starts_with(h));

    if is_under_home {
        home_icon(f, metrics::ICON_SMALL);
    } else {
        computer_icon(f, metrics::ICON_SMALL);
    }

    let visible_chain: Vec<&PathBuf> = if chain.len() > 1 {
        chain.iter().filter(|p| p.parent().is_some()).collect()
    } else {
        chain.iter().collect()
    };

    let hidden = visible_chain.len().saturating_sub(4);
    if hidden > 0 {
        f.push_style(style::muted_style_for(&palette));
        f.label("…");
        f.pop_style();
    }
    let last = visible_chain.len().saturating_sub(1);
    for (position, component) in visible_chain.into_iter().skip(hidden).enumerate() {
        if position > 0 || hidden > 0 {
            f.push_style(style::muted_style_for(&palette));
            f.label("/");
            f.pop_style();
        }
        let current = hidden + position == last;
        crumb_button(
            state,
            f,
            component,
            position,
            current,
            Color::rgba(40, 70, 130, 160),
        );
    }
}

/// One breadcrumb segment: the root shows a drive glyph, folders show
/// their name truncated to a measured pixel budget.
fn crumb_button(
    state: &mut State,
    f: &mut Frame,
    component: &Path,
    position: usize,
    current: bool,
    active_bg: Color,
) {
    let is_root = component.parent().is_none();
    let palette = state.appearance.palette();
    let name = truncate_to_width(f, &crumb_name(component), metrics::CRUMB_MAX_W);
    if !current {
        f.push_style(style::muted_style_for(&palette));
    }
    let crumb_id = format!("crumb-{position}");
    let (response, ()) = f
        .row()
        .gap(metrics::SPACE_XS)
        .pad(metrics::SPACE_XS)
        .min_height(metrics::CRUMB_HEIGHT)
        .items_center()
        .bg(if current {
            active_bg
        } else {
            Color::TRANSPARENT
        })
        .rounded(metrics::RADIUS_SM)
        .id(&crumb_id)
        .show(|f| {
            if is_root {
                f.label("/");
            } else {
                f.label(&name);
            }
        });
    if !current {
        f.pop_style();
    }
    if response.clicked && !current {
        state.navigate(component.to_path_buf());
    }
}

/// A crumb's display text: the folder's name (the root crumb, drawn as an
/// icon, would read "/").
fn crumb_name(component: &Path) -> String {
    component
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_owned())
}

/// The preview pane to the right of the listing (ADR-0017): the file under
/// the listing cursor, when its format decodes cheaply (PNG, JPEG, GIF,
/// WebP, BMP). Non-previewable targets collapse the pane entirely so
/// browsing keeps the full width; the pane also hides on windows too
/// narrow to fit it beside the listing. The pane is presentation only —
/// it never changes the selection or the accept result.
fn preview_pane(state: &mut State, f: &mut Frame, input: &Input) {
    let palette = state.appearance.palette();
    // Too narrow to be worth the split: keep the whole width for browsing.
    let (window_w, _) = display_size(input);
    if window_w > 0.0 && window_w < metrics::PREVIEW_MIN_WINDOW_W {
        state.preview.state_for(None);
        return;
    }
    let target = state
        .focus_index
        .and_then(|index| state.entries.get(index))
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.path.clone());
    let pane = state.preview.state_for(target.as_deref());
    let entry_name = target
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    match pane {
        PreviewState::Hidden => {}
        PreviewState::Loading | PreviewState::Failed { .. } | PreviewState::Ready { .. } => {
            f.separator();
            // The preview renders as a card (Finder's preview plate): the
            // material wash and card radius lift it off the listing plane.
            f.col()
                .radius(metrics::RADIUS_CARD)
                .width(metrics::PREVIEW_WIDTH)
                .gap(metrics::SPACE_S)
                .pad(metrics::SPACE_XS)
                .show_flat(|f| match pane {
                    PreviewState::Ready {
                        texture,
                        source_size,
                        file_bytes,
                    } => {
                        // The image box flexes to fill the pane's height;
                        // the caption row below keeps its intrinsic size.
                        f.flex(1.0);
                        f.scroll("chooser-preview-image", |f| {
                            let (w, h) = preview_image_box();
                            draw_texture_centered(f, &texture, w, h);
                        });
                        preview_caption(
                            f,
                            &palette,
                            entry_name.as_deref(),
                            Some((source_size, file_bytes)),
                        );
                    }
                    PreviewState::Loading => {
                        f.push_style(style::small_muted_style_for(&palette));
                        f.label_sized("Decoding preview…", metrics::FONT_SMALL);
                        f.pop_style();
                    }
                    PreviewState::Failed { reason } => {
                        f.push_style(style::small_muted_style_for(&palette));
                        f.label_wrapped_sized(&reason, metrics::FONT_SMALL, metrics::PREVIEW_WIDTH);
                        f.pop_style();
                        preview_caption(f, &palette, entry_name.as_deref(), None);
                    }
                    PreviewState::Hidden => {}
                });
        }
    }
}

/// The image box for the preview: the pane's content width and a height
/// allowance the scroll clips around. The drawn image is aspect-fitted to
/// this box by `draw_texture_centered`, so a tall or wide photo centers
/// instead of stretching.
fn preview_image_box() -> (f32, f32) {
    // The scroll's viewport height is not known before the first layout
    // pass; the fixed allowance plus clipping keeps the box stable across
    // frames without measuring probes.
    (
        metrics::PREVIEW_WIDTH - 2.0 * metrics::SPACE_XS,
        metrics::PREVIEW_IMAGE_HEIGHT,
    )
}

/// The quiet caption under the preview: the file name (truncated), and —
/// when the decode succeeded — its pixel dimensions and size on disk,
/// both captured at decode time so no frame stats the file.
fn preview_caption(
    f: &mut Frame,
    palette: &style::Palette,
    name: Option<&str>,
    details: Option<((u32, u32), u64)>,
) {
    if let Some(name) = name {
        let shown = truncate_to_width(f, name, metrics::PREVIEW_WIDTH - 2.0 * metrics::SPACE_XS);
        f.push_style(style::small_muted_style_for(palette));
        f.label_sized(&shown, metrics::FONT_SMALL);
        f.pop_style();
    }
    let mut parts = Vec::new();
    if let Some(((w, h), bytes)) = details {
        parts.push(format!("{w} × {h}"));
        parts.push(preview::format_size(bytes));
    }
    if !parts.is_empty() {
        f.push_style(style::small_muted_style_for(palette));
        f.label_sized(&parts.join(" · "), metrics::FONT_SMALL);
        f.pop_style();
    }
}

fn sidebar_section_header(f: &mut Frame, title: &str, palette: &style::Palette) {
    f.push_style(style::small_muted_style_for(palette));
    f.row()
        .height(metrics::SIDEBAR_HEADER_HEIGHT)
        .items_center()
        .pad(2.0)
        .show_flat(|f| {
            f.label_sized(title, 11.0);
        });
    f.pop_style();
}

fn drop_indicator(f: &mut Frame, palette: &style::Palette) {
    f.row()
        .gap(metrics::SPACE_S)
        .pad(metrics::SPACE_XS)
        .min_height(metrics::SIDEBAR_ROW_HEIGHT)
        .items_center()
        .bg(palette.hover)
        .rounded(metrics::RADIUS_SM)
        .show_flat(|f| {
            raw_icon(
                f,
                lens::sys::lens_icon_id::LENS_ICON_FOLDER_PLUS,
                metrics::ICON_SMALL,
            );
            f.push_style(style::small_muted_style_for(palette));
            f.label_sized("Drop to Pin", metrics::FONT_SMALL);
            f.pop_style();
        });
}

fn place_label(place: &Place) -> &str {
    if place.section == PlaceSection::Standard {
        match place.icon {
            PlaceIcon::Home => "Home",
            PlaceIcon::Desktop => "Desktop",
            PlaceIcon::Documents => "Documents",
            PlaceIcon::Downloads => "Downloads",
            PlaceIcon::Music => "Music",
            PlaceIcon::Pictures => "Pictures",
            PlaceIcon::Videos => "Videos",
            PlaceIcon::Computer => "Computer",
            PlaceIcon::Bookmark => &place.name,
        }
    } else {
        &place.name
    }
}

/// One sidebar shortcut row, highlighted when it is the browsed folder.
fn place_row(state: &mut State, f: &mut Frame, index: usize, place: &Place) {
    let active = state.dir == place.path;
    let palette = state.appearance.palette();
    if !active {
        f.push_style(style::muted_style_for(&palette));
    }
    let label = place_label(place);
    let (response, ()) = f
        .row()
        .gap(metrics::SPACE_S)
        .pad(metrics::SPACE_XS)
        .min_height(metrics::SIDEBAR_ROW_HEIGHT)
        .items_center()
        .bg(if active {
            Color::rgba(35, 60, 110, 160)
        } else {
            Color::TRANSPARENT
        })
        .rounded(metrics::RADIUS_SM)
        .id(&format!("place-{index}"))
        .show(|f| {
            place_icon(f, place.icon);
            f.label(label);
        });
    if !active {
        f.pop_style();
    }
    if response.pressed && state.drag_source.is_none() {
        state.drag_source = Some(place.path.clone());
        state.drag_active = false;
    }
    if response.right_clicked {
        f.place_open("place-context");
        state.context_place = Some(place.clone());
        state.context_place_rect = response.rect;
    }
    if response.clicked && !active {
        state.navigate(place.path.clone());
    }
}

/// The glyph for a sidebar place.
fn place_icon(f: &mut Frame, icon: PlaceIcon) {
    use lens::sys::lens_icon_id as id;
    let icon = match icon {
        PlaceIcon::Home => return home_icon(f, metrics::ICON_SMALL),
        PlaceIcon::Computer => return computer_icon(f, metrics::ICON_SMALL),
        PlaceIcon::Desktop => id::LENS_ICON_MONITOR,
        PlaceIcon::Documents => id::LENS_ICON_FILE_TEXT,
        PlaceIcon::Downloads => id::LENS_ICON_DOWNLOAD,
        PlaceIcon::Music => id::LENS_ICON_MUSIC,
        PlaceIcon::Pictures => id::LENS_ICON_IMAGE,
        PlaceIcon::Videos => id::LENS_ICON_FILM,
        PlaceIcon::Bookmark => id::LENS_ICON_BOOKMARK,
    };
    raw_icon(f, icon, metrics::ICON_SMALL);
}

/// Single click moves the keyboard cursor and selects (Ctrl toggles in
/// multiple mode); double click enters a folder or opens a file directly.
fn handle_click(state: &mut State, index: usize) {
    let Some(entry) = state.entries.get(index).cloned() else {
        return;
    };
    state.focus_index = Some(index);
    if entry.is_dir {
        state.drag_source = Some(entry.path.clone());
        state.drag_active = false;
    }
    let now = Instant::now();
    let double = state.last_click.as_ref().is_some_and(|(path, when)| {
        *path == entry.path && now.duration_since(*when) < DOUBLE_CLICK
    });
    state.last_click = Some((entry.path.clone(), now));

    if double {
        activate_entry(state, &entry);
        return;
    }
    if entry.is_dir {
        if state.dirs_selectable() {
            select(state, &entry.path);
        }
        return;
    }

    match state.request.mode {
        FileChooserMode::OpenFile => select(state, &entry.path),
        // Clicking a file while saving offers its name, like GTK.
        FileChooserMode::SaveFile => set_name(state, &entry.name),
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => {}
    }
}

/// Apply a click to the selection set: Ctrl toggles in multiple mode,
/// otherwise the clicked path becomes the whole selection.
fn select(state: &mut State, path: &Path) {
    if state.multiple_allowed() {
        if command_held_click(state) {
            if !state.selected.remove(path) {
                state.selected.insert(path.to_path_buf());
            }
        } else if !(state.selected.len() == 1 && state.selected.contains(path)) {
            state.selected.clear();
            state.selected.insert(path.to_path_buf());
        }
    } else {
        state.selected.clear();
        state.selected.insert(path.to_path_buf());
    }
}

/// Whether the current click event carries the multi-select modifier.
/// Read from the pressable row's own frame input, stashed on the state by
/// the build closure (a plain bool beats threading `Input` through).
fn command_held_click(state: &State) -> bool {
    state.ctrl_held
}

/// One embedded FileChooser choice: a boolean checkbox, or a labeled
/// dropdown of option labels.
fn choice_row(state: &mut State, f: &mut Frame, index: usize) {
    let choice = state.request.choices[index].clone();
    match &mut state.choices[index] {
        ChoiceState::Bool(value) => {
            f.checkbox(&choice.label, value);
        }
        ChoiceState::Options(selected) => {
            let popup_id = format!("choice-{}", choice.id);
            f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
                f.label(&choice.label);
                let labels: Vec<&str> = choice
                    .options
                    .iter()
                    .map(|(_, label)| label.as_str())
                    .collect();
                let current_label = labels
                    .get((*selected).max(0) as usize)
                    .copied()
                    .unwrap_or("");
                if f.button(current_label) {
                    f.place_toggle(&popup_id);
                }
                let btn_rect = f.response().rect;
                let palette = state.appearance.palette();
                f.place(
                    &popup_id,
                    &PlaceOpts {
                        mode: PlaceMode::Anchored,
                        band: Band::Popup,
                        rect: btn_rect,
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
                    },
                );
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected_of(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn open_file_requires_a_selection() {
        let dir = Path::new("/tmp");
        assert!(accept_paths(FileChooserMode::OpenFile, dir, &BTreeSet::new(), "").is_none());
        let selected = selected_of(&["/tmp/a.txt"]);
        assert_eq!(
            accept_paths(FileChooserMode::OpenFile, dir, &selected, ""),
            Some(vec![PathBuf::from("/tmp/a.txt")])
        );
    }

    #[test]
    fn open_directory_falls_back_to_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(FileChooserMode::OpenDirectory, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }

    #[test]
    fn save_file_rejects_empty_and_escaped_names() {
        let dir = Path::new("/tmp");
        assert!(accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "  ").is_none());
        assert!(accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "../x").is_none());
        assert_eq!(
            accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "out.txt"),
            Some(vec![PathBuf::from("/tmp/out.txt")])
        );
    }

    #[test]
    fn save_files_targets_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(FileChooserMode::SaveFiles, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }

    #[test]
    fn crumb_names_map_path_components() {
        assert_eq!(crumb_name(Path::new("/")), "/");
        assert_eq!(crumb_name(Path::new("/home/ming")), "ming");
    }
}

/// Headless interaction tests: the dialog's `build` runs on a headless
/// lens `Ui` with synthetic inputs, exercising the real keyboard routing,
/// the listing table's cursor/activation contract, and the lens text
/// fields — no windowing system involved.
#[cfg(test)]
mod ui_tests {
    use super::*;
    use lens::{Input, Ui, key, mods};

    /// A temporary directory tree for the dialog to browse: two folders
    /// first (dirs sort first), then two files.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root =
                std::env::temp_dir().join(format!("tessera-fc-ui-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("alpha")).unwrap();
            std::fs::create_dir_all(root.join("beta")).unwrap();
            std::fs::write(root.join("notes.txt"), "n").unwrap();
            std::fs::write(root.join("report.pdf"), "r").unwrap();
            Fixture(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn request(mode: FileChooserMode, fixture: &Fixture) -> FileChooserRequest {
        FileChooserRequest {
            mode,
            app_id: "dev.tessera.Test".to_owned(),
            title: String::new(),
            accept_label: None,
            modal: false,
            parent_window: None,
            multiple: false,
            current_folder: Some(BytePath::from(fixture.0.clone())),
            current_name: None,
            current_file: None,
            filters: vec![],
            current_filter: None,
            choices: vec![],
            files: vec![],
        }
    }

    fn fresh_state(mode: FileChooserMode, fixture: &Fixture) -> State {
        let mut state = State::new(request(mode, fixture), ThemeInput::resolve(None));
        state.reload_entries();
        state
    }

    fn frame(ui: &mut Ui, state: &mut State, input: &Input) {
        ui.frame(input, |f| build(state, f, input));
    }

    /// Idle frames so retained state (focus, the table's viewport) settles.
    fn settle(ui: &mut Ui, state: &mut State) {
        let input = Input::new((920.0, 540.0), 0.016);
        for _ in 0..3 {
            frame(ui, state, &input);
        }
    }

    fn tap(ui: &mut Ui, state: &mut State, key: i32) {
        let mut input = Input::new((920.0, 540.0), 0.016);
        input.push_key(key, true, false);
        frame(ui, state, &input);
    }

    fn tap_with_mods(ui: &mut Ui, state: &mut State, key: i32, mask: u32) {
        let mut input = Input::new((920.0, 540.0), 0.016);
        input.set_mods(mask);
        input.push_key(key, true, false);
        frame(ui, state, &input);
    }

    fn type_text(ui: &mut Ui, state: &mut State, text: &str) {
        let mut input = Input::new((920.0, 540.0), 0.016);
        input.set_text(text);
        frame(ui, state, &input);
    }

    fn selected_paths(state: &State) -> Vec<PathBuf> {
        state.selected.iter().cloned().collect()
    }

    /// Write a small two-tone PNG so the preview path decodes real bytes.
    fn write_png(path: &Path, width: u32, height: u32) {
        let mut rgba = image::RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let even = (x / 8 + y / 8) % 2 == 0;
                rgba.put_pixel(
                    x,
                    y,
                    image::Rgba([if even { 240 } else { 32 }, 96, 160, 255]),
                );
            }
        }
        rgba.save(path).expect("png fixture writes");
    }

    #[test]
    fn table_keyboard_drives_cursor_selection_and_activation() {
        let fixture = Fixture::new("kbd");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        settle(&mut ui, &mut state);
        assert_eq!(state.focus_index, None);

        // Rows: alpha/ beta/ notes.txt report.pdf (dirs first). The table
        // is focused by default, so arrows move its cursor.
        tap(&mut ui, &mut state, key::DOWN);
        assert_eq!(state.focus_index, Some(0));
        tap(&mut ui, &mut state, key::DOWN);
        assert_eq!(state.focus_index, Some(1));
        tap(&mut ui, &mut state, key::DOWN);
        assert_eq!(state.focus_index, Some(2));
        // Selection follows the cursor onto files.
        assert_eq!(selected_paths(&state), vec![fixture.0.join("notes.txt")]);

        tap(&mut ui, &mut state, key::END);
        assert_eq!(state.focus_index, Some(3));
        tap(&mut ui, &mut state, key::HOME);
        assert_eq!(state.focus_index, Some(0));

        // Enter activates the cursor row: a folder navigates.
        tap(&mut ui, &mut state, key::RETURN);
        assert_eq!(state.dir, fixture.0.join("alpha"));
        assert_eq!(state.focus_index, None);

        // Enter on a file accepts the dialog.
        tap(&mut ui, &mut state, key::END);
        tap(&mut ui, &mut state, key::UP);
        tap(&mut ui, &mut state, key::UP);
        // alpha/ is the only entry; go back up for a file instead.
        state.navigate(fixture.0.clone());
        settle(&mut ui, &mut state);
        tap(&mut ui, &mut state, key::DOWN);
        tap(&mut ui, &mut state, key::DOWN);
        tap(&mut ui, &mut state, key::DOWN);
        assert_eq!(state.focus_index, Some(2));
        tap(&mut ui, &mut state, key::RETURN);
        match &state.done {
            Some(FileChooserResponse::Selected { paths, .. }) => {
                assert_eq!(paths, &vec![BytePath::from(fixture.0.join("notes.txt"))]);
            }
            other => panic!("expected selection, got {other:?}"),
        }
    }

    #[test]
    fn typeahead_jumps_the_cursor() {
        let fixture = Fixture::new("typeahead");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        settle(&mut ui, &mut state);

        type_text(&mut ui, &mut state, "rep");
        assert_eq!(state.focus_index, Some(3));
        assert_eq!(selected_paths(&state), vec![fixture.0.join("report.pdf")]);
    }

    #[test]
    fn ctrl_space_toggles_multi_selection() {
        let fixture = Fixture::new("multi");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        state.request.multiple = true;
        settle(&mut ui, &mut state);

        tap(&mut ui, &mut state, key::DOWN);
        tap(&mut ui, &mut state, key::DOWN);
        tap(&mut ui, &mut state, key::DOWN);
        assert_eq!(selected_paths(&state), vec![fixture.0.join("notes.txt")]);
        // Ctrl+Down moves only the cursor; Ctrl+Space toggles its row.
        tap_with_mods(&mut ui, &mut state, key::DOWN, mods::CTRL);
        assert_eq!(state.focus_index, Some(3));
        assert_eq!(selected_paths(&state), vec![fixture.0.join("notes.txt")]);
        tap_with_mods(&mut ui, &mut state, ' ' as i32, mods::CTRL);
        let mut expected = vec![fixture.0.join("notes.txt"), fixture.0.join("report.pdf")];
        expected.sort();
        assert_eq!(selected_paths(&state), expected);
    }

    #[test]
    fn location_field_completes_and_navigates() {
        let fixture = Fixture::new("location");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        settle(&mut ui, &mut state);

        tap_with_mods(&mut ui, &mut state, 'l' as i32, mods::CTRL);
        assert!(state.location_editing);
        // The seeded path is the browsed folder; typing appends at the end.
        settle(&mut ui, &mut state);
        type_text(&mut ui, &mut state, "/al");
        tap(&mut ui, &mut state, key::TAB);
        let completed = state.location.as_str().into_owned();
        assert!(
            completed.ends_with("alpha/"),
            "unexpected completion: {completed}"
        );
        // Tab moved lens focus away; completion pulls it back, then Return
        // resolves the path.
        settle(&mut ui, &mut state);
        tap(&mut ui, &mut state, key::RETURN);
        assert_eq!(state.dir, fixture.0.join("alpha"));
        assert!(!state.location_editing);
    }

    #[test]
    fn save_name_prefilled_with_caret_at_end() {
        let fixture = Fixture::new("savename");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::SaveFile, &fixture);
        state.name.set("report.pdf");
        state.name_focus_pending = true;
        settle(&mut ui, &mut state);
        assert!(state.name_field_focused);

        type_text(&mut ui, &mut state, "x");
        assert_eq!(state.name.as_str(), "report.pdfx");

        tap(&mut ui, &mut state, key::RETURN);
        match &state.done {
            Some(FileChooserResponse::Selected { paths, .. }) => {
                assert_eq!(paths, &vec![BytePath::from(fixture.0.join("report.pdfx"))]);
            }
            other => panic!("expected selection, got {other:?}"),
        }
    }

    #[test]
    fn escape_cancels() {
        let fixture = Fixture::new("escape");
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        settle(&mut ui, &mut state);
        tap(&mut ui, &mut state, key::ESCAPE);
        assert!(matches!(state.done, Some(FileChooserResponse::Cancelled)));
    }

    #[test]
    fn preview_pane_loads_decodes_and_caches_images() {
        let fixture = Fixture::new("preview");
        // A real PNG alongside the plain fixture files; dirs sort first,
        // then files alphabetically: notes.txt, photo.png, report.pdf.
        write_png(&fixture.0.join("photo.png"), 40, 20);
        let mut ui = Ui::headless().unwrap();
        let mut state = fresh_state(FileChooserMode::OpenFile, &fixture);
        state.reload_entries();
        settle(&mut ui, &mut state);

        // No cursor: the pane hides without touching the worker.
        assert!(matches!(
            state.preview.state_for(None),
            preview::PreviewState::Hidden
        ));

        // The cursor lands on the PNG: a decode is requested (no device
        // in headless mode, so the texture never uploads, but the decode
        // itself runs on the worker and must round-trip).
        type_text(&mut ui, &mut state, "pho");
        // Rows: alpha/ beta/ notes.txt photo.png report.pdf.
        assert_eq!(state.focus_index, Some(3));
        let target = fixture.0.join("photo.png");
        assert!(matches!(
            state.preview.state_for(Some(&target)),
            preview::PreviewState::Loading
        ));
        // The worker finishes; draining admits the decode (headless has
        // no device, so the upload is skipped but the pending target
        // clears only through the channel — poll until it settles).
        let mut decoded = false;
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            let _ = state.preview.state_for(Some(&target));
            if state.preview.pending_is_none() {
                decoded = true;
                break;
            }
        }
        assert!(decoded, "the worker decode never completed");
        // The decode result itself is validated by the pure decode test
        // below; here the contract is the state machine: still loading in
        // headless (no device to upload through), never failed.
        assert!(matches!(
            state.preview.state_for(Some(&target)),
            preview::PreviewState::Loading
        ));

        // Moving to a non-image hides the pane again.
        type_text(&mut ui, &mut state, "not");
        let notes = fixture.0.join("notes.txt");
        assert!(matches!(
            state.preview.state_for(Some(&notes)),
            preview::PreviewState::Hidden
        ));
    }

    #[test]
    fn preselected_file_takes_the_cursor_and_previews() {
        let fixture = Fixture::new("preselect");
        write_png(&fixture.0.join("photo.png"), 40, 20);
        let mut req = request(FileChooserMode::OpenFile, &fixture);
        req.current_file = Some(BytePath::from(fixture.0.join("photo.png")));
        let mut state = State::new(req, ThemeInput::resolve(None));
        state.reload_entries();

        // The listing's cursor row is the preselected file
        // (alpha/ beta/ notes.txt photo.png report.pdf → index 3).
        assert_eq!(state.focus_index, Some(3));
        assert_eq!(selected_paths(&state), vec![fixture.0.join("photo.png")]);

        // The headless pane requests its decode without a device.
        let target = fixture.0.join("photo.png");
        assert!(matches!(
            state.preview.state_for(Some(&target)),
            preview::PreviewState::Loading
        ));
        // A non-existent preselection (deleted between request and dialog)
        // leaves the cursor unset, not panicked.
        let mut req = request(FileChooserMode::OpenFile, &fixture);
        req.current_file = Some(BytePath::from(fixture.0.join("gone.png")));
        let mut state = State::new(req, ThemeInput::resolve(None));
        state.reload_entries();
        assert_eq!(state.focus_index, None);
    }

    #[test]
    fn preview_decode_downsamples_real_pngs() {
        let fixture = Fixture::new("preview-decode");
        // A PNG larger than the texture budget: the decode must cap it.
        let big = fixture.0.join("big.png");
        write_png(&big, 1400, 700);
        let decoded = preview::decode_preview(&big).expect("large png decodes");
        assert_eq!(decoded.source_size, (1400, 700));
        assert_eq!((decoded.width, decoded.height), (672, 336));
        assert_eq!(decoded.pixels.len(), 672 * 336 * 4);
        // The caption's size rides the decode; frames never stat the file.
        assert!(decoded.file_bytes > 0);
        // A small PNG decodes at its natural size.
        let small = fixture.0.join("small.png");
        write_png(&small, 60, 40);
        let decoded = preview::decode_preview(&small).expect("small png decodes");
        assert_eq!((decoded.width, decoded.height), (60, 40));
        // A non-image refuses with a reason, not a panic.
        let reason = match preview::decode_preview(&fixture.0.join("notes.txt")) {
            Err(reason) => reason,
            Ok(_) => panic!("text files do not decode"),
        };
        assert!(reason.contains("no preview"), "unexpected reason: {reason}");
    }
}
