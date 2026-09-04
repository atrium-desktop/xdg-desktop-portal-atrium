//! Stable process contract for the portal's one-shot prompter.
//!
//! The contract uses JSON over anonymous pipes. Paths are byte arrays rather
//! than UTF-8 strings so every Unix filename accepted by the FileChooser
//! portal round-trips without loss.

#![forbid(unsafe_code)]

pub mod notify;

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// Version of the private stdin/stdout contract. The backend and prompter
/// reject mismatches instead of interpreting fields using different schemas.
/// Version 6 adds the request-level `appearance` snapshot (compositor
/// desktop preferences for the dialog's look); version 5 added the
/// `choose_source` prompt kind; version 4 added the app chooser and
/// launcher editor.
pub const PROCESS_CONTRACT_VERSION: u32 = 6;

/// The compositor-owned appearance snapshot every prompt renders with:
/// the desktop preferences that decide a dialog's palette and motion,
/// projected by the backend from its settings store. All fields are
/// optional so the backend can omit the whole snapshot (for example when
/// the compositor IPC was unavailable at startup); the prompter then
/// falls back to its own platform query.
///
/// This is a deliberate local projection of the compositor's
/// `DesktopPreferences` (Tessera IPC `GetSettings`), not a dependency on the
/// `atrium-portal-ipc` crate: the prompter process stays independent of
/// the backend's wire stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptAppearance {
    /// The compositor's resolved colour scheme request (`"system"`,
    /// `"dark"`, `"light"`), mirroring Tessera `ColorScheme` on the wire.
    pub color_scheme: PromptColorScheme,
    /// The user's accent colour as 8-bit RGB, when the compositor
    /// publishes one; `None` keeps the palette's built-in accent.
    pub accent_color: Option<PromptAccent>,
    /// Whether contrast-boosted text is requested.
    pub high_contrast: bool,
    /// Whether animations should be minimised (accessibility).
    pub reduced_motion: bool,
}

/// The colour-scheme half of [`PromptAppearance`]. Values match the
/// compositor's kebab-case wire values so the backend can forward its own
/// enum through serde without a mapping step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptColorScheme {
    /// Follow the platform; the prompter resolves this itself.
    #[default]
    System,
    Dark,
    Light,
}

/// The accent-colour half of [`PromptAppearance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptAccent {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl PromptAppearance {
    /// Validate the snapshot's invariants. The fields are plain enums and
    /// numbers, so only the accent needs a rule: a fully-transparent
    /// accent (0,0,0 with alpha implied) would blacken every accent
    /// surface, so it is rejected rather than rendered.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(accent) = self.accent_color
            && accent.red == 0
            && accent.green == 0
            && accent.blue == 0
        {
            return Err("appearance accent colour must not be black-transparent".into());
        }
        Ok(())
    }
}

/// Versioned wire envelope sent to a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrompterRequest {
    pub version: u32,
    pub prompt: PromptRequest,
    /// Compositor desktop preferences for this dialog's look. Absent
    /// (`null`) means "no snapshot available": the prompter resolves the
    /// scheme from the platform and applies its default palette.
    #[serde(default)]
    pub appearance: Option<PromptAppearance>,
}

impl PrompterRequest {
    #[must_use]
    pub fn new(request: FileChooserRequest) -> Self {
        Self::file_chooser(request)
    }

    #[must_use]
    pub fn file_chooser(request: FileChooserRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::FileChooser(request),
            appearance: None,
        }
    }

    /// Attach the compositor appearance snapshot the dialog should render
    /// with. Builders compose: `PrompterRequest::confirm(r).with_appearance(a)`.
    #[must_use]
    pub fn with_appearance(mut self, appearance: PromptAppearance) -> Self {
        self.appearance = Some(appearance);
        self
    }

    #[must_use]
    pub fn confirm(confirm: ConfirmRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::Confirm(confirm),
            appearance: None,
        }
    }

    #[must_use]
    pub fn secret(secret: SecretRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::Secret(secret),
            appearance: None,
        }
    }

    #[must_use]
    pub fn choose_app(choose_app: ChooseAppRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::ChooseApp(choose_app),
            appearance: None,
        }
    }

    #[must_use]
    pub fn choose_source(choose_source: ChooseSourceRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::ChooseSource(choose_source),
            appearance: None,
        }
    }

    #[must_use]
    pub fn launcher_edit(launcher_edit: LauncherEditRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::LauncherEdit(launcher_edit),
            appearance: None,
        }
    }

    pub fn into_prompt(self) -> Result<PromptRequest, String> {
        self.validate()?;
        Ok(self.prompt)
    }

    /// Validate the envelope's version and payload. Mirrors
    /// [`PrompterRequest::into_prompt`] without consuming the value, so
    /// the dialog host can check a decoded request before dispatch.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != PROCESS_CONTRACT_VERSION {
            return Err(format!(
                "unsupported prompter request version {}; expected {}",
                self.version, PROCESS_CONTRACT_VERSION
            ));
        }
        if let Some(appearance) = &self.appearance {
            appearance.validate()?;
        }
        self.prompt.validate()
    }

    pub fn into_file_chooser(self) -> Result<FileChooserRequest, String> {
        match self.into_prompt()? {
            PromptRequest::FileChooser(request) => Ok(request),
            _ => Err("prompter request is not a file chooser request".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum PromptRequest {
    FileChooser(FileChooserRequest),
    Confirm(ConfirmRequest),
    Secret(SecretRequest),
    ChooseApp(ChooseAppRequest),
    ChooseSource(ChooseSourceRequest),
    LauncherEdit(LauncherEditRequest),
}

impl PromptRequest {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::FileChooser(request) => request.validate(),
            Self::Confirm(confirm) => confirm.validate(),
            Self::Secret(secret) => secret.validate(),
            Self::ChooseApp(choose_app) => choose_app.validate(),
            Self::ChooseSource(choose_source) => choose_source.validate(),
            Self::LauncherEdit(launcher_edit) => launcher_edit.validate(),
        }
    }
}

/// Versioned wire envelope returned by a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrompterResponse {
    pub version: u32,
    pub result: PromptResult,
}

impl PrompterResponse {
    #[must_use]
    pub fn new(request: FileChooserResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::FileChooser(request),
        }
    }

    #[must_use]
    pub fn confirm(confirm: ConfirmResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Confirm(confirm),
        }
    }

    #[must_use]
    pub fn secret(secret: SecretResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Secret(secret),
        }
    }

    #[must_use]
    pub fn choose_app(choose_app: ChooseAppResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::ChooseApp(choose_app),
        }
    }

    #[must_use]
    pub fn choose_source(choose_source: ChooseSourceResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::ChooseSource(choose_source),
        }
    }

    #[must_use]
    pub fn launcher_edit(launcher_edit: LauncherEditResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::LauncherEdit(launcher_edit),
        }
    }

    #[must_use]
    pub fn failed(message: String) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Failed { message },
        }
    }

    pub fn into_result(self) -> Result<PromptResult, String> {
        if self.version != PROCESS_CONTRACT_VERSION {
            return Err(format!(
                "unsupported prompter response version {}; expected {}",
                self.version, PROCESS_CONTRACT_VERSION
            ));
        }
        Ok(self.result)
    }

    pub fn into_file_chooser(self) -> Result<FileChooserResponse, String> {
        match self.into_result()? {
            PromptResult::FileChooser(request) => Ok(request),
            PromptResult::Failed { message } => Err(message),
            _ => Err("prompter response is not a file chooser response".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum PromptResult {
    FileChooser(FileChooserResponse),
    Confirm(ConfirmResponse),
    Secret(SecretResponse),
    ChooseApp(ChooseAppResponse),
    ChooseSource(ChooseSourceResponse),
    LauncherEdit(LauncherEditResponse),
    Failed { message: String },
}

const MAX_PROMPT_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    pub title: String,
    pub body: String,
    pub accept_label: Option<String>,
    pub deny_label: Option<String>,
    pub modal: bool,
    pub parent_window: Option<String>,
}

impl ConfirmRequest {
    /// Reject malformed values before any dialog is shown. Public so the
    /// backend can double-check a composed body against the contract's
    /// text cap before enqueueing.
    pub fn validate(&self) -> Result<(), String> {
        validate_prompt_text("confirmation title", &self.title, false)?;
        validate_prompt_text("confirmation body", &self.body, false)?;
        if let Some(label) = self.accept_label.as_deref() {
            validate_prompt_text("confirmation accept label", label, false)?;
        }
        if let Some(label) = self.deny_label.as_deref() {
            validate_prompt_text("confirmation deny label", label, true)?;
        }
        if let Some(parent) = self.parent_window.as_deref() {
            validate_prompt_text("confirmation parent window", parent, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConfirmResponse {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequest {
    pub title: String,
    pub reason: Option<String>,
}

impl SecretRequest {
    fn validate(&self) -> Result<(), String> {
        validate_prompt_text("secret title", &self.title, false)?;
        if let Some(reason) = self.reason.as_deref() {
            validate_prompt_text("secret reason", reason, true)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SecretResponse {
    Secret { value: String },
    Cancelled,
}

impl SecretResponse {
    #[must_use]
    pub fn take_value(&mut self) -> Option<String> {
        match self {
            Self::Secret { value } => Some(std::mem::take(value)),
            Self::Cancelled => None,
        }
    }
}

impl std::fmt::Debug for SecretResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Secret { .. } => formatter.write_str("SecretResponse::Secret([REDACTED])"),
            Self::Cancelled => formatter.write_str("SecretResponse::Cancelled"),
        }
    }
}

impl Drop for SecretResponse {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        if let Self::Secret { value } = self {
            value.zeroize();
        }
    }
}

fn validate_prompt_text(name: &str, value: &str, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > MAX_PROMPT_TEXT_BYTES
        || value.chars().any(|character| character == '\0')
    {
        return Err(format!("{name} is empty, oversized, or contains NUL"));
    }
    Ok(())
}

/// The choice-list validation shared by FileChooser and AppChooser: unique
/// non-empty ids and labels, non-empty unique option pairs, and a selected
/// value that names an offered option (or `true`/`false` for a boolean
/// check button).
fn validate_choices(choices: &[Choice]) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    for choice in choices {
        if choice.id.is_empty() || choice.label.is_empty() {
            return Err("choice ids and labels must not be empty".into());
        }
        if !ids.insert(choice.id.as_str()) {
            return Err(format!("duplicate choice id {:?}", choice.id));
        }
        if choice
            .options
            .iter()
            .any(|(id, label)| id.is_empty() || label.is_empty())
        {
            return Err(format!("choice {:?} contains an empty option", choice.id));
        }
        let mut option_ids = std::collections::BTreeSet::new();
        if choice
            .options
            .iter()
            .any(|(id, _)| !option_ids.insert(id.as_str()))
        {
            return Err(format!("choice {:?} has duplicate option ids", choice.id));
        }
        if choice.options.is_empty() {
            if !matches!(choice.selected.as_str(), "" | "true" | "false") {
                return Err(format!(
                    "boolean choice {:?} has invalid value {:?}",
                    choice.id, choice.selected
                ));
            }
        } else if !choice.selected.is_empty()
            && !choice.options.iter().any(|(id, _)| id == &choice.selected)
        {
            return Err(format!(
                "choice {:?} selects unknown option {:?}",
                choice.id, choice.selected
            ));
        }
    }
    Ok(())
}

/// The response-side choice check shared by FileChooser and AppChooser: the
/// prompter must answer exactly the offered choices, in order, with values
/// from each choice's option set (`true`/`false` for booleans).
fn validate_choice_answers(answers: &[(String, String)], choices: &[Choice]) -> Result<(), String> {
    if answers.len() != choices.len() {
        return Err("prompter returned the wrong number of choices".into());
    }
    for ((id, selected), requested) in answers.iter().zip(choices) {
        if id != &requested.id {
            return Err(format!("prompter returned unexpected choice id {id:?}"));
        }
        let valid = if requested.options.is_empty() {
            matches!(selected.as_str(), "true" | "false")
        } else {
            requested.options.iter().any(|(value, _)| value == selected)
        };
        if !valid {
            return Err(format!(
                "prompter returned invalid value {selected:?} for choice {id:?}"
            ));
        }
    }
    Ok(())
}

/// One application offered by an AppChooser dialog.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppChoice {
    /// The desktop file id (`org.foo.Bar.desktop`); also the value the
    /// portal response reports as the choice.
    pub id: String,
    pub name: String,
    /// The themed icon name from the desktop entry. The dialog renders
    /// names only; the field rides the contract so a future icon-capable
    /// dialog needs no version bump.
    pub icon: Option<String>,
}

/// One complete AppChooser request sent from the D-Bus backend to the
/// prompter. `choices` carries the FileChooser-style embedded controls; an
/// empty-options choice is a boolean checkbox (the backend uses one for
/// "remember this choice").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChooseAppRequest {
    pub app_id: String,
    pub title: String,
    pub content_type: String,
    pub parent_window: Option<String>,
    pub apps: Vec<AppChoice>,
    pub choices: Vec<Choice>,
}

/// AppChooser dialog list cap: one screen of candidates, and far below the
/// process contract's byte limit.
const MAX_CHOOSE_APP_CANDIDATES: usize = 64;

impl ChooseAppRequest {
    /// Reject malformed values before any dialog is shown.
    pub fn validate(&self) -> Result<(), String> {
        validate_prompt_text("app chooser title", &self.title, false)?;
        validate_prompt_text("app chooser content type", &self.content_type, false)?;
        if let Some(parent) = self.parent_window.as_deref() {
            validate_prompt_text("app chooser parent window", parent, true)?;
        }
        if self.apps.is_empty() || self.apps.len() > MAX_CHOOSE_APP_CANDIDATES {
            return Err(format!(
                "app chooser needs 1..={MAX_CHOOSE_APP_CANDIDATES} candidates"
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for app in &self.apps {
            if app.id.is_empty() || app.id.len() > MAX_PROMPT_TEXT_BYTES || app.id.contains('\0') {
                return Err("app ids must be non-empty, bounded, and NUL-free".into());
            }
            if !ids.insert(app.id.as_str()) {
                return Err(format!("duplicate app id {:?}", app.id));
            }
            validate_prompt_text("app name", &app.name, false)?;
            if let Some(icon) = app.icon.as_deref() {
                validate_prompt_text("app icon", icon, true)?;
            }
        }
        validate_choices(&self.choices)
    }
}

/// The one response an AppChooser prompter process emits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChooseAppResponse {
    Selected {
        /// The desktop file id of the chosen application.
        app: String,
        choices: Vec<(String, String)>,
    },
    Cancelled,
}

impl ChooseAppResponse {
    /// Validate the child result against the exact request before exposing
    /// it as a portal response: the chosen app must have been offered and
    /// the choice answers must match the offered controls.
    pub fn validate_for(&self, request: &ChooseAppRequest) -> Result<(), String> {
        let Self::Selected { app, choices } = self else {
            return Ok(());
        };
        if !request.apps.iter().any(|offered| &offered.id == app) {
            return Err(format!(
                "prompter returned an app that was not offered: {app:?}"
            ));
        }
        validate_choice_answers(choices, &request.choices)
    }
}

/// One capture source offered by the ScreenCast source chooser: the whole
/// desktop, one connector-named output, or the interactive window pick.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceChoice {
    /// Opaque backend-owned identifier (`desktop`, `output:<connector>`,
    /// `window`); also the value the response reports as the choice.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// One complete ScreenCast source-chooser request sent from the D-Bus
/// backend to the prompter. The dialog renders a single-selection list of
/// the offered sources; `remember_offered` controls whether the
/// persistence checkbox is shown (the backend offers it only when the
/// client requested a nonzero `persist_mode`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChooseSourceRequest {
    pub app_id: String,
    pub title: String,
    pub options: Vec<SourceChoice>,
    pub remember_offered: bool,
    pub parent_window: Option<String>,
}

/// Source-chooser list cap: a desktop, a handful of outputs, and the
/// window pick — sixteen is far beyond any real layout and far below the
/// process contract's byte limit.
const MAX_CHOOSE_SOURCE_OPTIONS: usize = 16;

impl ChooseSourceRequest {
    /// Reject malformed values before any dialog is shown.
    pub fn validate(&self) -> Result<(), String> {
        validate_prompt_text("source chooser title", &self.title, false)?;
        if let Some(parent) = self.parent_window.as_deref() {
            validate_prompt_text("source chooser parent window", parent, true)?;
        }
        if self.options.is_empty() || self.options.len() > MAX_CHOOSE_SOURCE_OPTIONS {
            return Err(format!(
                "source chooser needs 1..={MAX_CHOOSE_SOURCE_OPTIONS} options"
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for option in &self.options {
            if option.id.is_empty()
                || option.id.len() > MAX_PROMPT_TEXT_BYTES
                || option.id.contains('\0')
            {
                return Err("source ids must be non-empty, bounded, and NUL-free".into());
            }
            if !ids.insert(option.id.as_str()) {
                return Err(format!("duplicate source id {:?}", option.id));
            }
            validate_prompt_text("source label", &option.label, false)?;
            if let Some(description) = option.description.as_deref() {
                validate_prompt_text("source description", description, true)?;
            }
        }
        Ok(())
    }
}

/// The one response a source-chooser prompter process emits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChooseSourceResponse {
    Selected {
        /// The id of the chosen source option.
        source: String,
        /// Whether the user ticked the persistence checkbox.
        remember: bool,
    },
    Cancelled,
}

impl ChooseSourceResponse {
    /// Validate the child result against the exact request before exposing
    /// it as a portal response: the chosen source must have been offered,
    /// and `remember` may only be set when the checkbox was offered.
    pub fn validate_for(&self, request: &ChooseSourceRequest) -> Result<(), String> {
        let Self::Selected { source, remember } = self else {
            return Ok(());
        };
        if !request.options.iter().any(|offered| &offered.id == source) {
            return Err(format!(
                "prompter returned a source that was not offered: {source:?}"
            ));
        }
        if *remember && !request.remember_offered {
            return Err("prompter remembered a source without the checkbox".into());
        }
        Ok(())
    }
}

/// A launcher name is short user-facing text; 1 KiB is generous in every
/// script the dialog can receive.
const MAX_LAUNCHER_NAME_BYTES: usize = 1024;

/// One DynamicLauncher PrepareInstall request sent from the D-Bus backend
/// to the prompter: the user reviews the proposed launcher name (editing
/// it when `editable_name`), then confirms or cancels the installation.
///
/// The icon itself never crosses the pipe — its bytes are not a file the
/// dialog could open, so the backend echoes the icon variant back in the
/// portal results verbatim and only a short human-readable label (the
/// themed name, or a generic note) is shown. (The file chooser's preview
/// pane decodes *files* through its own pipeline; see ADR-0017.)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LauncherEditRequest {
    pub app_id: String,
    pub title: String,
    /// The proposed name; may be empty only when `editable_name` is set
    /// (the field then starts blank and the user must type one).
    pub name: String,
    pub editable_name: bool,
    /// The web app's URL, displayed when the launcher type is Webapp.
    pub target: Option<String>,
    /// A short description of the proposed icon, if any (see the docs).
    pub icon_label: Option<String>,
    pub modal: bool,
    pub parent_window: Option<String>,
}

impl LauncherEditRequest {
    /// Reject malformed values before any dialog is shown.
    pub fn validate(&self) -> Result<(), String> {
        validate_prompt_text("launcher editor title", &self.title, false)?;
        if self.name.len() > MAX_LAUNCHER_NAME_BYTES || self.name.contains('\0') {
            return Err("launcher name is oversized or contains NUL".into());
        }
        if !self.editable_name && self.name.trim().is_empty() {
            return Err("a non-editable launcher name must not be empty".into());
        }
        if let Some(target) = self.target.as_deref() {
            validate_prompt_text("launcher target", target, false)?;
        }
        if let Some(icon_label) = self.icon_label.as_deref() {
            validate_prompt_text("launcher icon label", icon_label, false)?;
        }
        if let Some(parent) = self.parent_window.as_deref() {
            validate_prompt_text("launcher parent window", parent, true)?;
        }
        Ok(())
    }
}

/// The one response a launcher-editor prompter process emits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LauncherEditResponse {
    Saved { name: String },
    Cancelled,
}

impl LauncherEditResponse {
    /// Validate the child result against the exact request: the saved name
    /// must be non-empty and bounded, and a non-editable name must come
    /// back unchanged.
    pub fn validate_for(&self, request: &LauncherEditRequest) -> Result<(), String> {
        let Self::Saved { name } = self else {
            return Ok(());
        };
        if name.trim().is_empty() || name.len() > MAX_LAUNCHER_NAME_BYTES || name.contains('\0') {
            return Err("prompter returned an empty, oversized, or NUL name".into());
        }
        if !request.editable_name && name != &request.name {
            return Err("prompter changed a non-editable launcher name".into());
        }
        Ok(())
    }
}

/// One filesystem path encoded as its native Unix bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BytePath(pub Vec<u8>);

impl BytePath {
    #[must_use]
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self(path.as_ref().as_os_str().as_bytes().to_vec())
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0.clone()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<PathBuf> for BytePath {
    fn from(path: PathBuf) -> Self {
        Self::from_path(path)
    }
}

/// The FileChooser operation represented by one prompter process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChooserMode {
    OpenFile,
    OpenDirectory,
    SaveFile,
    SaveFiles,
}

/// The two rule kinds in the portal's `(sa(us))` filter structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterRuleKind {
    Glob,
    Mime,
}

/// One typed file-filter rule. The rule kind is never inferred from text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilterRule {
    pub kind: FilterRuleKind,
    pub value: String,
}

/// One user-visible file filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileFilter {
    pub label: String,
    pub rules: Vec<FilterRule>,
}

/// One optional control embedded in a FileChooser request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// Empty means a boolean check button whose values are `true`/`false`.
    pub options: Vec<(String, String)>,
    pub selected: String,
}

/// One complete request sent from the D-Bus backend to the prompter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileChooserRequest {
    pub mode: FileChooserMode,
    pub app_id: String,
    pub title: String,
    pub accept_label: Option<String>,
    pub modal: bool,
    pub parent_window: Option<String>,
    pub multiple: bool,
    pub current_folder: Option<BytePath>,
    pub current_name: Option<String>,
    pub current_file: Option<BytePath>,
    pub filters: Vec<FileFilter>,
    pub current_filter: Option<FileFilter>,
    pub choices: Vec<Choice>,
    /// Suggested basenames for `SaveFiles`, in request order.
    pub files: Vec<BytePath>,
}

impl FileChooserRequest {
    /// Reject malformed values before any dialog or filesystem access.
    pub fn validate(&self) -> Result<(), String> {
        for (name, path) in [
            ("current_folder", self.current_folder.as_ref()),
            ("current_file", self.current_file.as_ref()),
        ] {
            if let Some(path) = path {
                validate_absolute_path(name, &path.to_path_buf())?;
            }
        }
        if self.mode != FileChooserMode::SaveFiles && !self.files.is_empty() {
            return Err("suggested files are valid only for SaveFiles".into());
        }
        if self.mode == FileChooserMode::SaveFiles && self.files.is_empty() {
            return Err("SaveFiles requires at least one suggested basename".into());
        }
        for name in &self.files {
            validate_basename(&name.to_path_buf())?;
        }
        for filter in self.filters.iter().chain(self.current_filter.as_ref()) {
            if filter.label.is_empty() || filter.rules.iter().any(|rule| rule.value.is_empty()) {
                return Err("filter labels and rules must not be empty".into());
            }
        }
        validate_choices(&self.choices)
    }

    /// Apply `SaveFiles` basename and collision semantics to the selected
    /// folder. Other modes return the selected paths unchanged.
    pub fn finish_paths(&self, selected: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
        if self.mode != FileChooserMode::SaveFiles {
            return Ok(selected);
        }
        let folder = selected
            .into_iter()
            .next()
            .ok_or_else(|| "SaveFiles returned no selected folder".to_owned())?;
        let mut reserved = std::collections::HashSet::new();
        let mut paths = Vec::with_capacity(self.files.len());
        for name in &self.files {
            let path = unique_child(&folder, &name.to_path_buf(), &reserved)?;
            reserved.insert(path.clone());
            paths.push(path);
        }
        Ok(paths)
    }
}

/// The one response emitted by a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileChooserResponse {
    Selected {
        paths: Vec<BytePath>,
        current_filter: Option<FileFilter>,
        choices: Vec<(String, String)>,
    },
    Cancelled,
    Failed {
        message: String,
    },
}

impl FileChooserResponse {
    /// Validate the child result against the exact request before exposing it
    /// as a portal response. The prompter is a fault boundary, not a trusted
    /// source of arbitrarily shaped paths or choice values.
    pub fn validate_for(&self, request: &FileChooserRequest) -> Result<(), String> {
        let Self::Selected {
            paths,
            current_filter,
            choices,
        } = self
        else {
            return Ok(());
        };

        let expected_paths = match request.mode {
            FileChooserMode::SaveFile => Some(1),
            FileChooserMode::SaveFiles => Some(request.files.len()),
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory if !request.multiple => {
                Some(1)
            }
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory => None,
        };
        if paths.is_empty() || expected_paths.is_some_and(|expected| paths.len() != expected) {
            return Err(format!(
                "prompter returned {} path(s), incompatible with {:?}",
                paths.len(),
                request.mode
            ));
        }
        for path in paths {
            let path = path.to_path_buf();
            if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
                return Err(format!("prompter returned an invalid local path {path:?}"));
            }
        }

        if let Some(filter) = current_filter {
            let offered = request.filters.iter().any(|candidate| candidate == filter)
                || (request.filters.is_empty() && request.current_filter.as_ref() == Some(filter));
            if !offered {
                return Err("prompter returned a filter that was not offered".into());
            }
        }

        validate_choice_answers(choices, &request.choices)
    }
}

fn validate_basename(path: &Path) -> Result<(), String> {
    if path.as_os_str().as_bytes().contains(&0) {
        return Err("SaveFiles basenames must not contain NUL".into());
    }
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if !name.is_empty() => Ok(()),
        _ => Err(format!(
            "SaveFiles name {path:?} is not a single non-empty basename"
        )),
    }
}

fn validate_absolute_path(name: &str, path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return Err(format!("{name} is not a valid absolute Unix path"));
    }
    Ok(())
}

fn unique_child(
    folder: &Path,
    name: &Path,
    reserved: &std::collections::HashSet<PathBuf>,
) -> Result<PathBuf, String> {
    validate_basename(name)?;
    let candidate = folder.join(name);
    if !reserved.contains(&candidate) && !path_occupied(&candidate) {
        return Ok(candidate);
    }

    let raw = name.as_os_str().as_bytes();
    let (stem, extension) = split_extension(raw);
    for suffix in 1..=u32::MAX {
        let mut bytes = stem.to_vec();
        bytes.extend_from_slice(format!("({suffix})").as_bytes());
        if let Some(extension) = extension {
            bytes.push(b'.');
            bytes.extend_from_slice(extension);
        }
        let candidate = folder.join(OsStr::from_bytes(&bytes));
        if !reserved.contains(&candidate) && !path_occupied(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not construct a unique filename for {name:?}"
    ))
}

fn path_occupied(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        // Permission and I/O failures must not be interpreted as a free name.
        Err(_) => true,
    }
}

fn split_extension(name: &[u8]) -> (&[u8], Option<&[u8]>) {
    let Some(dot) = name.iter().rposition(|byte| *byte == b'.') else {
        return (name, None);
    };
    if dot == 0 || dot + 1 == name.len() {
        return (name, None);
    }
    (&name[..dot], Some(&name[dot + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: FileChooserMode) -> FileChooserRequest {
        FileChooserRequest {
            mode,
            app_id: "dev.tessera.Test".into(),
            title: "Choose".into(),
            accept_label: None,
            modal: true,
            parent_window: None,
            multiple: false,
            current_folder: None,
            current_name: None,
            current_file: None,
            filters: Vec::new(),
            current_filter: None,
            choices: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn byte_paths_round_trip_non_utf8() {
        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        let json = serde_json::to_string(&BytePath::from(path.clone())).unwrap();
        let decoded: BytePath = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.to_path_buf(), path);
    }

    #[test]
    fn process_contract_rejects_version_mismatches() {
        let envelope = PrompterRequest {
            version: PROCESS_CONTRACT_VERSION + 1,
            prompt: PromptRequest::FileChooser(request(FileChooserMode::OpenFile)),
            appearance: None,
        };
        assert!(envelope.into_file_chooser().is_err());
        let envelope = PrompterResponse {
            version: PROCESS_CONTRACT_VERSION + 1,
            result: PromptResult::FileChooser(FileChooserResponse::Cancelled),
        };
        assert!(envelope.into_file_chooser().is_err());
    }

    #[test]
    fn typed_prompt_contract_round_trips_without_exposing_secrets() {
        let confirm = PrompterRequest::confirm(ConfirmRequest {
            title: "Share".into(),
            body: "Share account information?".into(),
            accept_label: Some("_Share".into()),
            deny_label: Some("_Refuse".into()),
            modal: true,
            parent_window: Some("wayland:parent".into()),
        });
        let value = serde_json::to_value(&confirm).unwrap();
        assert_eq!(value["version"], PROCESS_CONTRACT_VERSION);
        assert_eq!(value["prompt"]["kind"], "confirm");
        assert!(matches!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .into_prompt()
                .unwrap(),
            PromptRequest::Confirm(_)
        ));

        let secret = PrompterResponse::secret(SecretResponse::Secret {
            value: "do-not-log-this".into(),
        });
        let debug = format!("{secret:?}");
        assert!(!debug.contains("do-not-log-this"), "{debug}");
        let encoded = serde_json::to_value(secret).unwrap();
        assert_eq!(encoded["result"]["kind"], "secret");
        assert_eq!(encoded["result"]["response"]["status"], "secret");
    }

    #[test]
    fn appearance_snapshot_round_trips_and_validates() {
        let appearance = PromptAppearance {
            color_scheme: PromptColorScheme::Light,
            accent_color: Some(PromptAccent {
                red: 43,
                green: 101,
                blue: 232,
            }),
            high_contrast: true,
            reduced_motion: false,
        };
        let confirm = PrompterRequest::confirm(ConfirmRequest {
            title: "Share".into(),
            body: "Share account information?".into(),
            accept_label: None,
            deny_label: None,
            modal: true,
            parent_window: None,
        })
        .with_appearance(appearance);
        let value = serde_json::to_value(&confirm).unwrap();
        assert_eq!(value["appearance"]["color_scheme"], "light");
        assert_eq!(value["appearance"]["accent_color"]["red"], 43);
        assert_eq!(value["appearance"]["high_contrast"], true);
        let decoded: PrompterRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.appearance, Some(appearance));
        assert!(decoded.validate().is_ok());

        // Absent appearance decodes as None (backend without a snapshot).
        let bare = PrompterRequest::confirm(ConfirmRequest {
            title: "T".into(),
            body: "B".into(),
            accept_label: None,
            deny_label: None,
            modal: false,
            parent_window: None,
        });
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("appearance").is_none() || value["appearance"].is_null());
        assert_eq!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .appearance,
            None
        );

        // A black-transparent accent is rejected before any dialog shows.
        let invalid = PromptAppearance {
            accent_color: Some(PromptAccent {
                red: 0,
                green: 0,
                blue: 0,
            }),
            ..Default::default()
        };
        assert!(invalid.validate().is_err());

        // Unknown scheme values fail closed (deny_unknown_fields is not
        // applicable to enums, but serde rejects unknown variants).
        let bad = serde_json::json!({"color_scheme": "sepia"});
        assert!(serde_json::from_value::<PromptColorScheme>(bad).is_err());
    }

    #[test]
    fn confirmation_and_secret_text_are_bounded() {
        let empty = PrompterRequest::confirm(ConfirmRequest {
            title: String::new(),
            body: "Body".into(),
            accept_label: None,
            deny_label: None,
            modal: true,
            parent_window: None,
        });
        assert!(empty.into_prompt().is_err());

        let oversized = PrompterRequest::secret(SecretRequest {
            title: "Unlock".into(),
            reason: Some("x".repeat(MAX_PROMPT_TEXT_BYTES + 1)),
        });
        assert!(oversized.into_prompt().is_err());
    }

    #[test]
    fn save_files_rejects_paths_and_parent_components() {
        for name in ["", ".", "..", "a/b", "/absolute"] {
            let mut req = request(FileChooserMode::SaveFiles);
            req.files.push(BytePath::from_path(name));
            assert!(req.validate().is_err(), "{name:?} must be rejected");
        }
    }

    #[test]
    fn request_rejects_non_absolute_locations_and_empty_filter_rules() {
        let mut req = request(FileChooserMode::OpenFile);
        req.current_folder = Some(BytePath::from_path("relative"));
        assert!(req.validate().is_err());

        req.current_folder = Some(BytePath::from_path("/tmp"));
        req.filters.push(FileFilter {
            label: "Files".into(),
            rules: vec![FilterRule {
                kind: FilterRuleKind::Glob,
                value: String::new(),
            }],
        });
        assert!(req.validate().is_err());
    }

    #[test]
    fn save_files_preserves_order_and_avoids_existing_names() {
        let folder = std::env::temp_dir().join(format!(
            "tessera-prompter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("report.txt"), b"old").unwrap();
        std::fs::write(folder.join("report(1).txt"), b"old").unwrap();

        let mut req = request(FileChooserMode::SaveFiles);
        req.files = vec![
            BytePath::from_path("report.txt"),
            BytePath::from_path("image.png"),
        ];
        let paths = req.finish_paths(vec![folder.clone()]).unwrap();
        assert_eq!(paths[0], folder.join("report(2).txt"));
        assert_eq!(paths[1], folder.join("image.png"));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn duplicate_choice_ids_are_rejected() {
        let mut req = request(FileChooserMode::OpenFile);
        req.choices = vec![
            Choice {
                id: "encoding".into(),
                label: "Encoding".into(),
                options: vec![("utf8".into(), "UTF-8".into())],
                selected: "utf8".into(),
            },
            Choice {
                id: "encoding".into(),
                label: "Again".into(),
                options: Vec::new(),
                selected: "false".into(),
            },
        ];
        assert!(req.validate().is_err());
    }

    #[test]
    fn selected_response_is_checked_against_the_request() {
        let mut req = request(FileChooserMode::OpenFile);
        req.choices.push(Choice {
            id: "encoding".into(),
            label: "Encoding".into(),
            options: vec![("utf8".into(), "UTF-8".into())],
            selected: "utf8".into(),
        });
        let valid = FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/file.txt")],
            current_filter: None,
            choices: vec![("encoding".into(), "utf8".into())],
        };
        assert!(valid.validate_for(&req).is_ok());

        let invalid = FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("relative")],
            current_filter: None,
            choices: vec![("encoding".into(), "unknown".into())],
        };
        assert!(invalid.validate_for(&req).is_err());
    }

    #[test]
    fn save_files_avoids_duplicate_suggestions_in_one_request() {
        let folder = std::env::temp_dir().join(format!(
            "tessera-prompter-duplicates-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let mut req = request(FileChooserMode::SaveFiles);
        req.files = vec![
            BytePath::from_path("same.txt"),
            BytePath::from_path("same.txt"),
        ];
        let paths = req.finish_paths(vec![folder.clone()]).unwrap();
        assert_eq!(paths, [folder.join("same.txt"), folder.join("same(1).txt")]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn choose_app_request() -> ChooseAppRequest {
        ChooseAppRequest {
            app_id: "dev.tessera.Test".into(),
            title: "Open with".into(),
            content_type: "text/plain".into(),
            parent_window: Some("wayland:parent".into()),
            apps: vec![
                AppChoice {
                    id: "org.foo.Editor.desktop".into(),
                    name: "Foo Editor".into(),
                    icon: Some("foo-editor".into()),
                },
                AppChoice {
                    id: "org.bar.Notes.desktop".into(),
                    name: "Bar Notes".into(),
                    icon: None,
                },
            ],
            choices: vec![Choice {
                id: "remember".into(),
                label: "Remember this choice".into(),
                options: Vec::new(),
                selected: "false".into(),
            }],
        }
    }

    #[test]
    fn choose_app_contract_round_trips() {
        let request = PrompterRequest::choose_app(choose_app_request());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["version"], PROCESS_CONTRACT_VERSION);
        assert_eq!(value["prompt"]["kind"], "choose_app");
        assert!(matches!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .into_prompt()
                .unwrap(),
            PromptRequest::ChooseApp(_)
        ));

        let response = PrompterResponse::choose_app(ChooseAppResponse::Selected {
            app: "org.bar.Notes.desktop".into(),
            choices: vec![("remember".into(), "true".into())],
        });
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["result"]["kind"], "choose_app");
        assert_eq!(encoded["result"]["response"]["status"], "selected");
        let PromptResult::ChooseApp(decoded) = serde_json::from_value::<PrompterResponse>(encoded)
            .unwrap()
            .into_result()
            .unwrap()
        else {
            panic!("expected a choose_app result");
        };
        assert!(decoded.validate_for(&choose_app_request()).is_ok());
    }

    #[test]
    fn choose_app_request_validation_is_bounded() {
        let mut request = choose_app_request();

        let mut no_apps = request.clone();
        no_apps.apps.clear();
        assert!(no_apps.validate().is_err());

        let mut duplicate = request.clone();
        duplicate.apps.push(duplicate.apps[0].clone());
        assert!(duplicate.validate().is_err());

        let mut unnamed = request.clone();
        unnamed.apps[0].name.clear();
        assert!(unnamed.validate().is_err());

        request.choices[0].selected = "maybe".into();
        assert!(request.validate().is_err());
    }

    #[test]
    fn choose_app_response_is_checked_against_the_request() {
        let request = choose_app_request();
        let unknown_app = ChooseAppResponse::Selected {
            app: "org.evil.NotOffered.desktop".into(),
            choices: vec![("remember".into(), "false".into())],
        };
        assert!(unknown_app.validate_for(&request).is_err());

        let wrong_choice = ChooseAppResponse::Selected {
            app: "org.foo.Editor.desktop".into(),
            choices: vec![("remember".into(), "yes".into())],
        };
        assert!(wrong_choice.validate_for(&request).is_err());

        assert!(ChooseAppResponse::Cancelled.validate_for(&request).is_ok());
    }

    fn choose_source_request() -> ChooseSourceRequest {
        ChooseSourceRequest {
            app_id: "dev.tessera.Test".into(),
            title: "Share Your Screen".into(),
            options: vec![
                SourceChoice {
                    id: "desktop".into(),
                    label: "Entire desktop".into(),
                    description: None,
                },
                SourceChoice {
                    id: "output:HDMI-A-1".into(),
                    label: "HDMI-A-1".into(),
                    description: Some("1920×1080".into()),
                },
                SourceChoice {
                    id: "window".into(),
                    label: "Window…".into(),
                    description: None,
                },
            ],
            remember_offered: true,
            parent_window: Some("wayland:parent".into()),
        }
    }

    #[test]
    fn choose_source_contract_round_trips() {
        let request = PrompterRequest::choose_source(choose_source_request());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["version"], PROCESS_CONTRACT_VERSION);
        assert_eq!(value["prompt"]["kind"], "choose_source");
        assert!(matches!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .into_prompt()
                .unwrap(),
            PromptRequest::ChooseSource(_)
        ));

        let response = PrompterResponse::choose_source(ChooseSourceResponse::Selected {
            source: "output:HDMI-A-1".into(),
            remember: true,
        });
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["result"]["kind"], "choose_source");
        assert_eq!(encoded["result"]["response"]["status"], "selected");
        let PromptResult::ChooseSource(decoded) =
            serde_json::from_value::<PrompterResponse>(encoded)
                .unwrap()
                .into_result()
                .unwrap()
        else {
            panic!("expected a choose_source result");
        };
        assert!(decoded.validate_for(&choose_source_request()).is_ok());
    }

    #[test]
    fn choose_source_request_validation_is_bounded() {
        let request = choose_source_request();

        let mut no_options = request.clone();
        no_options.options.clear();
        assert!(no_options.validate().is_err());

        let mut too_many = request.clone();
        while too_many.options.len() <= 16 {
            let index = too_many.options.len();
            too_many.options.push(SourceChoice {
                id: format!("output:DP-{index}"),
                label: format!("DP-{index}"),
                description: None,
            });
        }
        assert!(too_many.validate().is_err());

        let mut duplicate = request.clone();
        duplicate.options.push(duplicate.options[0].clone());
        assert!(duplicate.validate().is_err());

        let mut unnamed = request.clone();
        unnamed.options[0].label.clear();
        assert!(unnamed.validate().is_err());

        let mut empty_id = request.clone();
        empty_id.options[0].id.clear();
        assert!(empty_id.validate().is_err());

        let mut nul_id = request;
        nul_id.options[0].id.push('\0');
        assert!(nul_id.validate().is_err());
    }

    #[test]
    fn choose_source_response_is_checked_against_the_request() {
        let request = choose_source_request();
        let unknown_source = ChooseSourceResponse::Selected {
            source: "output:SNEAKY-1".into(),
            remember: false,
        };
        assert!(unknown_source.validate_for(&request).is_err());

        // Remembering is only valid while the checkbox was offered.
        let remembered = ChooseSourceResponse::Selected {
            source: "desktop".into(),
            remember: true,
        };
        assert!(remembered.validate_for(&request).is_ok());
        let without_checkbox = ChooseSourceRequest {
            remember_offered: false,
            ..choose_source_request()
        };
        assert!(remembered.validate_for(&without_checkbox).is_err());

        assert!(
            ChooseSourceResponse::Cancelled
                .validate_for(&request)
                .is_ok()
        );
    }

    #[test]
    fn choose_source_rejects_unknown_fields() {
        let mut value = serde_json::to_value(choose_source_request()).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ChooseSourceRequest>(value).is_err());
    }

    fn launcher_edit_request() -> LauncherEditRequest {
        LauncherEditRequest {
            app_id: "dev.tessera.Test".into(),
            title: "Install Launcher".into(),
            name: "Cool App".into(),
            editable_name: true,
            target: None,
            icon_label: Some("cool-app".into()),
            modal: true,
            parent_window: Some("wayland:parent".into()),
        }
    }

    #[test]
    fn launcher_edit_contract_round_trips() {
        let request = PrompterRequest::launcher_edit(launcher_edit_request());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["version"], PROCESS_CONTRACT_VERSION);
        assert_eq!(value["prompt"]["kind"], "launcher_edit");
        assert!(matches!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .into_prompt()
                .unwrap(),
            PromptRequest::LauncherEdit(_)
        ));

        let response = PrompterResponse::launcher_edit(LauncherEditResponse::Saved {
            name: "Renamed App".into(),
        });
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["result"]["kind"], "launcher_edit");
        assert_eq!(encoded["result"]["response"]["status"], "saved");
        let PromptResult::LauncherEdit(decoded) =
            serde_json::from_value::<PrompterResponse>(encoded)
                .unwrap()
                .into_result()
                .unwrap()
        else {
            panic!("expected a launcher_edit result");
        };
        assert!(decoded.validate_for(&launcher_edit_request()).is_ok());
    }

    #[test]
    fn launcher_edit_names_are_bounded_and_edit_rules_hold() {
        let mut request = launcher_edit_request();
        // An empty proposed name is valid only when the user can type one.
        request.name.clear();
        assert!(request.validate().is_ok());
        request.editable_name = false;
        assert!(request.validate().is_err());

        let mut request = launcher_edit_request();
        request.name = "x".repeat(1025);
        assert!(request.validate().is_err());

        let request = launcher_edit_request();
        let empty = LauncherEditResponse::Saved { name: " ".into() };
        assert!(empty.validate_for(&request).is_err());

        // A non-editable name must come back unchanged.
        let request = LauncherEditRequest {
            editable_name: false,
            ..launcher_edit_request()
        };
        let renamed = LauncherEditResponse::Saved {
            name: "Sneaky".into(),
        };
        assert!(renamed.validate_for(&request).is_err());
        let unchanged = LauncherEditResponse::Saved {
            name: "Cool App".into(),
        };
        assert!(unchanged.validate_for(&request).is_ok());
        assert!(
            LauncherEditResponse::Cancelled
                .validate_for(&request)
                .is_ok()
        );
    }
}
