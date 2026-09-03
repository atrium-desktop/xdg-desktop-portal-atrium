//! Portal-owned projection of Tessera IPC protocol version 29.
//!
//! Only compositor-owned portal resources belong here. The wire types are
//! implemented independently from the compositor's Rust model so an internal
//! Tessera refactor cannot become a Portal build dependency.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Newest protocol version this projection speaks. The handshake asks for
/// this version and accepts a downgrade to [`MIN_PROTOCOL_VERSION`] when the
/// compositor is older (version-gated features, such as dmabuf slot
/// streaming at 25 and the protocol-29 additions — output enumeration,
/// per-output stream targets, the stream cursor mode, and the
/// `StreamGeometryChanged` event — key off the negotiated version).
/// Protocol 26 (`CaptureWindow`), 27 (`LaunchApp`, `Focus.reveal`), and 28
/// are deliberately not projected: no Portal interface needs them.
pub const PROTOCOL_VERSION: u32 = 29;
/// Oldest protocol version this projection can negotiate down to.
pub const MIN_PROTOCOL_VERSION: u32 = 24;
pub const LOCAL_PORTAL_SCOPE: &str = "atrium-portal";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCapabilities {
    pub query: bool,
    pub control: bool,
    #[serde(default)]
    pub input: bool,
    pub session: bool,
    #[serde(default)]
    pub interaction_domain: bool,
}

impl ConnectionCapabilities {
    pub const QUERY: Self = Self {
        query: true,
        control: false,
        input: false,
        session: false,
        interaction_domain: false,
    };

    pub fn privileged(self) -> bool {
        self.control || self.input || self.session || self.interaction_domain
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseRequest {
    pub ttl_ms: u64,
}

impl Default for LeaseRequest {
    fn default() -> Self {
        Self { ttl_ms: 900_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub id: u64,
    pub ttl_ms: u64,
    pub renewable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            origin: Point { x, y },
            size: Size { w, h },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorScheme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Contrast {
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccentColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl AccentColor {
    #[must_use]
    pub fn normalized(self) -> (f64, f64, f64) {
        (
            f64::from(self.red) / 255.0,
            f64::from(self.green) / 255.0,
            f64::from(self.blue) / 255.0,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopPreferences {
    pub color_scheme: ColorScheme,
    pub accent_color: Option<AccentColor>,
    pub contrast: Contrast,
    pub reduced_motion: bool,
    pub font_name: String,
    pub monospace_font_name: String,
    pub text_scale: f64,
    pub icon_theme: String,
    pub cursor_theme: String,
    pub cursor_size: u32,
}

impl Default for DesktopPreferences {
    fn default() -> Self {
        Self {
            color_scheme: ColorScheme::System,
            accent_color: None,
            contrast: Contrast::Normal,
            reduced_motion: false,
            font_name: "Sans 10".into(),
            monospace_font_name: "Monospace 10".into(),
            text_scale: 1.0,
            icon_theme: "hicolor".into(),
            cursor_theme: "default".into(),
            cursor_size: 24,
        }
    }
}

/// Partial settings snapshot. Serde intentionally ignores the compositor's
/// touchpad, display, and idle fields because the Settings portal exposes
/// only desktop preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub preferences: DesktopPreferences,
}

/// One compositor output as `EnumerateOutputs` reports it (protocol 29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub connector: String,
    pub primary: bool,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamPixelFormat {
    Bgra8,
    Rgba8,
    Dmabuf { drm_format: u32, modifier: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamTarget {
    /// An output stream. `output` names the connector to stream (protocol
    /// 29); `None` streams the whole desktop and is the only shape protocol
    /// 28 and older speak, so it serializes as bare `{"type":"Output"}`.
    Output {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    Window {
        window: WindowId,
    },
}

impl Default for StreamTarget {
    fn default() -> Self {
        Self::Output { output: None }
    }
}

impl StreamTarget {
    /// True for the default whole-desktop output target: the wire default,
    /// omitted from `StreamOutputStart`. A connector-named output target is
    /// not the default and always crosses the wire.
    pub(crate) fn is_output(&self) -> bool {
        matches!(self, Self::Output { output: None })
    }
}

/// Cursor composition mode of a stream (protocol 29). `hidden` — the wire
/// and compositor default — never paints the cursor; `embedded` composites
/// it into the frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamCursorMode {
    #[default]
    Hidden,
    Embedded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PickKind {
    Region,
    Pixel,
    Window,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PickResult {
    Region {
        rect: Rect,
    },
    Pixel {
        point: Point,
        rgb: [u8; 3],
    },
    Window {
        id: WindowId,
    },
    /// The picked output. `connector` names it (protocol 29); older
    /// compositors report no connector, so a bare `{"type":"Output"}` still
    /// deserializes with `connector: None`.
    Output {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connector: Option<String>,
    },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConfirmPickResult {
    Confirmed,
    Cancelled,
}

/// The compositor's `SetWallpaper` path rule
/// (`ActorResource::FilesystemPath { access: Read }::validate`), mirrored so
/// the client refuses a request the compositor would reject anyway: a
/// bounded absolute path in its one canonical lexical spelling. The bound is
/// 4_096 bytes, like the real check.
pub(crate) fn valid_wallpaper_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    if !path.is_absolute()
        || path.as_os_str().len() > 4_096
        || path.as_os_str().as_bytes().contains(&0)
    {
        return false;
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return false;
    }
    let normalized = path.components().collect::<PathBuf>();
    normalized.as_os_str().as_bytes() == path.as_os_str().as_bytes()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    SettingsChanged {
        revision: u64,
    },
    StreamFrame {
        stream_id: u64,
        sequence: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: StreamPixelFormat,
        damage: Vec<Rect>,
        dropped: u64,
        byte_len: u64,
        /// dmabuf slot index (protocol 25); no blob follows such frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot: Option<u32>,
    },
    StreamEnded {
        stream_id: u64,
        reason: String,
    },
    /// The stream's output geometry changed (protocol 29). After this
    /// event the compositor produces no further frames for the stream
    /// until the client restarts it (`StreamOutputStop` +
    /// `StreamOutputStart`).
    StreamGeometryChanged {
        stream_id: u64,
        width: u32,
        height: u32,
    },
    #[serde(skip)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Request {
    Hello {
        version: u32,
        caps: ConnectionCapabilities,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<LeaseRequest>,
    },
    GetSettings,
    Subscribe,
    RenewLease {
        ttl_ms: u64,
    },
    /// List the compositor's outputs (protocol 29).
    EnumerateOutputs,
    CaptureOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Rect>,
    },
    StreamOutputStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_fps: Option<u32>,
        #[serde(default, skip_serializing_if = "StreamTarget::is_output")]
        target: StreamTarget,
        /// Opt in to a zero-copy dmabuf slot stream (protocol 25). A client
        /// that does not opt in never receives a dmabuf announcement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dmabuf: Option<bool>,
        /// Cursor composition mode (protocol 29); absent asks for the
        /// compositor default (`hidden`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<StreamCursorMode>,
    },
    /// Release a dmabuf stream slot (protocol 25) after the PipeWire
    /// consumer returned the buffer bound to it.
    StreamBufferRelease {
        stream_id: u64,
        slot: u32,
    },
    StreamOutputStop {
        stream_id: u64,
    },
    PickTarget {
        kind: PickKind,
    },
    PickConfirm {
        title: String,
        body: String,
        accept_label: Option<String>,
    },
    /// Replace the desktop wallpaper with the image at `path` (the
    /// Wallpaper portal). The op predates this projection's version floor
    /// (the compositor has spoken it since protocol 17), so no version gate
    /// applies. The compositor decodes on its main loop and swaps live; the
    /// reply is an authoritative receipt, not a queue acknowledgment. It is
    /// fail-closed: `control`, a live lease, an explicit scope op, and a
    /// bounded absolute lexically-normalized path
    /// ([`valid_wallpaper_path`]).
    SetWallpaper {
        path: PathBuf,
    },
}

/// Partial response projection. Unknown fields inside known responses are
/// intentionally ignored, while unknown response variants fail closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Response {
    Hello {
        version: u32,
        caps: ConnectionCapabilities,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<LeaseGrant>,
    },
    Settings {
        snapshot: SettingsSnapshot,
    },
    /// Reply to [`Request::EnumerateOutputs`] (protocol 29).
    Outputs {
        outputs: Vec<OutputInfo>,
    },
    CaptureOutput {
        width: u32,
        height: u32,
        png_bytes: u64,
    },
    StreamOutputStarted {
        stream_id: u64,
        width: u32,
        height: u32,
        format: StreamPixelFormat,
        /// dmabuf slot count (protocol 25); the reply is followed by this
        /// many slot descriptors on the blob channel, in slot order.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slots: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_stride: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_bytes: Option<u64>,
    },
    /// Reply to [`Request::StreamBufferRelease`] (protocol 25).
    StreamBufferReleased {
        stream_id: u64,
        slot: u32,
    },
    StreamOutputStopped {
        stream_id: u64,
    },
    Picked {
        result: PickResult,
    },
    ConfirmPicked {
        result: ConfirmPickResult,
    },
    /// Reply to [`Request::SetWallpaper`]: the wallpaper was decoded and
    /// swapped (an authoritative main-loop receipt). Refusals arrive as
    /// `Error`.
    WallpaperSet {},
    LeaseRenewed {
        lease: LeaseGrant,
    },
    Subscribed,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmabuf_stream_frames_match_the_v24_wire_shape() {
        // DRM_FORMAT_XRGB8888 with DRM_FORMAT_MOD_LINEAR.
        let format = StreamPixelFormat::Dmabuf {
            drm_format: 0x3432_5258,
            modifier: 0,
        };
        assert_eq!(
            serde_json::to_value(format).unwrap(),
            serde_json::json!({
                "type": "Dmabuf",
                "drm_format": 875713112,
                "modifier": 0
            })
        );
        // A full StreamFrame event as the compositor emits it for a
        // single-plane dmabuf: the descriptor follows out of band and the
        // header carries the plane stride and the buffer's byte length.
        let event: Event = serde_json::from_value(serde_json::json!({
            "type": "StreamFrame",
            "stream_id": 3,
            "sequence": 41,
            "width": 1920,
            "height": 1080,
            "stride": 7680,
            "format": {
                "type": "Dmabuf",
                "drm_format": 875713112,
                "modifier": 72057594037927937_u64
            },
            "damage": [{ "origin": { "x": 0, "y": 0 }, "size": { "w": 1920, "h": 1080 } }],
            "dropped": 0,
            "byte_len": 8294400
        }))
        .unwrap();
        let Event::StreamFrame {
            stream_id,
            stride,
            format,
            byte_len,
            ..
        } = event
        else {
            panic!("dmabuf stream frame");
        };
        assert_eq!(stream_id, 3);
        assert_eq!(stride, 7680);
        assert_eq!(byte_len, 8294400);
        assert_eq!(
            format,
            StreamPixelFormat::Dmabuf {
                drm_format: 0x3432_5258,
                modifier: 0x0100_0000_0000_0001
            }
        );
    }

    #[test]
    fn hello_matches_the_v29_wire_shape() {
        // The literal 29 pins this fixture to the v29 shape; PROTOCOL_VERSION
        // must equal it.
        let request = Request::Hello {
            version: PROTOCOL_VERSION,
            caps: ConnectionCapabilities::QUERY,
            scope: None,
            lease: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "type": "Hello",
                "version": 29,
                "caps": {
                    "query": true,
                    "control": false,
                    "input": false,
                    "session": false,
                    "interaction_domain": false
                },
                "scope": null
            })
        );
    }

    #[test]
    fn wallpaper_operations_match_the_real_wire_fixtures() {
        // The literals below were produced by serializing the compositor's
        // own schema types (tessera-ipc `Request`/`Response`) with serde_json,
        // and deserialize against it in both directions.
        let request = Request::SetWallpaper {
            path: PathBuf::from("/run/user/1000/atrium-portal/wallpaper/current.png"),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "type": "SetWallpaper",
                "path": "/run/user/1000/atrium-portal/wallpaper/current.png"
            })
        );
        let request: Request = serde_json::from_value(serde_json::json!({
            "type": "SetWallpaper",
            "path": "/run/user/1000/atrium-portal/wallpaper/current.png"
        }))
        .unwrap();
        assert_eq!(
            request,
            Request::SetWallpaper {
                path: PathBuf::from("/run/user/1000/atrium-portal/wallpaper/current.png"),
            }
        );

        assert_eq!(
            serde_json::to_value(Response::WallpaperSet {}).unwrap(),
            serde_json::json!({ "type": "WallpaperSet" })
        );
        let response: Response =
            serde_json::from_value(serde_json::json!({ "type": "WallpaperSet" })).unwrap();
        assert_eq!(response, Response::WallpaperSet {});
    }

    #[test]
    fn wallpaper_path_validation_matches_the_compositor_rule() {
        assert!(valid_wallpaper_path(Path::new(
            "/run/user/1000/atrium-portal/wallpaper/current.png"
        )));
        assert!(valid_wallpaper_path(Path::new("/a")));
        // Relative paths, dot components, redundant separators, trailing
        // slashes, and oversized paths are all refused.
        assert!(!valid_wallpaper_path(Path::new("wall.png")));
        assert!(!valid_wallpaper_path(Path::new("relative/wall.png")));
        assert!(!valid_wallpaper_path(Path::new("/run/../etc/wall.png")));
        assert!(!valid_wallpaper_path(Path::new("/run/./wall.png")));
        assert!(!valid_wallpaper_path(Path::new("/run//wall.png")));
        assert!(!valid_wallpaper_path(Path::new("/run/wall.png/")));
        let oversized = PathBuf::from(format!("/{}", "a".repeat(4_096)));
        assert!(!valid_wallpaper_path(&oversized));
        let at_bound = PathBuf::from(format!("/{}", "a".repeat(4_095)));
        assert!(valid_wallpaper_path(&at_bound));
    }

    #[test]
    fn portal_operations_match_v24_wire_fixtures() {
        let fixtures = [
            (
                Request::CaptureOutput { region: None },
                serde_json::json!({ "type": "CaptureOutput" }),
            ),
            (
                Request::CaptureOutput {
                    region: Some(Rect::new(1, 2, 3, 4)),
                },
                serde_json::json!({
                    "type": "CaptureOutput",
                    "region": {
                        "origin": { "x": 1, "y": 2 },
                        "size": { "w": 3, "h": 4 }
                    }
                }),
            ),
            (
                Request::PickTarget {
                    kind: PickKind::Pixel,
                },
                serde_json::json!({
                    "type": "PickTarget",
                    "kind": { "type": "Pixel" }
                }),
            ),
            (
                Request::PickConfirm {
                    title: "Capture".into(),
                    body: "Allow capture?".into(),
                    accept_label: None,
                },
                serde_json::json!({
                    "type": "PickConfirm",
                    "title": "Capture",
                    "body": "Allow capture?",
                    "accept_label": null
                }),
            ),
            (
                Request::StreamOutputStart {
                    max_fps: Some(30),
                    target: StreamTarget::Output { output: None },
                    dmabuf: None,
                    cursor: None,
                },
                serde_json::json!({
                    "type": "StreamOutputStart",
                    "max_fps": 30
                }),
            ),
            (
                Request::StreamOutputStart {
                    max_fps: Some(60),
                    target: StreamTarget::Output { output: None },
                    dmabuf: Some(true),
                    cursor: None,
                },
                serde_json::json!({
                    "type": "StreamOutputStart",
                    "max_fps": 60,
                    "dmabuf": true
                }),
            ),
            (
                Request::StreamBufferRelease {
                    stream_id: 7,
                    slot: 2,
                },
                serde_json::json!({
                    "type": "StreamBufferRelease",
                    "stream_id": 7,
                    "slot": 2
                }),
            ),
            (
                Request::StreamOutputStop { stream_id: 7 },
                serde_json::json!({
                    "type": "StreamOutputStop",
                    "stream_id": 7
                }),
            ),
        ];
        for (request, fixture) in fixtures {
            assert_eq!(serde_json::to_value(request).unwrap(), fixture);
        }
    }

    #[test]
    fn v29_operations_match_their_wire_fixtures() {
        // EnumerateOutputs, both directions.
        assert_eq!(
            serde_json::to_value(Request::EnumerateOutputs).unwrap(),
            serde_json::json!({ "type": "EnumerateOutputs" })
        );
        let request: Request =
            serde_json::from_value(serde_json::json!({ "type": "EnumerateOutputs" })).unwrap();
        assert_eq!(request, Request::EnumerateOutputs);
        let outputs = Response::Outputs {
            outputs: vec![
                OutputInfo {
                    connector: "HDMI-A-1".into(),
                    primary: true,
                    rect: Rect::new(0, 0, 1920, 1080),
                },
                OutputInfo {
                    connector: "DP-1".into(),
                    primary: false,
                    rect: Rect::new(1920, 0, 2560, 1440),
                },
            ],
        };
        let fixture = serde_json::json!({
            "type": "Outputs",
            "outputs": [
                {
                    "connector": "HDMI-A-1",
                    "primary": true,
                    "rect": { "origin": { "x": 0, "y": 0 }, "size": { "w": 1920, "h": 1080 } }
                },
                {
                    "connector": "DP-1",
                    "primary": false,
                    "rect": { "origin": { "x": 1920, "y": 0 }, "size": { "w": 2560, "h": 1440 } }
                }
            ]
        });
        assert_eq!(serde_json::to_value(&outputs).unwrap(), fixture);
        let parsed: Response = serde_json::from_value(fixture).unwrap();
        assert_eq!(parsed, outputs);

        // StreamTarget: the legacy bare `{"type":"Output"}` (v≤28) and the
        // connector-carrying v29 shape, both directions.
        let legacy = serde_json::json!({ "type": "Output" });
        assert_eq!(
            serde_json::to_value(StreamTarget::Output { output: None }).unwrap(),
            legacy
        );
        let parsed: StreamTarget = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed, StreamTarget::Output { output: None });
        let addressed = serde_json::json!({ "type": "Output", "output": "HDMI-A-1" });
        assert_eq!(
            serde_json::to_value(StreamTarget::Output {
                output: Some("HDMI-A-1".into())
            })
            .unwrap(),
            addressed
        );
        let parsed: StreamTarget = serde_json::from_value(addressed).unwrap();
        assert_eq!(
            parsed,
            StreamTarget::Output {
                output: Some("HDMI-A-1".into())
            }
        );

        // StreamOutputStart with a cursor mode and a connector target.
        let request = Request::StreamOutputStart {
            max_fps: Some(60),
            target: StreamTarget::Output {
                output: Some("HDMI-A-1".into()),
            },
            dmabuf: Some(true),
            cursor: Some(StreamCursorMode::Embedded),
        };
        let fixture = serde_json::json!({
            "type": "StreamOutputStart",
            "max_fps": 60,
            "target": { "type": "Output", "output": "HDMI-A-1" },
            "dmabuf": true,
            "cursor": "embedded"
        });
        assert_eq!(serde_json::to_value(&request).unwrap(), fixture);
        let parsed: Request = serde_json::from_value(fixture).unwrap();
        assert_eq!(parsed, request);
        // The cursor modes are kebab-case on the wire; `hidden` is the
        // default and serializes inside the option only when asked for.
        assert_eq!(
            serde_json::to_value(StreamCursorMode::Hidden).unwrap(),
            serde_json::json!("hidden")
        );
        assert_eq!(
            serde_json::to_value(StreamCursorMode::Embedded).unwrap(),
            serde_json::json!("embedded")
        );

        // StreamGeometryChanged: after it the compositor sends no further
        // frames until the client restarts the stream.
        let event: Event = serde_json::from_value(serde_json::json!({
            "type": "StreamGeometryChanged",
            "stream_id": 3,
            "width": 2560,
            "height": 1440
        }))
        .unwrap();
        assert_eq!(
            event,
            Event::StreamGeometryChanged {
                stream_id: 3,
                width: 2560,
                height: 1440
            }
        );

        // PickTarget for an output, and the picked result with and without
        // a connector. The legacy bare `{"type":"Output"}` result (v≤28)
        // deserializes with `connector: None`.
        assert_eq!(
            serde_json::to_value(Request::PickTarget {
                kind: PickKind::Output
            })
            .unwrap(),
            serde_json::json!({
                "type": "PickTarget",
                "kind": { "type": "Output" }
            })
        );
        let legacy_result = serde_json::json!({ "type": "Output" });
        let parsed: PickResult = serde_json::from_value(legacy_result).unwrap();
        assert_eq!(parsed, PickResult::Output { connector: None });
        assert_eq!(
            serde_json::to_value(PickResult::Output { connector: None }).unwrap(),
            serde_json::json!({ "type": "Output" })
        );
        let addressed_result = serde_json::json!({ "type": "Output", "connector": "DP-1" });
        let parsed: PickResult = serde_json::from_value(addressed_result).unwrap();
        assert_eq!(
            parsed,
            PickResult::Output {
                connector: Some("DP-1".into())
            }
        );
        assert_eq!(
            serde_json::to_value(PickResult::Output {
                connector: Some("DP-1".into())
            })
            .unwrap(),
            serde_json::json!({ "type": "Output", "connector": "DP-1" })
        );
    }

    #[test]
    fn hello_response_ignores_non_portal_v24_fields() {
        let response: Response = serde_json::from_value(serde_json::json!({
            "type": "Hello",
            "version": 24,
            "caps": {
                "query": true,
                "control": true,
                "input": false,
                "session": false,
                "interaction_domain": false
            },
            "scope": { "name": "atrium-portal", "ops": ["CaptureOutput"] },
            "lease": { "id": 9, "ttl_ms": 900000, "renewable": true },
            "session": { "id": "ignored" },
            "agent": null
        }))
        .unwrap();
        let Response::Hello {
            version,
            caps,
            lease,
        } = response
        else {
            panic!("hello response");
        };
        assert_eq!(version, MIN_PROTOCOL_VERSION);
        assert!(caps.control);
        assert_eq!(lease.unwrap().id, 9);
    }

    #[test]
    fn real_settings_response_ignores_non_portal_fields() {
        let response: Response = serde_json::from_value(serde_json::json!({
            "type": "Settings",
            "snapshot": {
                "revision": 9,
                "touchpad": { "available": false, "config": {} },
                "display": { "configurable": false, "outputs": [], "error": null },
                "preferences": {
                    "color_scheme": "dark",
                    "accent_color": { "red": 1, "green": 2, "blue": 3 },
                    "contrast": "normal",
                    "reduced_motion": false,
                    "font_name": "Sans 10",
                    "monospace_font_name": "Monospace 10",
                    "text_scale": 1.0,
                    "icon_theme": "hicolor",
                    "cursor_theme": "default",
                    "cursor_size": 24
                },
                "idle": {}
            }
        }))
        .unwrap();
        let Response::Settings { snapshot } = response else {
            panic!("settings response");
        };
        assert_eq!(snapshot.preferences.color_scheme, ColorScheme::Dark);
    }
}
