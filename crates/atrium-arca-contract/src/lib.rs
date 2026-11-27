//! The `arca --chooser-prompt` FileChooser contract.
//!
//! ADR-0027: this crate owns the one inter-process prompt contract the portal
//! still speaks. Arca is the file manager and renders the FileChooser dialog;
//! the backend writes one JSON request to its standard input and reads one JSON
//! response from its standard output.
//!
//! Paths are byte arrays rather than UTF-8 strings because the FileChooser
//! portal's path arguments are `ay` on the D-Bus surface: every Unix filename
//! the portal accepts round-trips the contract without loss.
//!
//! The crate holds no process supervision, no executable resolution, and no
//! socket. Those live in the daemon (`src/companion/arca.rs`), which is the
//! only party that knows its install layout, its cancellation closure, and its
//! concurrency caps.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// Version of the arca FileChooser stdio contract.
///
/// The version names the shape of the bytes on the two pipes, and that shape
/// covers FileChooser only. ADR-0027 restarts it at 1: the legacy prompter
/// envelope's version 6 described seven prompt kinds, six of which have no
/// process left to speak them (ADR-0026). `scripts/version-consistency.sh`
/// checks that the repository's smoke-test payloads match this constant, and
/// `arca` carries the matching `ARCA_CONTRACT_VERSION` constant.
pub const ARCA_CONTRACT_VERSION: u32 = 1;

/// Cap on the one request line written to arca's standard input: the
/// complete `{"version":…,"request":…}` envelope *and* its trailing newline.
///
/// arca reads at most this many bytes and refuses anything longer, so the
/// backend must measure exactly the bytes it writes — measuring the bare
/// request would admit a request that arca then rejects. arca defines the
/// matching `MAX_CHOOSER_REQUEST_BYTES`.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

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

/// The FileChooser operation represented by one chooser process.
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

/// One complete request sent from the D-Bus backend to the chooser.
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
}

/// The one response emitted by a chooser process.
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
    /// as a portal response. The chooser is a fault boundary, not a trusted
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
                "chooser returned {} path(s), incompatible with {:?}",
                paths.len(),
                request.mode
            ));
        }
        for path in paths {
            let path = path.to_path_buf();
            if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
                return Err(format!("chooser returned an invalid local path {path:?}"));
            }
        }

        if let Some(filter) = current_filter {
            let offered = request.filters.iter().any(|candidate| candidate == filter)
                || (request.filters.is_empty() && request.current_filter.as_ref() == Some(filter));
            if !offered {
                return Err("chooser returned a filter that was not offered".into());
            }
        }

        validate_choice_answers(choices, &request.choices)
    }
}

/// The choice-list validation: unique non-empty ids and labels, non-empty
/// unique option pairs, and a selected value that names an offered option (or
/// `true`/`false` for a boolean check button).
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

/// Check the answered choice values against the offered controls, in order.
fn validate_choice_answers(answers: &[(String, String)], choices: &[Choice]) -> Result<(), String> {
    if answers.len() != choices.len() {
        return Err("chooser returned the wrong number of choices".into());
    }
    for ((id, selected), requested) in answers.iter().zip(choices) {
        if id != &requested.id {
            return Err(format!("chooser returned unexpected choice id {id:?}"));
        }
        let valid = if requested.options.is_empty() {
            matches!(selected.as_str(), "true" | "false")
        } else {
            requested.options.iter().any(|(value, _)| value == selected)
        };
        if !valid {
            return Err(format!(
                "chooser returned invalid value {selected:?} for choice {id:?}"
            ));
        }
    }
    Ok(())
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
    fn contract_version_is_pinned_to_the_arca_wire_shape() {
        // The version names the bytes on the pipes. Bumping it is a
        // coordinated change with the arca repository's own
        // `ARCA_CONTRACT_VERSION` constant.
        assert_eq!(ARCA_CONTRACT_VERSION, 1);
    }

    #[test]
    fn request_round_trips_the_wire_shape() {
        let mut req = request(FileChooserMode::OpenFile);
        req.filters.push(FileFilter {
            label: "Text".into(),
            rules: vec![FilterRule {
                kind: FilterRuleKind::Glob,
                value: "*.txt".into(),
            }],
        });
        req.choices.push(Choice {
            id: "encoding".into(),
            label: "Encoding".into(),
            options: vec![("utf8".into(), "UTF-8".into())],
            selected: "utf8".into(),
        });
        let json = serde_json::to_string(&req).unwrap();
        let decoded: FileChooserRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, req);
    }
}
