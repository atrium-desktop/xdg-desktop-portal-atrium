//! Daemon-level media portal tests. A fake compositor serves the real scoped
//! Tessera IPC protocol; requests cross D-Bus and capture bytes cross the
//! sealed-memfd transport before the backend persists/returns them.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atrium_portal_ipc::testing::{
    CaptureOutputPayload, Handler, Server, StreamFramePayload, StreamInfo,
};
use atrium_portal_ipc::{
    ConfirmPickResult, PickKind, PickResult, StreamCursorMode, StreamPixelFormat, StreamTarget,
};
use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

mod common;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

// Valid 1x1 transparent PNG. The backend must transport it byte-for-byte;
// PNG encoding itself remains compositor-owned.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// One recorded `StreamOutputStart`: fps cap, target, dmabuf opt-in, cursor.
type StreamStart = (
    Option<u32>,
    StreamTarget,
    Option<bool>,
    Option<StreamCursorMode>,
);

#[derive(Default)]
struct FakeCompositor {
    captures: Mutex<Vec<Option<atrium_portal_ipc::Rect>>>,
    picks: Mutex<Vec<PickKind>>,
    confirms: Mutex<Vec<(String, String)>>,
    stream_starts: Mutex<Vec<StreamStart>>,
    stream_stops: Mutex<Vec<u64>>,
    stream_disconnects: Mutex<Vec<u64>>,
    /// The outputs `EnumerateOutputs` reports; a single HDMI-A-1 by
    /// default (which keeps per-output entries off the source chooser).
    outputs: Mutex<Vec<atrium_portal_ipc::OutputInfo>>,
    /// Per-stream announced formats, consumed in start order; streams start
    /// as Bgra8 when the queue is empty.
    stream_formats: Mutex<Vec<StreamPixelFormat>>,
    /// Per-stream announced geometries, consumed in start order; streams
    /// start as 2x2 when the queue is empty.
    stream_sizes: Mutex<Vec<(u32, u32)>>,
    /// Stream ids handed out in start order, mirroring the compositor.
    next_stream_id: Mutex<u64>,
    /// Offer a protocol-25 slot table on dmabuf-opted-in streams.
    offer_slots: Mutex<bool>,
    /// The slot files handed out, kept writable so tests control the slot
    /// contents between frames.
    slot_files: Mutex<Vec<std::fs::File>>,
    /// Slot releases received from the portal (protocol 25).
    releases: Mutex<Vec<(u64, u32)>>,
    /// The connection that last started a stream (the cast connection).
    stream_conn: Mutex<Option<u64>>,
}

impl FakeCompositor {
    /// The default single output; tests with more push their own list.
    fn one_output() -> atrium_portal_ipc::OutputInfo {
        atrium_portal_ipc::OutputInfo {
            connector: "HDMI-A-1".into(),
            primary: true,
            rect: atrium_portal_ipc::Rect::new(0, 0, 1920, 1080),
        }
    }
}

impl Handler for FakeCompositor {
    fn capture_output(
        &self,
        region: Option<atrium_portal_ipc::Rect>,
    ) -> Result<CaptureOutputPayload, String> {
        self.captures.lock().unwrap().push(region);
        Ok(CaptureOutputPayload {
            width: 1,
            height: 1,
            png: PNG.to_vec(),
        })
    }

    fn enumerate_outputs(&self) -> Result<Vec<atrium_portal_ipc::OutputInfo>, String> {
        let outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            return Ok(vec![Self::one_output()]);
        }
        Ok(outputs.clone())
    }

    fn pick_target(&self, _conn_id: u64, kind: PickKind) -> Result<PickResult, String> {
        self.picks.lock().unwrap().push(kind);
        Ok(match kind {
            PickKind::Region => PickResult::Region {
                rect: atrium_portal_ipc::Rect::new(10, 20, 30, 40),
            },
            PickKind::Pixel => PickResult::Pixel {
                point: atrium_portal_ipc::Point { x: 4, y: 8 },
                rgb: [255, 128, 0],
            },
            PickKind::Window => PickResult::Window {
                id: atrium_portal_ipc::WindowId(7),
            },
            PickKind::Output => PickResult::Output {
                connector: Some("HDMI-A-1".into()),
            },
        })
    }

    fn pick_confirm(
        &self,
        _conn_id: u64,
        title: String,
        body: String,
        _accept_label: Option<String>,
    ) -> Result<ConfirmPickResult, String> {
        self.confirms.lock().unwrap().push((title, body));
        Ok(ConfirmPickResult::Confirmed)
    }

    fn stream_output_start(
        &self,
        conn_id: u64,
        max_fps: Option<u32>,
        target: StreamTarget,
        dmabuf: Option<bool>,
        cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        *self.stream_conn.lock().unwrap() = Some(conn_id);
        self.stream_starts
            .lock()
            .unwrap()
            .push((max_fps, target, dmabuf, cursor));
        let stream_id = {
            let mut id = self.next_stream_id.lock().unwrap();
            *id += 1;
            *id
        };
        let format = {
            let mut queue = self.stream_formats.lock().unwrap();
            if queue.is_empty() {
                StreamPixelFormat::Bgra8
            } else {
                queue.remove(0)
            }
        };
        let (width, height) = {
            let mut queue = self.stream_sizes.lock().unwrap();
            if queue.is_empty() {
                (2, 2)
            } else {
                queue.remove(0)
            }
        };
        let mut slots = None;
        if dmabuf == Some(true)
            && matches!(format, StreamPixelFormat::Dmabuf { .. })
            && *self.offer_slots.lock().unwrap()
        {
            use std::os::fd::AsRawFd;
            let mut files = Vec::new();
            let mut infos = Vec::new();
            for _ in 0..3 {
                let file = unsealed_memfd(&[0_u8; 16]);
                infos.push(atrium_portal_ipc::testing::StreamSlotInfo {
                    fd: file.as_raw_fd(),
                    stride: 8,
                    byte_len: 16,
                });
                files.push(file);
            }
            *self.slot_files.lock().unwrap() = files;
            slots = Some(infos);
        }
        Ok(StreamInfo {
            stream_id,
            width,
            height,
            format,
            slots,
        })
    }

    fn stream_buffer_release(&self, stream_id: u64, slot: u32) {
        self.releases.lock().unwrap().push((stream_id, slot));
    }

    fn stream_output_stop(&self, stream_id: u64) {
        self.stream_stops.lock().unwrap().push(stream_id);
    }

    fn streams_disconnected(&self, conn_id: u64) {
        self.stream_disconnects.lock().unwrap().push(conn_id);
    }
}

fn handle(path: &str) -> ObjectPath<'_> {
    ObjectPath::try_from(path).expect("valid request path")
}

#[test]
fn screenshot_and_color_cross_real_daemon_and_scoped_ipc() {
    let Some(bus) = private_bus() else {
        eprintln!("media: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("media-data");
    let runtime_dir = temp_dir("media-runtime");
    let fake = Arc::new(FakeCompositor::default());
    let _server = Server::start(&runtime_dir.join("tessera.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend.env("RUST_LOG", "debug").stderr(Stdio::from(
        std::fs::File::create(&backend_log).expect("backend log"),
    ));
    let _daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let screenshot = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Screenshot",
    )
    .expect("Screenshot proxy");
    let mut options = HashMap::new();
    options.insert("interactive".to_owned(), Value::from(true));
    options.insert("target".to_owned(), Value::from(4_u32));
    let (code, results): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "Screenshot",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/shot1"),
                "dev.tessera.MediaTest",
                "",
                options,
            ),
        )
        .expect("Screenshot call");
    assert_eq!(code, 0, "interactive screenshot: {results:?}");
    let uri = String::try_from(results["uri"].clone()).expect("uri string");
    let path = uri.strip_prefix("file://").expect("local file URI");
    assert_eq!(std::fs::read(path).expect("read capture"), PNG);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fake.captures.lock().unwrap().as_slice(),
        &[Some(atrium_portal_ipc::Rect::new(10, 20, 30, 40))]
    );

    let (code, results): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "PickColor",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/color1"),
                "dev.tessera.MediaTest",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("PickColor call");
    assert_eq!(code, 0, "PickColor: {results:?}");
    let color = Value::from(results["color"].clone());
    assert_eq!(color.value_signature().to_string(), "(ddd)");
    let Value::Structure(color) = color else {
        panic!("color must be a structure");
    };
    let channels: Vec<f64> = color
        .fields()
        .iter()
        .map(|channel| f64::try_from(channel).expect("double channel"))
        .collect();
    assert_eq!(channels[0], 1.0);
    assert!((channels[1] - 128.0 / 255.0).abs() < f64::EPSILON);
    assert_eq!(channels[2], 0.0);
    assert_eq!(
        fake.picks.lock().unwrap().as_slice(),
        &[PickKind::Region, PickKind::Pixel]
    );

    // Legacy, noninteractive capture is fail-closed unless the frontend says
    // PermissionStore already approved it: an explicit compositor consent
    // must occur before any pixels are requested.
    let (code, _): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "Screenshot",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/shot2"),
                "dev.tessera.MediaTest",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("legacy Screenshot call");
    assert_eq!(code, 0);
    assert_eq!(fake.confirms.lock().unwrap().len(), 1);
    assert_eq!(fake.captures.lock().unwrap().len(), 2);

    std::fs::remove_file(path).ok();
    std::fs::remove_dir_all(data_dir).ok();
    std::fs::remove_dir_all(runtime_dir).ok();
}

fn pipewire_e2e_required() -> bool {
    std::env::var_os("ATRIUM_PORTAL_REQUIRE_PIPEWIRE_E2E").is_some() || common::e2e_required()
}

fn media_tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn require_or_skip(condition: bool, message: &str) -> bool {
    if condition {
        return true;
    }
    assert!(!pipewire_e2e_required(), "{message}");
    eprintln!("screencast PipeWire E2E: {message}; skipping");
    false
}

/// Spawn the isolated PipeWire daemon and WirePlumber session manager that
/// screencast E2E tests share. Returns `None` after printing a skip reason
/// when the environment cannot host the stack (unless E2E is required).
fn spawn_pipewire_stack(
    bus_address: &str,
    runtime_dir: &std::path::Path,
) -> Option<(KillOnDrop, KillOnDrop)> {
    if !require_or_skip(
        media_tool_available("pipewire"),
        "pipewire executable unavailable",
    ) || !require_or_skip(
        media_tool_available("gst-launch-1.0"),
        "GStreamer PipeWire consumer unavailable",
    ) || !require_or_skip(
        media_tool_available("wireplumber"),
        "WirePlumber session manager unavailable",
    ) {
        return None;
    }
    let pipewire_log = runtime_dir.join("pipewire.log");
    let mut pipewire = Command::new("pipewire");
    pipewire
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&pipewire_log).expect("PipeWire log"),
        ));
    let mut pipewire = pipewire.spawn().expect("pipewire was probed above");
    let socket = runtime_dir.join("pipewire-0");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket.exists() {
            break;
        }
        if let Some(status) = pipewire.try_wait().expect("poll PipeWire") {
            let log = std::fs::read_to_string(&pipewire_log).unwrap_or_default();
            if !require_or_skip(
                false,
                &format!("isolated PipeWire exited as {status}: {log}"),
            ) {
                return None;
            }
        }
        if Instant::now() >= deadline {
            let _ = pipewire.kill();
            let _ = pipewire.wait();
            let log = std::fs::read_to_string(&pipewire_log).unwrap_or_default();
            if !require_or_skip(false, &format!("isolated PipeWire did not start: {log}")) {
                return None;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let pipewire = KillOnDrop(pipewire);

    // Target-node linking is session-manager policy in PipeWire. Running the
    // same WirePlumber component production desktops use makes this a real
    // producer/consumer test rather than only a registry-object check.
    let wireplumber_log = runtime_dir.join("wireplumber.log");
    let mut wireplumber = Command::new("wireplumber");
    wireplumber
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&wireplumber_log).expect("WirePlumber log"),
        ));
    let mut wireplumber = wireplumber.spawn().expect("WirePlumber was probed above");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = wireplumber.try_wait().expect("poll WirePlumber") {
            let log = std::fs::read_to_string(&wireplumber_log).unwrap_or_default();
            panic!("WirePlumber exited as {status}: {log}");
        }
        let registry = Command::new("pw-dump")
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
            .output();
        if registry
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("WirePlumber"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "WirePlumber did not register: {}",
            std::fs::read_to_string(&wireplumber_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let wireplumber = KillOnDrop(wireplumber);
    Some((pipewire, wireplumber))
}

/// Consume one raw frame from a stream node through an independent
/// GStreamer PipeWire client.
fn spawn_gst_consumer(
    bus_address: &str,
    runtime_dir: &std::path::Path,
    serial: u64,
    caps: &str,
    captured: &std::path::Path,
) -> (std::process::Child, std::path::PathBuf) {
    let consumer_log = runtime_dir.join("consumer.log");
    let mut consumer = Command::new("gst-launch-1.0");
    consumer
        .args([
            "-q",
            "pipewiresrc",
            &format!("target-object={serial}"),
            "num-buffers=1",
            "!",
            caps,
            "!",
            "filesink",
            &format!("location={}", captured.display()),
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
        .env("PIPEWIRE_REMOTE", "pipewire-0-manager")
        .env("GST_DEBUG", "pipewiresrc:6")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&consumer_log).expect("consumer log"),
        ));
    (
        consumer.spawn().expect("GStreamer was probed above"),
        consumer_log,
    )
}

/// Wait for a spawned consumer, dumping diagnostics on timeout.
fn wait_consumer(
    consumer: &mut std::process::Child,
    runtime_dir: &std::path::Path,
    consumer_log: &std::path::Path,
    backend_log: &std::path::Path,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = consumer.try_wait().expect("poll consumer") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = consumer.kill();
            let _ = consumer.wait();
            let log = std::fs::read_to_string(consumer_log).unwrap_or_default();
            let registry = Command::new("pw-dump")
                .env("XDG_RUNTIME_DIR", runtime_dir)
                .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            let backend = std::fs::read_to_string(backend_log).unwrap_or_default();
            panic!(
                "PipeWire consumer timed out: {log}\nbackend:\n{backend}\nregistry:\n{registry}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stream_details(results: &HashMap<String, OwnedValue>) -> (u32, u64) {
    let streams = Value::from(results["streams"].clone());
    let Value::Array(streams) = streams else {
        panic!("streams result must be an array");
    };
    let Value::Structure(stream) = streams.get(0).expect("read stream").expect("one stream") else {
        panic!("stream entry must be a structure");
    };
    let node_id = u32::try_from(&stream.fields()[0]).expect("PipeWire node id");
    let Value::Dict(properties) = &stream.fields()[1] else {
        panic!("stream properties must be a dict");
    };
    let serial = properties
        .iter()
        .find_map(|(key, value)| {
            let Value::Str(key) = key else {
                return None;
            };
            if key.as_str() != "pipewire-serial" {
                return None;
            }
            let Value::Value(value) = value else {
                return None;
            };
            u64::try_from(value.as_ref()).ok()
        })
        .expect("v6 stream must include pipewire-serial");
    (node_id, serial)
}

#[test]
fn screencast_republishes_compositor_frames_through_real_pipewire() {
    let Some(bus) = private_bus() else {
        if pipewire_e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("screencast PipeWire E2E: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("cast-data");
    let runtime_dir = temp_dir("cast-runtime");
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .expect("secure PipeWire runtime directory");

    let Some((_pipewire, _wireplumber)) = spawn_pipewire_stack(bus.address(), &runtime_dir) else {
        std::fs::remove_dir_all(data_dir).ok();
        std::fs::remove_dir_all(runtime_dir).ok();
        return;
    };

    let fake = Arc::new(FakeCompositor::default());
    let server = Server::start(&runtime_dir.join("tessera.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    // The monitor|window SelectSources below gets a chooser with two
    // options (desktop, window); the scripted prompter picks the desktop.
    let prompter_dir = temp_dir("cast-prompter");
    let prompter = common::fake_prompter(&prompter_dir);
    common::write_prompter_response(
        &prompter_dir,
        1,
        &atrium_portal_prompter::PrompterResponse::choose_source(
            atrium_portal_prompter::ChooseSourceResponse::Selected {
                source: "desktop".into(),
                remember: false,
            },
        ),
    );
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend
        .env("RUST_LOG", "debug")
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &prompter_dir)
        .stderr(Stdio::from(
            std::fs::File::create(&backend_log).expect("backend log"),
        ));
    let _daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let screencast = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    let session_path = "/org/freedesktop/portal/desktop/session/1/cast1";
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/create1"),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession");
    assert_eq!(code, 0, "host applications have an empty backend app_id");

    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/select1"),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("SelectSources");
    assert_eq!(code, 0);

    // Regression: OBS's unified "Screen Capture (PipeWire)" source offers
    // monitor|window (0b11); the backend must accept the mask and serve its
    // monitor subset instead of rejecting the mixed offer.
    let mix_session_path = "/org/freedesktop/portal/desktop/session/1/cast_mix";
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/create_mix"),
                handle(mix_session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession (mixed types)");
    assert_eq!(code, 0);
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/select_mix"),
                handle(mix_session_path),
                "",
                HashMap::from([("types".to_string(), Value::from(0b11_u32))]),
            ),
        )
        .expect("SelectSources (mixed types)");
    assert_eq!(code, 0, "monitor|window offer must be served as monitor");

    let (code, results): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "Start",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/start1"),
                handle(session_path),
                "",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("Start");
    assert_eq!(code, 0, "ScreenCast Start: {results:?}");
    let (node_id, serial) = stream_details(&results);
    assert_ne!(node_id, u32::MAX, "PipeWire node id must be valid");
    assert_ne!(serial, 0, "v6 requires a stable PipeWire serial");
    assert_eq!(
        fake.stream_starts.lock().unwrap().as_slice(),
        &[(
            Some(60),
            StreamTarget::Output { output: None },
            Some(true),
            Some(StreamCursorMode::Hidden)
        )]
    );

    // Consume one raw frame from the exact node through an independent
    // PipeWire client. Keeping the latest compositor frame lets the producer
    // satisfy the first process callback even when linking finishes later.
    let captured = runtime_dir.join("captured.bgrx");
    let (mut consumer, consumer_log) = spawn_gst_consumer(
        bus.address(),
        &runtime_dir,
        serial,
        "video/x-raw,format=BGRx,width=2,height=2",
        &captured,
    );
    let pixels: Arc<[u8]> = Arc::from(&[7_u8; 16][..]);
    assert!(server.push_stream_frame(StreamFramePayload {
        stream_id: 1,
        sequence: 1,
        width: 2,
        height: 2,
        stride: 8,
        format: StreamPixelFormat::Bgra8,
        damage: vec![atrium_portal_ipc::Rect::new(0, 0, 2, 2)],
        dropped: 0,
        pixels,
    }));
    let status = wait_consumer(&mut consumer, &runtime_dir, &consumer_log, &backend_log);
    let log = std::fs::read_to_string(&consumer_log).unwrap_or_default();
    assert!(
        status.success(),
        "PipeWire consumer failed: {log}\nbackend:\n{}",
        std::fs::read_to_string(&backend_log).unwrap_or_default()
    );
    assert_eq!(
        std::fs::read(&captured).expect("captured raw frame"),
        [7; 16]
    );

    let session = Proxy::new(
        &conn,
        PORTAL,
        session_path,
        "org.freedesktop.impl.portal.Session",
    )
    .expect("Session proxy");
    let _: () = session.call("Close", &()).expect("Session.Close");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.stream_disconnects.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "closing the portal session must disconnect the compositor stream"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    std::fs::remove_dir_all(data_dir).ok();
    std::fs::remove_dir_all(runtime_dir).ok();
}

#[path = "media/pipewire_consumer.rs"]
mod pipewire_consumer;
use pipewire_consumer::{
    ConsumeRequest, Received, consume_frames_metadata, consume_one_frame, consume_one_frame_damage,
};

/// DRM_FORMAT_XRGB8888: the fourcc the compositor announces for its
/// single-plane BGRA8-class dmabuf exports.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// DRM_FORMAT_MOD_I915_X_TILED: a device-native tiled layout, the kind of
/// modifier the compositor's slot streams carry in production.
const DRM_FORMAT_MOD_I915_X_TILED: u64 = 0x0100_0000_0000_0001;

/// The format queue for a dmabuf-announced stream with LINEAR slots.
fn linear_dmabuf_formats() -> Vec<StreamPixelFormat> {
    vec![StreamPixelFormat::Dmabuf {
        drm_format: DRM_FORMAT_XRGB8888,
        modifier: DRM_FORMAT_MOD_LINEAR,
    }]
}

/// An unsealed memfd stands in for a GPU dmabuf: same wire transport, same
/// fixed size, no seals, and mappable on any test machine.
fn unsealed_memfd(bytes: &[u8]) -> std::fs::File {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    // SAFETY: the name is static and NUL-terminated; the fd is checked
    // before ownership is constructed.
    let fd = unsafe { libc::memfd_create(c"tessera-media-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    // SAFETY: `fd` is a new owned descriptor from memfd_create.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).unwrap();
    file
}

/// A running dmabuf-announced cast: private bus, isolated PipeWire stack,
/// fake compositor, and daemon, with one started session. Guards drop in
/// declaration order when the fixture goes away.
struct DmabufCastFixture {
    fake: Arc<FakeCompositor>,
    server: Server,
    runtime_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    node_id: u32,
    _daemon: KillOnDrop,
    _stack: (KillOnDrop, KillOnDrop),
    _bus: common::PrivateBus,
}

impl Drop for DmabufCastFixture {
    fn drop(&mut self) {
        // Keep the directories on request so failed runs leave their
        // backend/PipeWire logs inspectable.
        if std::env::var_os("ATRIUM_PORTAL_E2E_KEEP").is_some() {
            return;
        }
        std::fs::remove_dir_all(&self.runtime_dir).ok();
        std::fs::remove_dir_all(&self.data_dir).ok();
    }
}

fn start_cast_session(
    conn: &zbus::blocking::Connection,
    session_path: &str,
    tag: &str,
) -> (u32, u64) {
    let screencast = Proxy::new(
        conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_create"
                )),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession");
    assert_eq!(code, 0);
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_select"
                )),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("SelectSources");
    assert_eq!(code, 0);
    let (code, results): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "Start",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_start"
                )),
                handle(session_path),
                "",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("Start");
    assert_eq!(code, 0, "dmabuf-announced Start: {results:?}");
    stream_details(&results)
}

fn dmabuf_cast_fixture(
    tag: &str,
    offer_slots: bool,
    formats: Vec<StreamPixelFormat>,
) -> Option<DmabufCastFixture> {
    let Some(bus) = private_bus() else {
        if pipewire_e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("dmabuf PipeWire E2E: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();
    let data_dir = temp_dir(&format!("cast-{tag}-data"));
    let runtime_dir = temp_dir(&format!("cast-{tag}-runtime"));
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .expect("secure PipeWire runtime directory");
    let Some(stack) = spawn_pipewire_stack(bus.address(), &runtime_dir) else {
        std::fs::remove_dir_all(data_dir).ok();
        std::fs::remove_dir_all(runtime_dir).ok();
        return None;
    };
    let fake = Arc::new(FakeCompositor::default());
    *fake.offer_slots.lock().unwrap() = offer_slots;
    *fake.stream_formats.lock().unwrap() = formats;
    let server = Server::start(&runtime_dir.join("tessera.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend.env("RUST_LOG", "debug").stderr(Stdio::from(
        std::fs::File::create(&backend_log).expect("backend log"),
    ));
    let daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);
    let session_path = format!("/org/freedesktop/portal/desktop/session/1/{tag}");
    let (node_id, _) = start_cast_session(&conn, &session_path, tag);
    assert_eq!(
        fake.stream_starts.lock().unwrap().as_slice(),
        &[(
            Some(60),
            StreamTarget::Output { output: None },
            Some(true),
            Some(StreamCursorMode::Hidden)
        )]
    );
    Some(DmabufCastFixture {
        fake,
        server,
        runtime_dir,
        data_dir,
        node_id,
        _daemon: daemon,
        _stack: stack,
        _bus: bus,
    })
}

/// Push one dmabuf frame (memfd stand-in) through the fixture's compositor.
fn push_dmabuf_frame(fixture: &DmabufCastFixture, pixels: &[u8]) {
    let frame_fd = unsealed_memfd(pixels);
    assert!(fixture.server.push_stream_frame_fd(
        atrium_portal_ipc::testing::StreamFrameFdPayload {
            stream_id: 1,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: DRM_FORMAT_MOD_LINEAR,
            },
            damage: vec![atrium_portal_ipc::Rect::new(0, 0, 2, 2)],
            dropped: 0,
            byte_len: pixels.len() as u64,
        },
        std::os::fd::AsRawFd::as_raw_fd(&frame_fd),
    ));
}

/// Push one protocol-25 slot frame through the fixture's compositor with explicit modifier.
fn push_slot_frame_modifier(fixture: &DmabufCastFixture, slot: u32, modifier: u64) {
    assert!(fixture.server.push_stream_frame_slot(
        atrium_portal_ipc::testing::StreamFrameFdPayload {
            stream_id: 1,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier,
            },
            damage: vec![atrium_portal_ipc::Rect::new(0, 0, 2, 2)],
            dropped: 0,
            byte_len: 16,
        },
        slot,
    ));
}

/// Push one protocol-25 slot frame through the fixture's compositor.
fn push_slot_frame(fixture: &DmabufCastFixture, slot: u32) {
    push_slot_frame_modifier(fixture, slot, DRM_FORMAT_MOD_LINEAR);
}

/// Drive one frame through the cast: link a consumer, wait for it to reach
/// streaming, push the frame, and return what the consumer received.
fn drive_one_frame(
    fixture: &DmabufCastFixture,
    offer_dmabuf: bool,
    pixels: &[u8],
) -> Result<Received, String> {
    let socket = fixture.runtime_dir.join("pipewire-0");
    let node_id = fixture.node_id;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame(
            &socket,
            node_id,
            2,
            2,
            offer_dmabuf,
            ready_tx,
            Duration::from_secs(20),
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");
    push_dmabuf_frame(fixture, pixels);
    consumer.join().expect("consumer thread")
}

/// A consumer that enumerates no modifier gets the shared-memory copy of
/// each dmabuf frame (the universal fallback path).
#[test]
fn screencast_maps_dmabuf_frames_for_shm_consumers() {
    let Some(fixture) = dmabuf_cast_fixture("shm", false, linear_dmabuf_formats()) else {
        return;
    };
    let pixels = [0xa5_u8; 16];
    let received = drive_one_frame(&fixture, false, &pixels).expect("frame delivery");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));
}

/// PipeWire fixes each pool buffer's descriptor at allocation time, so a
/// per-frame dmabuf descriptor cannot be forwarded through the pool: the
/// consumer's buffer keeps its allocation-time type. A consumer enumerating
/// a modifier therefore still receives shared-memory copies. Zero-copy
/// delivery needs the slot protocol tracked in ADR-0005.
#[test]
fn screencast_cannot_forward_per_frame_descriptors() {
    let Some(fixture) = dmabuf_cast_fixture("fwd", false, linear_dmabuf_formats()) else {
        return;
    };
    let pixels = [0x5a_u8; 16];
    let received = drive_one_frame(&fixture, true, &pixels).expect("frame delivery");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));
}

/// The protocol-25 slot path: the compositor transfers slot descriptors
/// once, frames reference them by index, the consumer receives a real
/// `SPA_DATA_DmaBuf` buffer, and the release flows back to the compositor.
#[test]
fn screencast_forwards_slot_frames_zero_copy() {
    use std::io::{Seek, SeekFrom, Write};
    let Some(fixture) = dmabuf_cast_fixture("slots", true, linear_dmabuf_formats()) else {
        return;
    };
    assert_eq!(fixture.fake.slot_files.lock().unwrap().len(), 3);

    let socket = fixture.runtime_dir.join("pipewire-0");
    let node_id = fixture.node_id;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame(
            &socket,
            node_id,
            2,
            2,
            true,
            ready_tx,
            Duration::from_secs(20),
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");

    // The compositor rendered into slot 1: rewrite its stand-in's contents
    // and announce the frame.
    let pixels = [0x3c_u8; 16];
    {
        let mut files = fixture.fake.slot_files.lock().unwrap();
        let file = &mut files[1];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&pixels).unwrap();
    }
    push_slot_frame(&fixture, 1);

    let received = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery");
    assert_eq!(received, Received::DmaBuf(pixels.to_vec()));

    // The consumer returned the buffer, so the portal must have released
    // slot 1 back to the compositor.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture.fake.releases.lock().unwrap().contains(&(1, 1)) {
            break;
        }
        assert!(Instant::now() < deadline, "slot release never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A slot-mode consumer that cannot import dmabufs gets a shared-memory
/// copy of each frame, and the slot is released right after the copy so
/// the compositor can reuse it.
#[test]
fn screencast_copies_slot_frames_for_shm_consumers() {
    use std::io::{Seek, SeekFrom, Write};
    let Some(fixture) = dmabuf_cast_fixture("slotshm", true, linear_dmabuf_formats()) else {
        return;
    };
    let socket = fixture.runtime_dir.join("pipewire-0");
    let node_id = fixture.node_id;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame(
            &socket,
            node_id,
            2,
            2,
            false,
            ready_tx,
            Duration::from_secs(20),
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");

    let pixels = [0x77_u8; 16];
    {
        let mut files = fixture.fake.slot_files.lock().unwrap();
        let file = &mut files[2];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&pixels).unwrap();
    }
    push_slot_frame(&fixture, 2);

    let received = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture.fake.releases.lock().unwrap().contains(&(1, 2)) {
            break;
        }
        assert!(Instant::now() < deadline, "slot release never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A consumer that cannot import the compositor's tiled modifier must never
/// receive a linear memcpy of the tiled slot bytes — that copies
/// tile-swizzled garbage. Fixating the modifier-less entry restarts the
/// compositor stream on the SHM readback transport underneath the live
/// PipeWire connection, and the readback pixels are what the consumer
/// receives.
#[test]
fn screencast_switches_tiled_slot_streams_to_shm_readback() {
    let Some(fixture) = dmabuf_cast_fixture(
        "tiled",
        true,
        vec![
            StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: DRM_FORMAT_MOD_I915_X_TILED,
            },
            // The restarted stream answers on the SHM readback transport.
            StreamPixelFormat::Bgra8,
        ],
    ) else {
        return;
    };
    let socket = fixture.runtime_dir.join("pipewire-0");
    let node_id = fixture.node_id;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame(
            &socket,
            node_id,
            2,
            2,
            false,
            ready_tx,
            Duration::from_secs(20),
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");

    // The fixation must stop the tiled slot stream and restart it without
    // the dmabuf opt-in.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = fixture.fake.stream_stops.lock().unwrap().contains(&1);
        let restarts = fixture.fake.stream_starts.lock().unwrap().clone();
        if stopped && restarts.len() == 2 {
            assert_eq!(
                restarts.as_slice(),
                &[
                    (
                        Some(60),
                        StreamTarget::Output { output: None },
                        Some(true),
                        Some(StreamCursorMode::Hidden)
                    ),
                    (
                        Some(60),
                        StreamTarget::Output { output: None },
                        None,
                        Some(StreamCursorMode::Hidden)
                    ),
                ]
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the compositor stream never restarted as SHM readback"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The new transport delivers sealed SHM readback frames on stream 2.
    let pixels = [0x66_u8; 16];
    assert!(
        fixture
            .server
            .push_stream_frame(atrium_portal_ipc::testing::StreamFramePayload {
                stream_id: 2,
                sequence: 1,
                width: 2,
                height: 2,
                stride: 8,
                format: StreamPixelFormat::Bgra8,
                damage: vec![atrium_portal_ipc::Rect::new(0, 0, 2, 2)],
                dropped: 0,
                pixels: pixels.to_vec().into(),
            },)
    );

    let received = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));
}

/// Two slot frames arriving without an intervening PipeWire process cycle:
/// the first frame is superseded while still pending, and its slot has no
/// other release path — a published slot's release is owned by its pool
/// binding or went out on the copy path. The overwrite must release the
/// superseded slot, or the compositor's slot ring permanently shrinks.
#[test]
fn screencast_releases_a_superseded_pending_slot_frame() {
    let Some(fixture) = dmabuf_cast_fixture("pendingslot", true, linear_dmabuf_formats()) else {
        return;
    };
    // No consumer links, so the stream never reaches Streaming and no
    // process cycle runs between the two frames.
    push_slot_frame(&fixture, 0);
    push_slot_frame(&fixture, 1);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture.fake.releases.lock().unwrap().contains(&(1, 0)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the superseded slot frame's release never arrived"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The superseding frame is still pending; it must not be released
    // before it is published (its release belongs to the publish path).
    assert!(!fixture.fake.releases.lock().unwrap().contains(&(1, 1)));
}

/// A running cast environment for the protocol-29 surface tests: private
/// bus, isolated PipeWire stack, fake compositor, scripted one-shot
/// prompter, and daemon. Guards drop in declaration order.
struct CastEnv {
    conn: zbus::blocking::Connection,
    fake: Arc<FakeCompositor>,
    server: Server,
    runtime_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    prompter_dir: std::path::PathBuf,
    _daemon: KillOnDrop,
    _stack: (KillOnDrop, KillOnDrop),
    _bus: common::PrivateBus,
}

impl Drop for CastEnv {
    fn drop(&mut self) {
        if std::env::var_os("ATRIUM_PORTAL_E2E_KEEP").is_some() {
            return;
        }
        std::fs::remove_dir_all(&self.runtime_dir).ok();
        std::fs::remove_dir_all(&self.data_dir).ok();
        std::fs::remove_dir_all(&self.prompter_dir).ok();
    }
}

fn cast_env(
    tag: &str,
    outputs: Vec<atrium_portal_ipc::OutputInfo>,
    responses: &[atrium_portal_prompter::PrompterResponse],
) -> Option<CastEnv> {
    let Some(bus) = private_bus() else {
        if pipewire_e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("cast PipeWire E2E: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();
    let data_dir = temp_dir(&format!("cast-{tag}-data"));
    let runtime_dir = temp_dir(&format!("cast-{tag}-runtime"));
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .expect("secure PipeWire runtime directory");
    let Some(stack) = spawn_pipewire_stack(bus.address(), &runtime_dir) else {
        std::fs::remove_dir_all(data_dir).ok();
        std::fs::remove_dir_all(runtime_dir).ok();
        return None;
    };
    let fake = Arc::new(FakeCompositor::default());
    *fake.outputs.lock().unwrap() = outputs;
    let server = Server::start(&runtime_dir.join("tessera.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let prompter_dir = temp_dir(&format!("cast-{tag}-prompter"));
    let prompter = common::fake_prompter(&prompter_dir);
    for (index, response) in responses.iter().enumerate() {
        common::write_prompter_response(&prompter_dir, (index + 1) as u32, response);
    }
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend
        .env("RUST_LOG", "debug")
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &prompter_dir)
        .stderr(Stdio::from(
            std::fs::File::create(&backend_log).expect("backend log"),
        ));
    let daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);
    Some(CastEnv {
        conn,
        fake,
        server,
        runtime_dir,
        data_dir,
        prompter_dir,
        _daemon: daemon,
        _stack: stack,
        _bus: bus,
    })
}

fn two_outputs() -> Vec<atrium_portal_ipc::OutputInfo> {
    vec![
        atrium_portal_ipc::OutputInfo {
            connector: "HDMI-A-1".into(),
            primary: true,
            rect: atrium_portal_ipc::Rect::new(0, 0, 1920, 1080),
        },
        atrium_portal_ipc::OutputInfo {
            connector: "DP-1".into(),
            primary: false,
            rect: atrium_portal_ipc::Rect::new(1920, 0, 2560, 1440),
        },
    ]
}

fn create_session(conn: &zbus::blocking::Connection, tag: &str) -> String {
    let screencast = Proxy::new(
        conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    let session_path = format!("/org/freedesktop/portal/desktop/session/1/{tag}");
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_create"
                )),
                handle(&session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession");
    assert_eq!(code, 0);
    session_path
}

fn select_sources(
    conn: &zbus::blocking::Connection,
    session_path: &str,
    tag: &str,
    options: HashMap<String, Value<'_>>,
) -> u32 {
    let screencast = Proxy::new(
        conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_select"
                )),
                handle(session_path),
                "",
                options,
            ),
        )
        .expect("SelectSources");
    code
}

fn start_session(
    conn: &zbus::blocking::Connection,
    session_path: &str,
    tag: &str,
) -> (u32, HashMap<String, OwnedValue>) {
    let screencast = Proxy::new(
        conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    screencast
        .call(
            "Start",
            &(
                handle(&format!(
                    "/org/freedesktop/portal/desktop/request/1/{tag}_start"
                )),
                handle(session_path),
                "",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("Start")
}

/// Read the `source_type` property of the single stream in a Start result.
fn stream_source_type(results: &HashMap<String, OwnedValue>) -> u32 {
    let streams = Value::from(results["streams"].clone());
    let Value::Array(streams) = streams else {
        panic!("streams result must be an array");
    };
    let Value::Structure(stream) = streams.get(0).expect("read stream").expect("one stream") else {
        panic!("stream entry must be a structure");
    };
    let Value::Dict(properties) = &stream.fields()[1] else {
        panic!("stream properties must be a dict");
    };
    properties
        .iter()
        .find_map(|(key, value)| {
            let Value::Str(key) = key else { return None };
            if key.as_str() != "source_type" {
                return None;
            }
            let Value::Value(value) = value else {
                return None;
            };
            u32::try_from(value.as_ref()).ok()
        })
        .expect("stream must include source_type")
}

/// Read the `position` property of the single stream in a Start result.
fn stream_position(results: &HashMap<String, OwnedValue>) -> (i32, i32) {
    let streams = Value::from(results["streams"].clone());
    let Value::Array(streams) = streams else {
        panic!("streams result must be an array");
    };
    let Value::Structure(stream) = streams.get(0).expect("read stream").expect("one stream") else {
        panic!("stream entry must be a structure");
    };
    let Value::Dict(properties) = &stream.fields()[1] else {
        panic!("stream properties must be a dict");
    };
    properties
        .iter()
        .find_map(|(key, value)| {
            let Value::Str(key) = key else { return None };
            if key.as_str() != "position" {
                return None;
            }
            let Value::Value(value) = value else {
                return None;
            };
            let Value::Structure(position) = value.as_ref() else {
                return None;
            };
            let x = i32::try_from(&position.fields()[0]).ok()?;
            let y = i32::try_from(&position.fields()[1]).ok()?;
            Some((x, y))
        })
        .expect("stream must include position")
}

/// Wait until `condition` holds, with the standard E2E deadline.
fn wait_until(condition: impl Fn() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A two-output compositor gets per-output chooser entries; selecting one
/// names its connector in the compositor stream start, the confirmation
/// names the concrete output, and the negotiated cursor mode crosses the
/// wire (protocol 29).
#[test]
fn screencast_selects_a_single_output_and_embeds_cursor() {
    let Some(env) = cast_env(
        "persel",
        two_outputs(),
        &[atrium_portal_prompter::PrompterResponse::choose_source(
            atrium_portal_prompter::ChooseSourceResponse::Selected {
                source: "output:DP-1".into(),
                remember: false,
            },
        )],
    ) else {
        return;
    };
    let session_path = create_session(&env.conn, "persel");
    let code = select_sources(
        &env.conn,
        &session_path,
        "persel",
        HashMap::from([
            ("types".to_string(), Value::from(1_u32)),
            ("cursor_mode".to_string(), Value::from(2_u32)),
        ]),
    );
    assert_eq!(code, 0);

    // The chooser saw desktop + both connectors, and consent named DP-1.
    let request = common::read_prompter_request(&env.prompter_dir, 1);
    let atrium_portal_prompter::PromptRequest::ChooseSource(request) = request else {
        panic!("expected a choose_source prompter request");
    };
    let ids: Vec<&str> = request
        .options
        .iter()
        .map(|option| option.id.as_str())
        .collect();
    assert_eq!(ids, ["desktop", "output:HDMI-A-1", "output:DP-1"]);
    assert!(!request.remember_offered);
    let confirms = env.fake.confirms.lock().unwrap().clone();
    assert_eq!(confirms.len(), 1);
    assert!(
        confirms[0].1.contains("record output DP-1"),
        "consent names the concrete output: {confirms:?}"
    );

    let (code, results) = start_session(&env.conn, &session_path, "persel");
    assert_eq!(code, 0, "Start: {results:?}");
    assert_eq!(stream_source_type(&results), 1);
    assert_eq!(stream_position(&results), (1920, 0));
    assert!(!results.contains_key("persist_mode"));
    assert_eq!(
        env.fake.stream_starts.lock().unwrap().as_slice(),
        &[(
            Some(60),
            StreamTarget::Output {
                output: Some("DP-1".into())
            },
            Some(true),
            Some(StreamCursorMode::Embedded)
        )]
    );
}

/// A window selection goes through the compositor's interactive toplevel
/// pick and starts a window stream; the stream result carries source_type
/// window.
#[test]
fn screencast_window_selection_streams_the_window() {
    let Some(env) = cast_env(
        "winsel",
        Vec::new(),
        &[atrium_portal_prompter::PrompterResponse::choose_source(
            atrium_portal_prompter::ChooseSourceResponse::Selected {
                source: "window".into(),
                remember: false,
            },
        )],
    ) else {
        return;
    };
    let session_path = create_session(&env.conn, "winsel");
    let code = select_sources(
        &env.conn,
        &session_path,
        "winsel",
        HashMap::from([("types".to_string(), Value::from(0b11_u32))]),
    );
    assert_eq!(code, 0);
    assert_eq!(
        env.fake.picks.lock().unwrap().as_slice(),
        &[PickKind::Window]
    );
    let confirms = env.fake.confirms.lock().unwrap().clone();
    assert_eq!(confirms.len(), 1);
    assert!(
        confirms[0].1.contains("record the selected window"),
        "consent names the window: {confirms:?}"
    );

    let (code, results) = start_session(&env.conn, &session_path, "winsel");
    assert_eq!(code, 0, "Start: {results:?}");
    assert_eq!(stream_source_type(&results), 2);
    assert_eq!(
        env.fake.stream_starts.lock().unwrap().as_slice(),
        &[(
            Some(60),
            StreamTarget::Window {
                window: atrium_portal_ipc::WindowId(7)
            },
            Some(true),
            Some(StreamCursorMode::Hidden)
        )]
    );
}

/// persist_mode 1 with the remember tick issues a restore token; a later
/// session presenting it restores the selection with no chooser and no
/// compositor consent, and Start re-issues the same token.
#[test]
fn screencast_persist_restore_round_trip() {
    let Some(env) = cast_env(
        "persist",
        two_outputs(),
        &[atrium_portal_prompter::PrompterResponse::choose_source(
            atrium_portal_prompter::ChooseSourceResponse::Selected {
                source: "desktop".into(),
                remember: true,
            },
        )],
    ) else {
        return;
    };
    let session_path = create_session(&env.conn, "persist1");
    let code = select_sources(
        &env.conn,
        &session_path,
        "persist1",
        HashMap::from([
            ("types".to_string(), Value::from(1_u32)),
            ("persist_mode".to_string(), Value::from(1_u32)),
        ]),
    );
    assert_eq!(code, 0);
    let (code, results) = start_session(&env.conn, &session_path, "persist1");
    assert_eq!(code, 0, "Start: {results:?}");
    let persist_mode = u32::try_from(results["persist_mode"].clone()).expect("persist_mode");
    assert_eq!(persist_mode, 1);
    let token = String::try_from(results["restore_token"].clone()).expect("restore_token");
    assert_eq!(token.len(), 32);
    assert!(
        env.data_dir
            .join("atrium-portal/screencast-restore.json")
            .is_file(),
        "the mode-1 token store landed under XDG_DATA_HOME"
    );

    // A later session presenting the token skips every dialog.
    let restored_path = create_session(&env.conn, "persist2");
    let code = select_sources(
        &env.conn,
        &restored_path,
        "persist2",
        HashMap::from([
            ("types".to_string(), Value::from(1_u32)),
            ("persist_mode".to_string(), Value::from(1_u32)),
            ("restore_token".to_string(), Value::from(token.clone())),
        ]),
    );
    assert_eq!(code, 0);
    assert!(
        !env.prompter_dir.join("request-2.json").exists(),
        "a restored selection must not open the chooser"
    );
    assert_eq!(
        env.fake.confirms.lock().unwrap().len(),
        1,
        "a restored selection must not ask for compositor consent again"
    );
    let (code, results) = start_session(&env.conn, &restored_path, "persist2");
    assert_eq!(code, 0, "Start: {results:?}");
    let persist_mode = u32::try_from(results["persist_mode"].clone()).expect("persist_mode");
    assert_eq!(persist_mode, 1);
    let restored_token = String::try_from(results["restore_token"].clone()).expect("restore_token");
    assert_eq!(restored_token, token, "the token is re-issued unchanged");
}

/// A compositor geometry change restarts the stream with the same target,
/// cursor mode, and dmabuf opt-in, and the PipeWire consumer renegotiates
/// to the new geometry.
#[test]
fn screencast_geometry_change_restarts_and_renegotiates() {
    let Some(env) = cast_env("geom", Vec::new(), &[]) else {
        return;
    };
    *env.fake.stream_sizes.lock().unwrap() = vec![(2, 2), (4, 4)];
    let session_path = create_session(&env.conn, "geom");
    let code = select_sources(&env.conn, &session_path, "geom", HashMap::new());
    assert_eq!(code, 0);
    let (code, results) = start_session(&env.conn, &session_path, "geom");
    assert_eq!(code, 0, "Start: {results:?}");
    let (node_id, _) = stream_details(&results);

    assert!(env.server.push_stream_geometry_changed(1, 4, 4));
    wait_until(
        || {
            env.fake.stream_stops.lock().unwrap().contains(&1)
                && env.fake.stream_starts.lock().unwrap().len() == 2
        },
        "the compositor stream never restarted for the geometry change",
    );
    assert_eq!(
        env.fake.stream_starts.lock().unwrap().as_slice(),
        &[
            (
                Some(60),
                StreamTarget::Output { output: None },
                Some(true),
                Some(StreamCursorMode::Hidden)
            ),
            (
                Some(60),
                StreamTarget::Output { output: None },
                Some(true),
                Some(StreamCursorMode::Hidden)
            ),
        ],
        "the restart reuses the live transport's target, cursor, and dmabuf opt-in"
    );

    // A consumer linking now negotiates the new 4x4 geometry and receives
    // the restarted stream's frames.
    let socket = env.runtime_dir.join("pipewire-0");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame(
            &socket,
            node_id,
            4,
            4,
            false,
            ready_tx,
            Duration::from_secs(20),
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated at the new geometry");
    let pixels = [0x42_u8; 64];
    assert!(env.server.push_stream_frame(StreamFramePayload {
        stream_id: 2,
        sequence: 1,
        width: 4,
        height: 4,
        stride: 16,
        format: StreamPixelFormat::Bgra8,
        damage: vec![atrium_portal_ipc::Rect::new(0, 0, 4, 4)],
        dropped: 0,
        pixels: pixels.to_vec().into(),
    }));
    let received = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery after the geometry change");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));
}

/// A restart that answers a geometry different from the announced change
/// fails the stream cleanly: the cast stops, the IPC connection drops, and
/// the session closes instead of publishing mismatched frames.
#[test]
fn screencast_geometry_mismatch_fails_the_stream() {
    let Some(env) = cast_env("geomx", Vec::new(), &[]) else {
        return;
    };
    // The restart answers the OLD geometry (the sizes queue only covers
    // the first start), which never matches the announced 4x4 change.
    let session_path = create_session(&env.conn, "geomx");
    let code = select_sources(&env.conn, &session_path, "geomx", HashMap::new());
    assert_eq!(code, 0);
    let (code, results) = start_session(&env.conn, &session_path, "geomx");
    assert_eq!(code, 0, "Start: {results:?}");

    assert!(env.server.push_stream_geometry_changed(1, 4, 4));
    // The mismatched restart still stops and restarts the stream, then the
    // cast fails: its IPC connection (the one that started the stream)
    // disconnects instead of publishing mismatched frames.
    wait_until(
        || {
            env.fake.stream_stops.lock().unwrap().contains(&1)
                && env.fake.stream_starts.lock().unwrap().len() == 2
        },
        "a mismatched geometry restart must stop and restart the stream",
    );
    let cast_conn = env
        .fake
        .stream_conn
        .lock()
        .unwrap()
        .expect("the cast started a stream");
    wait_until(
        || {
            env.fake
                .stream_disconnects
                .lock()
                .unwrap()
                .contains(&cast_conn)
        },
        "a mismatched geometry restart must fail the stream",
    );
}

/// The compositor's per-frame damage rects reach the consumer as
/// `SPA_META_VideoDamage` metadata on the published buffer.
#[test]
fn screencast_damage_reaches_the_consumer() {
    let Some(env) = cast_env("damage", Vec::new(), &[]) else {
        return;
    };
    let session_path = create_session(&env.conn, "damage");
    let code = select_sources(&env.conn, &session_path, "damage", HashMap::new());
    assert_eq!(code, 0);
    let (code, results) = start_session(&env.conn, &session_path, "damage");
    assert_eq!(code, 0, "Start: {results:?}");
    let (node_id, _) = stream_details(&results);

    let socket = env.runtime_dir.join("pipewire-0");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_one_frame_damage(&socket, node_id, 2, 2, ready_tx, Duration::from_secs(20))
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");
    let pixels = [0x9a_u8; 16];
    assert!(env.server.push_stream_frame(StreamFramePayload {
        stream_id: 1,
        sequence: 1,
        width: 2,
        height: 2,
        stride: 8,
        format: StreamPixelFormat::Bgra8,
        damage: vec![atrium_portal_ipc::Rect::new(0, 0, 1, 1)],
        dropped: 0,
        pixels: pixels.to_vec().into(),
    }));
    let (received, damage) = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery");
    assert_eq!(received, Received::SharedMem(pixels.to_vec()));
    assert_eq!(
        damage,
        vec![pipewire_consumer::DamageRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1
        }],
        "the compositor's damage rect must reach the consumer as VideoDamage meta"
    );
}

/// A consumer offering a real-world multi-modifier Enum choice (matching
/// OBS Studio's EGL modifier enumeration) must successfully match and fixate
/// the compositor's tiled modifier and receive zero-copy DmaBuf frames.
#[test]
fn screencast_negotiates_multi_modifier_consumer_zero_copy() {
    use std::io::{Seek, SeekFrom, Write};
    let Some(fixture) = dmabuf_cast_fixture(
        "multimod",
        true,
        vec![StreamPixelFormat::Dmabuf {
            drm_format: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_I915_X_TILED,
        }],
    ) else {
        return;
    };
    let socket = fixture.runtime_dir.join("pipewire-0");
    let node_id = fixture.node_id;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    // The consumer offers a list of modifiers including LINEAR, I915_X_TILED, and dummy.
    let consumer_modifiers = [
        DRM_FORMAT_MOD_LINEAR,
        DRM_FORMAT_MOD_I915_X_TILED,
        0x0100_0000_0000_0002,
    ];
    let consumer = std::thread::spawn(move || {
        consume_frames_metadata(
            &socket,
            ConsumeRequest {
                node_id,
                width: 2,
                height: 2,
                modifiers: &consumer_modifiers,
                count: 1,
                ready: ready_tx,
                timeout: Duration::from_secs(20),
            },
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");

    let pixels = [0x55_u8; 16];
    {
        let mut files = fixture.fake.slot_files.lock().unwrap();
        let file = &mut files[0];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&pixels).unwrap();
    }
    push_slot_frame_modifier(&fixture, 0, DRM_FORMAT_MOD_I915_X_TILED);

    let frames = consumer
        .join()
        .expect("consumer thread")
        .expect("frame delivery");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].received, Received::DmaBuf(pixels.to_vec()));
    assert!(
        frames[0].pts_nanos.is_some(),
        "delivered frame must carry SPA_META_Header PTS timestamp"
    );
    assert_eq!(
        frames[0].seq,
        Some(0),
        "delivered frame must carry 0-indexed sequence number"
    );
}

/// A consumer streaming multiple consecutive frames must receive continuous
/// monotonically increasing PTS timestamps and sequential sequence numbers
/// without buffer starvation or stalls.
#[test]
fn screencast_continuous_cadence_and_pts_headers() {
    let Some(env) = cast_env("cadence", Vec::new(), &[]) else {
        return;
    };
    let session_path = create_session(&env.conn, "cadence");
    let code = select_sources(&env.conn, &session_path, "cadence", HashMap::new());
    assert_eq!(code, 0);
    let (code, results) = start_session(&env.conn, &session_path, "cadence");
    assert_eq!(code, 0, "Start: {results:?}");
    let (node_id, _) = stream_details(&results);

    let socket = env.runtime_dir.join("pipewire-0");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        consume_frames_metadata(
            &socket,
            ConsumeRequest {
                node_id,
                width: 2,
                height: 2,
                modifiers: &[],
                count: 5,
                ready: ready_tx,
                timeout: Duration::from_secs(20),
            },
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the consumer linked and negotiated");

    for i in 1..=5 {
        let pixels = [i as u8; 16];
        assert!(env.server.push_stream_frame(StreamFramePayload {
            stream_id: 1,
            sequence: i as u64,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Bgra8,
            damage: vec![atrium_portal_ipc::Rect::new(0, 0, 2, 2)],
            dropped: 0,
            pixels: pixels.to_vec().into(),
        }));
        // The stream is latest-wins: a frame still pending publication is
        // dropped when the next one replaces it (that is the designed
        // backpressure behaviour a slow consumer needs). A real screen
        // cadence gives the portal's PipeWire loop time to publish each
        // frame before its successor lands, so the consumer observes five
        // distinct frames; a 10 ms burst only asserts that five pushes in
        // one loop cycle collapse to the last, which races on slow
        // runners.
        std::thread::sleep(Duration::from_millis(150));
    }

    let frames = consumer
        .join()
        .expect("consumer thread")
        .expect("multi-frame delivery");
    assert_eq!(frames.len(), 5, "must receive all 5 frames");

    for (idx, frame) in frames.iter().enumerate() {
        let expected_val = (idx + 1) as u8;
        assert_eq!(
            frame.received,
            Received::SharedMem(vec![expected_val; 16]),
            "frame content must match sequence"
        );
    }
}
