//! `org.freedesktop.impl.portal.FileChooser` v3.
//!
//! The backend owns D-Bus translation and request lifetime. Each request is
//! rendered by a fresh `atrium-portal-prompter` child, communicating over a
//! private JSON pipe contract. No file path or directory entry crosses Tessera
//! compositor IPC, and closing the portal request terminates the child.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{
    BytePath, Choice, FileChooserMode, FileChooserRequest, FileChooserResponse, FileFilter,
    FilterRule, FilterRuleKind, PromptResult, PrompterRequest,
};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};
use zbus::zvariant::{ObjectPath, Value};

use crate::{files, prompter};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_FILE_CHOOSERS: usize = 32;

/// One file-chooser request handed from the bus methods to the dispatcher.
/// The request contains the complete portal semantics; no UI policy is
/// reconstructed by the worker task.
pub(crate) enum FileChooserJob {
    Choose {
        request_path: String,
        request: FileChooserRequest,
        reply: ResponseSender,
    },
}

pub(crate) struct FileChooserIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<FileChooserJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserIface {
    async fn open_file(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = FileChooserRequest {
            mode: if parsed.directory {
                FileChooserMode::OpenDirectory
            } else {
                FileChooserMode::OpenFile
            },
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: parsed.multiple,
            current_folder: parsed.current_folder,
            current_name: None,
            current_file: None,
            filters: parsed.filters,
            current_filter: parsed.current_filter,
            choices: parsed.choices,
            files: Vec::new(),
        };
        self.choose(handle, request).await
    }

    async fn save_file(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = FileChooserRequest {
            mode: FileChooserMode::SaveFile,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: false,
            current_folder: parsed.current_folder,
            current_name: parsed.current_name,
            current_file: parsed.current_file,
            filters: parsed.filters,
            current_filter: parsed.current_filter,
            choices: parsed.choices,
            files: Vec::new(),
        };
        self.choose(handle, request).await
    }

    async fn save_files(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = FileChooserRequest {
            mode: FileChooserMode::SaveFiles,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: false,
            current_folder: parsed.current_folder,
            current_name: None,
            current_file: None,
            filters: Vec::new(),
            current_filter: None,
            choices: parsed.choices,
            files: parsed.save_files,
        };
        self.choose(handle, request).await
    }
}

impl FileChooserIface {
    async fn choose(
        &self,
        handle: ObjectPath<'_>,
        request: FileChooserRequest,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_owned();
        log::info!(
            "portal: FileChooser for '{}' ({:?}) at {path}",
            request.app_id,
            request.mode
        );

        if let Err(error) = validate_request(&request) {
            log::warn!("portal: refusing invalid FileChooser request: {error}");
            return Ok(failed());
        }

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "file chooser",
            &self.jobs,
            |reply| FileChooserJob::Choose {
                request_path: path.clone(),
                request,
                reply,
            },
        )
        .await
    }
}

/// Validate before enqueueing so a hostile request cannot multiply a large
/// D-Bus payload across the bounded worker queue.
fn validate_request(request: &FileChooserRequest) -> Result<(), String> {
    request.validate()?;
    let encoded = serde_json::to_vec(request)
        .map_err(|error| format!("could not encode FileChooser request: {error}"))?;
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "FileChooser request exceeds the {MAX_REQUEST_BYTES}-byte limit"
        ));
    }
    Ok(())
}

/// Dispatch each request to its own supervised task. A modal chooser belongs
/// to one calling application and must not block unrelated portal clients or
/// delay cancellation of a queued request.
pub(crate) fn file_chooser_worker(
    rx: mpsc::Receiver<FileChooserJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
) {
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(FileChooserJob::Choose {
        request_path,
        request,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_FILE_CHOOSERS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing FileChooser request: concurrency limit reached");
            let _ = reply.send_blocking(failed());
            continue;
        }
        let tracker = Arc::clone(&tracker);
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("tessera-file-chooser".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_pick(&tracker, &request_path, request, Some(&task_settings));
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn FileChooser task: {error}");
            let _ = spawn_failure_reply.send_blocking(failed());
        }
    }
}

fn run_pick(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    request: FileChooserRequest,
    settings: Option<&crate::settings::SettingsStore>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "file chooser tracker").was_closed(request_path) {
        return cancelled();
    }
    if let Err(error) = request.validate() {
        log::warn!("portal: invalid FileChooser request: {error}");
        return failed();
    }

    let app_id = request.app_id.clone();
    match invoke_prompter(tracker, request_path, &request, settings) {
        Ok(response @ FileChooserResponse::Selected { .. }) => {
            // Request.Close wins a race with a completed child response.
            if sync::lock(tracker, "file chooser tracker").was_closed(request_path) {
                return cancelled();
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid FileChooser response for '{app_id}': {error}");
                return failed();
            }
            let FileChooserResponse::Selected {
                paths,
                current_filter,
                choices,
            } = response
            else {
                unreachable!()
            };
            let (count, results) = build_results(paths, current_filter, choices);
            log::info!("portal: FileChooser for '{app_id}' -> {count} uri(s)");
            (0, results)
        }
        Ok(FileChooserResponse::Cancelled) => cancelled(),
        Ok(FileChooserResponse::Failed { message }) | Err(message) => {
            log::warn!("portal: FileChooser for '{app_id}' failed: {message}");
            failed()
        }
    }
}

fn invoke_prompter(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    request: &FileChooserRequest,
    settings: Option<&crate::settings::SettingsStore>,
) -> Result<FileChooserResponse, String> {
    let cancelled = || sync::lock(tracker, "file chooser tracker").was_closed(request_path);
    match prompter::invoke(
        PrompterRequest::file_chooser(request.clone()),
        settings,
        Some(&cancelled),
    ) {
        Ok(PromptResult::FileChooser(response)) => Ok(response),
        Ok(_) => Err("prompter returned the wrong response kind".into()),
        Err(prompter::InvokeError::Cancelled) => Ok(FileChooserResponse::Cancelled),
        Err(prompter::InvokeError::Failed(message)) => Err(message),
    }
}

fn build_results(
    paths: Vec<BytePath>,
    current_filter: Option<FileFilter>,
    choices: Vec<(String, String)>,
) -> (usize, HashMap<String, Value<'static>>) {
    let uris: Vec<String> = paths
        .into_iter()
        .filter(|path| !path.is_empty())
        .map(|path| files::file_uri(&path.to_path_buf()))
        .collect();
    let count = uris.len();
    let mut results = HashMap::from([("uris".to_owned(), Value::from(uris))]);
    if let Some(filter) = current_filter {
        results.insert(
            "current_filter".to_owned(),
            Value::Structure(zbus::zvariant::Structure::from(filter_to_wire(filter))),
        );
    }
    if !choices.is_empty() {
        results.insert("choices".to_owned(), Value::from(choices));
    }
    (count, results)
}

fn cancelled() -> (u32, HashMap<String, Value<'static>>) {
    (1, HashMap::new())
}

fn failed() -> (u32, HashMap<String, Value<'static>>) {
    (2, HashMap::new())
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

struct ParsedOptions {
    modal: bool,
    multiple: bool,
    directory: bool,
    accept_label: Option<String>,
    current_folder: Option<BytePath>,
    current_name: Option<String>,
    current_file: Option<BytePath>,
    filters: Vec<FileFilter>,
    current_filter: Option<FileFilter>,
    choices: Vec<Choice>,
    save_files: Vec<BytePath>,
}

fn parse_options(options: &HashMap<String, Value<'_>>) -> ParsedOptions {
    let get_bool = |key: &str, default| {
        options
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(default)
    };
    let get_string = |key: &str| {
        options
            .get(key)
            .and_then(|value| String::try_from(value).ok())
    };
    let get_path = |key: &str| {
        options
            .get(key)
            .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
            .and_then(byte_path)
    };
    let filters = options
        .get("filters")
        .and_then(|value| {
            Vec::<(String, Vec<(u32, String)>)>::try_from(value.try_clone().ok()?).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(filter_from_wire)
        .collect();
    let current_filter = options
        .get("current_filter")
        .and_then(filter_from_value)
        .map(filter_from_wire);
    let choices = options
        .get("choices")
        .and_then(|value| {
            Vec::<(String, String, Vec<(String, String)>, String)>::try_from(
                value.try_clone().ok()?,
            )
            .ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(id, label, options, selected)| Choice {
            id,
            label,
            options,
            selected,
        })
        .collect();
    let save_files = options
        .get("files")
        .and_then(|value| Vec::<Vec<u8>>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(byte_path)
        .collect();

    ParsedOptions {
        modal: get_bool("modal", true),
        multiple: get_bool("multiple", false),
        directory: get_bool("directory", false),
        accept_label: get_string("accept_label").filter(|label| !label.is_empty()),
        current_folder: get_path("current_folder").filter(|path| path.to_path_buf().is_absolute()),
        current_name: get_string("current_name"),
        current_file: get_path("current_file").filter(|path| path.to_path_buf().is_absolute()),
        filters,
        current_filter,
        choices,
        save_files,
    }
}

fn byte_path(mut bytes: Vec<u8>) -> Option<BytePath> {
    if bytes.last() != Some(&0) {
        return None;
    }
    bytes.pop();
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    Some(BytePath(bytes))
}

fn filter_from_wire((label, rules): (String, Vec<(u32, String)>)) -> FileFilter {
    FileFilter {
        label,
        rules: rules
            .into_iter()
            .filter_map(|(kind, value)| {
                let kind = match kind {
                    0 => FilterRuleKind::Glob,
                    1 => FilterRuleKind::Mime,
                    _ => return None,
                };
                Some(FilterRule { kind, value })
            })
            .collect(),
    }
}

fn filter_from_value(value: &Value<'_>) -> Option<(String, Vec<(u32, String)>)> {
    let Value::Structure(structure) = value else {
        return None;
    };
    let [label, rules] = structure.fields() else {
        return None;
    };
    Some((
        String::try_from(label).ok()?,
        Vec::<(u32, String)>::try_from(rules.try_clone().ok()?).ok()?,
    ))
}

fn filter_to_wire(filter: FileFilter) -> (String, Vec<(u32, String)>) {
    let rules = filter
        .rules
        .into_iter()
        .map(|rule| {
            let kind = match rule.kind {
                FilterRuleKind::Glob => 0,
                FilterRuleKind::Mime => 1,
            };
            (kind, rule.value)
        })
        .collect();
    (filter.label, rules)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn defaults_match_the_portal_contract() {
        let parsed = parse_options(&HashMap::new());
        assert!(parsed.modal);
        assert!(!parsed.multiple && !parsed.directory);
        assert!(parsed.filters.is_empty() && parsed.save_files.is_empty());
        assert!(parsed.current_folder.is_none() && parsed.current_file.is_none());
    }

    #[test]
    fn filters_keep_their_explicit_rule_types() {
        let rules = vec![
            (0u32, "image/*".to_owned()),
            (1u32, "not-a-slashless-mime".to_owned()),
        ];
        let parsed = parse_options(&options(&[(
            "filters",
            Value::from(vec![("Images".to_owned(), rules)]),
        )]));
        assert_eq!(parsed.filters.len(), 1);
        assert_eq!(parsed.filters[0].rules[0].kind, FilterRuleKind::Glob);
        assert_eq!(parsed.filters[0].rules[1].kind, FilterRuleKind::Mime);
    }

    #[test]
    fn path_options_require_one_trailing_nul_and_reject_interior_nuls() {
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp/docs\0".to_vec()),
        )]));
        assert_eq!(
            parsed.current_folder.unwrap().to_path_buf(),
            PathBuf::from("/tmp/docs")
        );
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp\0/docs\0".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp/docs".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"relative\0".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
    }

    #[test]
    fn results_preserve_filter_types_and_real_choices() {
        let filter = FileFilter {
            label: "Images".into(),
            rules: vec![FilterRule {
                kind: FilterRuleKind::Glob,
                value: "image/*".into(),
            }],
        };
        let (_, results) = build_results(
            vec![BytePath::from_path("/tmp/a file.png")],
            Some(filter),
            vec![("encoding".into(), "utf8".into())],
        );
        let Value::Array(uris) = &results["uris"] else {
            panic!("uris must be an array");
        };
        let uri = String::try_from(uris.iter().next().unwrap()).unwrap();
        assert_eq!(uri, "file:///tmp/a%20file.png");
        assert!(results.contains_key("current_filter"));
        let Value::Array(choices) = &results["choices"] else {
            panic!("choices must be an array");
        };
        assert_eq!(choices.len(), 1);
    }

    #[test]
    fn oversized_request_is_rejected_before_enqueue() {
        let request = FileChooserRequest {
            mode: FileChooserMode::OpenFile,
            app_id: "dev.tessera.Test".into(),
            title: "x".repeat(MAX_REQUEST_BYTES + 1),
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
        };
        assert!(validate_request(&request).is_err());
    }
}
