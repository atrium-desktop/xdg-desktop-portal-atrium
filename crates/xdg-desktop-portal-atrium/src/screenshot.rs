//! `org.freedesktop.impl.portal.Screenshot` v3.
//!
//! `Screenshot` exports an `org.freedesktop.impl.portal.Request` object at the
//! exact `handle` supplied by the portal frontend, then awaits a dedicated
//! capture worker without blocking zbus's executor. The worker pulls the
//! focused output's PNG over scoped IPC and returns the backend contract's
//! `(response, results)` tuple. With `interactive = true` it first runs the
//! compositor's region picker (`PickTarget`).
//!
//! Version 2 adds `PickColor`: the compositor's crosshair picker returns the
//! clicked point's RGB, which the response reports as the spec's `color`
//! `(ddd)` triple (0–1 doubles).
//!
//! Version 3 advertises the target modes the compositor can isolate safely.
//! Tessera currently advertises Area only: output capture is retained as the
//! legacy no-`target` behavior, while Window and Active Window are withheld
//! until compositor IPC can render a toplevel independently of occluding
//! windows. Advertising fewer truthful targets is preferable to leaking
//! pixels from an unrelated window.
//!
//! Response codes follow the portal specification: 0 success, 1 cancelled
//! (the client called `Request.Close` first, or the user dismissed the
//! picker), 2 other error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use zbus::zvariant::{ObjectPath, Value};

use crate::files;
use crate::ipc::PortalCapture;
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// The served interface version: 3 adds target selection.
pub(crate) const SCREENSHOT_VERSION: u32 = 3;
/// `AvailableTargets` bit: an interactively selected rectangular area.
pub(crate) const SCREENSHOT_TARGET_AREA: u32 = 4;

/// One screenshot/color request handed from the bus methods to the capture
/// worker.
pub(crate) enum CaptureJob {
    Screenshot {
        request_path: String,
        token: String,
        app_id: String,
        interactive: bool,
        target: Option<ScreenshotTarget>,
        permission_store_checked: bool,
        reply: ResponseSender,
    },
    PickColor {
        request_path: String,
        app_id: String,
        reply: ResponseSender,
    },
}

/// Options parsed out of the `a{sv}` argument.
pub(crate) struct ScreenshotOptions {
    pub(crate) interactive: bool,
    pub(crate) target: Option<ScreenshotTarget>,
    pub(crate) permission_store_checked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScreenshotTarget {
    Area,
}

/// Parse the backend `Screenshot` options dict. Request-token handling belongs
/// to the portal frontend; backend options contain only request policy.
pub(crate) fn parse_options(
    options: &HashMap<String, Value<'_>>,
) -> Result<ScreenshotOptions, String> {
    let interactive = options
        .get("interactive")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false);
    let target = match options
        .get("target")
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "Screenshot target must be an unsigned integer".to_string())?
    {
        None => None,
        Some(SCREENSHOT_TARGET_AREA) => Some(ScreenshotTarget::Area),
        Some(target) => {
            return Err(format!(
                "Screenshot target {target} is not advertised by this backend"
            ));
        }
    };
    let permission_store_checked = options
        .get("permission_store_checked")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false);
    Ok(ScreenshotOptions {
        interactive,
        target,
        permission_store_checked,
    })
}

/// Object-path elements allow `[A-Za-z0-9_]`; the filename and the bus path
/// share one sanitized token so neither can be escaped through the other.
pub(crate) fn sanitize_token(token: &str) -> String {
    token
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn capture_token(handle: &ObjectPath<'_>) -> String {
    handle
        .as_str()
        .rsplit('/')
        .next()
        .filter(|tail| !tail.is_empty())
        .map(sanitize_token)
        .unwrap_or_else(|| "capture".to_string())
}

/// The served screenshot interface. Methods only register the request
/// object and enqueue; all blocking work happens on the capture worker.
pub(crate) struct ScreenshotIface {
    /// Async handle onto the same connection; only used inside served
    /// methods, which already run on zbus's executor (tray precedent).
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<CaptureJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotIface {
    async fn screenshot(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let options = match parse_options(&options) {
            Ok(options) => options,
            Err(error) => {
                log::warn!("portal: refusing Screenshot for '{app_id}': {error}");
                return Ok((2, HashMap::new()));
            }
        };
        let path = handle.as_str().to_string();
        let token = capture_token(&handle);
        log::info!(
            "portal: Screenshot for '{app_id}' (interactive={}, target={:?}) at {path}",
            options.interactive,
            options.target
        );

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "screenshot",
            &self.jobs,
            |reply| CaptureJob::Screenshot {
                request_path: path.clone(),
                token,
                app_id: app_id.to_string(),
                interactive: options.interactive,
                target: options.target,
                permission_store_checked: options.permission_store_checked,
                reply,
            },
        )
        .await
    }

    async fn pick_color(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: PickColor for '{app_id}' at {path}");

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "pick color",
            &self.jobs,
            |reply| CaptureJob::PickColor {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                reply,
            },
        )
        .await
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        SCREENSHOT_VERSION
    }

    #[zbus(property)]
    fn available_targets(&self) -> u32 {
        SCREENSHOT_TARGET_AREA
    }
}

/// Dispatch requests independently. Interactive compositor chrome can stay
/// open for minutes; it must not head-of-line block unrelated screenshots
/// or color picks. The cap bounds memory/thread consumption under a hostile
/// request flood.
pub(crate) fn capture_worker(
    rx: mpsc::Receiver<CaptureJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    socket: std::path::PathBuf,
) {
    const MAX_ACTIVE_CAPTURES: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(job) = rx.recv() {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_CAPTURES {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing capture request: concurrency limit reached");
            let reply = match &job {
                CaptureJob::Screenshot { reply, .. } | CaptureJob::PickColor { reply, .. } => reply,
            };
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_socket = socket.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = match &job {
            CaptureJob::Screenshot { reply, .. } | CaptureJob::PickColor { reply, .. } => {
                reply.clone()
            }
        };
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-capture-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let mut capture = PortalCapture::new(task_socket);
                let result = run_job(&mut capture, &task_tracker, &job);
                let reply = match &job {
                    CaptureJob::Screenshot { reply, .. } | CaptureJob::PickColor { reply, .. } => {
                        reply
                    }
                };
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn capture task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one job and produce the `(response_code, results)` pair.
fn run_job(
    capture: &mut PortalCapture,
    tracker: &Arc<Mutex<RequestTracker>>,
    job: &CaptureJob,
) -> (u32, HashMap<String, Value<'static>>) {
    match job {
        CaptureJob::Screenshot {
            request_path,
            token,
            app_id,
            interactive,
            target,
            permission_store_checked,
            ..
        } => {
            if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            // Interactive: the compositor's region picker decides what to
            // capture. A dismissed picker answers 1 (cancelled),
            // exactly like a client Close.
            let region = if *interactive || *target == Some(ScreenshotTarget::Area) {
                match capture.pick(atrium_portal_ipc::PickKind::Region) {
                    Ok(atrium_portal_ipc::PickResult::Region { rect }) => Some(rect),
                    Ok(atrium_portal_ipc::PickResult::Cancelled) => return (1, HashMap::new()),
                    Ok(other) => {
                        log::warn!("portal: region pick answered with {other:?}");
                        return (2, HashMap::new());
                    }
                    Err(error) => {
                        log::warn!("portal: region pick for '{app_id}' failed: {error}");
                        return (2, HashMap::new());
                    }
                }
            } else {
                None
            };
            if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            // A non-interactive legacy capture has no picker interaction of
            // its own. If the frontend did not already verify PermissionStore
            // consent, require an explicit compositor confirmation here.
            if !*interactive && target.is_none() && !*permission_store_checked {
                match capture.pick_confirm(
                    "Take a Screenshot".to_string(),
                    format!("Allow {app_id} to capture the current monitor?"),
                    Some("Capture".to_string()),
                ) {
                    Ok(atrium_portal_ipc::ConfirmPickResult::Confirmed) => {}
                    Ok(atrium_portal_ipc::ConfirmPickResult::Cancelled) => {
                        return (1, HashMap::new());
                    }
                    Err(error) => {
                        log::warn!(
                            "portal: screenshot confirmation for '{app_id}' failed: {error}"
                        );
                        return (2, HashMap::new());
                    }
                }
            }
            if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            match capture_and_write(capture, token, region) {
                Ok(uri) => {
                    // A Close racing the capture wins over a completed result.
                    if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                        return (1, HashMap::new());
                    }
                    log::info!("portal: screenshot for '{app_id}' → {uri}");
                    (0, HashMap::from([("uri".to_string(), Value::from(uri))]))
                }
                Err(error) => {
                    log::warn!("portal: screenshot for '{app_id}' failed: {error}");
                    (2, HashMap::new())
                }
            }
        }
        CaptureJob::PickColor {
            request_path,
            app_id,
            ..
        } => {
            if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            match capture.pick(atrium_portal_ipc::PickKind::Pixel) {
                Ok(atrium_portal_ipc::PickResult::Pixel { rgb, .. }) => {
                    if sync::lock(tracker, "screenshot tracker").was_closed(request_path) {
                        return (1, HashMap::new());
                    }
                    log::info!(
                        "portal: PickColor for '{app_id}' → #{:02x}{:02x}{:02x}",
                        rgb[0],
                        rgb[1],
                        rgb[2]
                    );
                    (0, HashMap::from([("color".to_string(), color_value(rgb))]))
                }
                Ok(atrium_portal_ipc::PickResult::Cancelled) => (1, HashMap::new()),
                Ok(other) => {
                    log::warn!("portal: pixel pick answered with {other:?}");
                    (2, HashMap::new())
                }
                Err(error) => {
                    log::warn!("portal: PickColor for '{app_id}' failed: {error}");
                    (2, HashMap::new())
                }
            }
        }
    }
}

/// The PickColor `color` result: `(ddd)` red/green/blue as 0–1 doubles.
fn color_value(rgb: [u8; 3]) -> Value<'static> {
    Value::Structure(zbus::zvariant::Structure::from((
        f64::from(rgb[0]) / 255.0,
        f64::from(rgb[1]) / 255.0,
        f64::from(rgb[2]) / 255.0,
    )))
}

/// Capture the focused output (or one region of it) and persist the PNG
/// under the portal cache directory, returning the `file://` URI the portal
/// contract expects.
fn capture_and_write(
    capture: &mut PortalCapture,
    token: &str,
    region: Option<atrium_portal_ipc::Rect>,
) -> std::io::Result<String> {
    let dir = files::cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither $XDG_CACHE_HOME nor $XDG_RUNTIME_DIR is set",
        )
    })?;
    let png = match region {
        Some(region) => capture.capture_region_png(region)?,
        None => capture.capture_png()?,
    };
    let path = files::write_capture(&dir, token, &png)?;
    Ok(files::file_uri(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn options_default_to_non_interactive() {
        let parsed = parse_options(&HashMap::new()).unwrap();
        assert!(!parsed.interactive);
        assert_eq!(parsed.target, None);
        assert!(!parsed.permission_store_checked);
    }

    #[test]
    fn options_parse_interactive() {
        let parsed = parse_options(&options(&[("interactive", Value::from(true))])).unwrap();
        assert!(parsed.interactive);
    }

    #[test]
    fn wrong_typed_options_are_ignored() {
        let parsed = parse_options(&options(&[("interactive", Value::from("yes"))])).unwrap();
        assert!(!parsed.interactive);
    }

    #[test]
    fn area_target_is_supported_and_unadvertised_targets_are_rejected() {
        let parsed =
            parse_options(&options(&[("target", Value::from(SCREENSHOT_TARGET_AREA))])).unwrap();
        assert_eq!(parsed.target, Some(ScreenshotTarget::Area));

        for target in [1_u32, 2, 8, 3] {
            assert!(
                parse_options(&options(&[("target", Value::from(target))])).is_err(),
                "target {target} must not be accepted unless advertised"
            );
        }
    }

    #[test]
    fn permission_store_hint_is_parsed_fail_closed() {
        let checked =
            parse_options(&options(&[("permission_store_checked", Value::from(true))])).unwrap();
        assert!(checked.permission_store_checked);

        let wrong_type = parse_options(&options(&[(
            "permission_store_checked",
            Value::from("yes"),
        )]))
        .unwrap();
        assert!(!wrong_type.permission_store_checked);
    }

    #[test]
    fn token_sanitization_replaces_path_separators() {
        assert_eq!(sanitize_token("a/b-c.d"), "a_b_c_d");
    }

    #[test]
    fn screenshot_version_is_3_with_area_target_only() {
        assert_eq!(SCREENSHOT_VERSION, 3);
        assert_eq!(SCREENSHOT_TARGET_AREA, 4);
    }

    #[test]
    fn color_result_is_a_ddd_structure_of_unit_doubles() {
        let value = color_value([255, 128, 0]);
        assert_eq!(
            value.value_signature().to_string(),
            "(ddd)",
            "the spec's color result is a (ddd) structure"
        );
        let Value::Structure(structure) = &value else {
            panic!("color must be a structure");
        };
        let fields = structure.fields();
        assert_eq!(fields.len(), 3);
        let channels: Vec<f64> = fields
            .iter()
            .map(|field| f64::try_from(field).expect("double channel"))
            .collect();
        assert_eq!(channels[0], 1.0);
        assert!((channels[1] - 128.0 / 255.0).abs() < f64::EPSILON);
        assert_eq!(channels[2], 0.0);
    }
}
