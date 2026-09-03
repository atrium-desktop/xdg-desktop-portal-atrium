//! Stream-frame transport tests against the independent test server. The
//! server speaks the literal v24 wire shape; a plain memfd stands in for a
//! GPU dmabuf so the descriptor paths run on machines without a render node.
#![cfg(feature = "test-server")]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::time::Duration;

use atrium_portal_ipc::testing::{Handler, Server, StreamFrameFdPayload, StreamInfo};
use atrium_portal_ipc::{
    Client, ConnectionCapabilities, StreamCursorMode, StreamMessage, StreamPixelFormat,
    StreamTarget,
};

const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const MOD_LINEAR: u64 = 0;

struct DmabufStream;

impl Handler for DmabufStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        _dmabuf: Option<bool>,
        _cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            slots: None,
        })
    }
}

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "tessera-ipc-{name}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// An unsealed memfd mirrors what the compositor sends for a dmabuf frame:
/// a fixed-size descriptor without memfd seals.
fn unsealed_memfd(bytes: &[u8]) -> std::fs::File {
    // SAFETY: the name is static and NUL-terminated; the fd is checked
    // before ownership is constructed.
    let fd = unsafe { libc::memfd_create(c"tessera-ipc-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    // SAFETY: `fd` is a new owned descriptor from memfd_create.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).unwrap();
    file
}

#[test]
fn dmabuf_stream_frames_cross_the_wire_as_descriptors() {
    let server = Server::start(&socket_path("dmabuf-stream"), Arc::new(DmabufStream))
        .expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let started = client
        .start_output_stream_target(None, StreamTarget::Output { output: None })
        .expect("stream start");
    assert_eq!(started.stream_id, 7);
    assert_eq!(
        started.format,
        StreamPixelFormat::Dmabuf {
            drm_format: DRM_FORMAT_XRGB8888,
            modifier: MOD_LINEAR
        }
    );

    let pixels = [0x5a_u8; 16];
    let frame_fd = unsealed_memfd(&pixels);
    assert!(server.push_stream_frame_fd(
        StreamFrameFdPayload {
            stream_id: 7,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            damage: vec![],
            dropped: 0,
            byte_len: pixels.len() as u64,
        },
        frame_fd.as_raw_fd(),
    ));

    let message = client.next_stream_message().expect("frame message");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected a frame, got {message:?}");
    };
    assert_eq!(frame.sequence, 1);
    assert_eq!(frame.stride, 8);
    let atrium_portal_ipc::StreamPayload::Dmabuf(mut file) = frame.payload else {
        panic!("dmabuf frames must carry a dmabuf payload");
    };
    let mut received = Vec::new();
    file.read_to_end(&mut received).unwrap();
    assert_eq!(received, pixels);
}

struct SlotStream {
    releases: std::sync::Mutex<Vec<(u64, u32)>>,
    slot_files: std::sync::Mutex<Vec<std::fs::File>>,
}

impl Handler for SlotStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        dmabuf: Option<bool>,
        _cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        if dmabuf != Some(true) {
            return Err("expected the dmabuf opt-in".into());
        }
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
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            slots: Some(infos),
        })
    }

    fn stream_buffer_release(&self, stream_id: u64, slot: u32) {
        self.releases.lock().unwrap().push((stream_id, slot));
    }
}

#[test]
fn slot_streams_transfer_the_table_frames_and_releases() {
    let handler = Arc::new(SlotStream {
        releases: std::sync::Mutex::new(Vec::new()),
        slot_files: std::sync::Mutex::new(Vec::new()),
    });
    let server =
        Server::start(&socket_path("slot-stream"), handler.clone()).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert_eq!(
        client.protocol_version(),
        atrium_portal_ipc::PROTOCOL_VERSION
    );

    let started = client
        .start_output_stream(None, StreamTarget::Output { output: None }, true, None)
        .expect("stream start");
    let slots = started.slots.expect("a slot table");
    assert_eq!(slots.len(), 3);
    assert_eq!((slots[0].stride, slots[0].byte_len), (8, 16));

    assert!(server.push_stream_frame_slot(
        atrium_portal_ipc::testing::StreamFrameFdPayload {
            stream_id: 7,
            sequence: 9,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            damage: vec![],
            dropped: 0,
            byte_len: 16,
        },
        2,
    ));
    let message = client.next_stream_message().expect("frame message");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected a frame, got {message:?}");
    };
    assert_eq!(frame.slot, Some(2));
    assert!(matches!(
        frame.payload,
        atrium_portal_ipc::StreamPayload::Slot
    ));

    client.release_stream_buffer(7, 2).expect("release write");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handler.releases.lock().unwrap().contains(&(7, 2)) {
        assert!(
            std::time::Instant::now() < deadline,
            "release never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn handshake_negotiates_down_to_a_legacy_server() {
    let server = Server::start_legacy(
        &socket_path("legacy"),
        Arc::new(SlotStream {
            releases: std::sync::Mutex::new(Vec::new()),
            slot_files: std::sync::Mutex::new(Vec::new()),
        }),
        atrium_portal_ipc::MIN_PROTOCOL_VERSION,
    )
    .expect("bind legacy test server");
    let client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake with downgrade");
    assert_eq!(
        client.protocol_version(),
        atrium_portal_ipc::MIN_PROTOCOL_VERSION
    );
}

/// The compositor publishes the stream lane before it writes the
/// `StreamOutputStarted` reply, so an already-produced frame can reach the
/// client first. The client must buffer it and still complete the start.
struct RacyStream;

impl Handler for RacyStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        _dmabuf: Option<bool>,
        _cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Bgra8,
            slots: None,
        })
    }

    fn frames_before_started(
        &self,
        stream_id: u64,
    ) -> Vec<atrium_portal_ipc::testing::StreamFramePayload> {
        vec![atrium_portal_ipc::testing::StreamFramePayload {
            stream_id,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Bgra8,
            damage: vec![],
            dropped: 0,
            pixels: Arc::from(&[0x5a_u8; 16][..]),
        }]
    }
}

#[test]
fn a_frame_racing_ahead_of_started_is_buffered() {
    let server =
        Server::start(&socket_path("racy-start"), Arc::new(RacyStream)).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let started = client
        .start_output_stream_target(None, StreamTarget::Output { output: None })
        .expect("stream start despite the early frame");
    assert_eq!(started.stream_id, 7);

    let message = client.next_stream_message().expect("buffered frame");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected the early frame, got {message:?}");
    };
    assert_eq!(frame.sequence, 1);
    let atrium_portal_ipc::StreamPayload::Memfd(mut file) = frame.payload else {
        panic!("SHM frames must carry a memfd payload");
    };
    let mut received = Vec::new();
    file.read_to_end(&mut received).unwrap();
    assert_eq!(received, [0x5a_u8; 16]);
}

/// Same race on a protocol-25 dmabuf slot stream: the early frame is
/// buffered and the slot table that follows the reply is still received.
struct RacySlotStream {
    slot_files: std::sync::Mutex<Vec<std::fs::File>>,
}

impl Handler for RacySlotStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        dmabuf: Option<bool>,
        _cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        if dmabuf != Some(true) {
            return Err("expected the dmabuf opt-in".into());
        }
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
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            slots: Some(infos),
        })
    }

    fn frames_before_started(
        &self,
        stream_id: u64,
    ) -> Vec<atrium_portal_ipc::testing::StreamFramePayload> {
        vec![atrium_portal_ipc::testing::StreamFramePayload {
            stream_id,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Bgra8,
            damage: vec![],
            dropped: 0,
            pixels: Arc::from(&[0x5a_u8; 16][..]),
        }]
    }
}

#[test]
fn a_frame_racing_ahead_of_a_slot_stream_start_is_buffered() {
    let handler = Arc::new(RacySlotStream {
        slot_files: std::sync::Mutex::new(Vec::new()),
    });
    let server = Server::start(&socket_path("racy-slot-start"), handler).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let started = client
        .start_output_stream(None, StreamTarget::Output { output: None }, true, None)
        .expect("stream start despite the early frame");
    let slots = started.slots.expect("a slot table");
    assert_eq!(slots.len(), 3);

    let message = client.next_stream_message().expect("buffered frame");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected the early frame, got {message:?}");
    };
    assert_eq!(frame.sequence, 1);
}

/// Protocol-29 output enumeration and output-addressed streaming.
struct Outputs {
    starts: std::sync::Mutex<Vec<(StreamTarget, Option<StreamCursorMode>)>>,
}

impl Handler for Outputs {
    fn enumerate_outputs(&self) -> Result<Vec<atrium_portal_ipc::OutputInfo>, String> {
        Ok(vec![
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
        ])
    }

    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        target: StreamTarget,
        _dmabuf: Option<bool>,
        cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        self.starts.lock().unwrap().push((target, cursor));
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Bgra8,
            slots: None,
        })
    }
}

#[test]
fn enumerate_outputs_round_trips_the_output_set() {
    let handler = Arc::new(Outputs {
        starts: std::sync::Mutex::new(Vec::new()),
    });
    let server = Server::start(&socket_path("outputs"), handler).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let outputs = client.enumerate_outputs().expect("enumerate outputs");
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].connector, "HDMI-A-1");
    assert!(outputs[0].primary);
    assert_eq!(
        outputs[0].rect,
        atrium_portal_ipc::Rect::new(0, 0, 1920, 1080)
    );
    assert_eq!(outputs[1].connector, "DP-1");
    assert!(!outputs[1].primary);
}

#[test]
fn v29_stream_start_carries_the_connector_and_cursor_mode() {
    let handler = Arc::new(Outputs {
        starts: std::sync::Mutex::new(Vec::new()),
    });
    let server =
        Server::start(&socket_path("v29-start"), Arc::clone(&handler)).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert_eq!(
        client.protocol_version(),
        atrium_portal_ipc::PROTOCOL_VERSION
    );

    let started = client
        .start_output_stream(
            Some(60),
            StreamTarget::Output {
                output: Some("HDMI-A-1".into()),
            },
            false,
            Some(StreamCursorMode::Embedded),
        )
        .expect("stream start");
    assert_eq!(started.stream_id, 7);
    assert_eq!(
        handler.starts.lock().unwrap().as_slice(),
        &[(
            StreamTarget::Output {
                output: Some("HDMI-A-1".into())
            },
            Some(StreamCursorMode::Embedded)
        )]
    );

    // The geometry-changed event surfaces on the stream lane; after it the
    // compositor sends no further frames until the stream is restarted.
    assert!(server.push_stream_geometry_changed(7, 2560, 1440));
    let message = client.next_stream_message().expect("geometry message");
    let StreamMessage::GeometryChanged {
        stream_id,
        width,
        height,
    } = message
    else {
        panic!("expected a geometry change, got {message:?}");
    };
    assert_eq!((stream_id, width, height), (7, 2560, 1440));
}

/// Against a pre-29 peer the cursor mode is silently dropped (the peer only
/// streams the hidden-cursor default anyway), but a connector-named target
/// fails closed: an older compositor could only stream the whole desktop,
/// which captures more than the caller asked for.
#[test]
fn v29_start_parameters_degrade_against_a_legacy_server() {
    let handler = Arc::new(Outputs {
        starts: std::sync::Mutex::new(Vec::new()),
    });
    let server = Server::start_legacy(
        &socket_path("v29-legacy"),
        Arc::clone(&handler),
        atrium_portal_ipc::MIN_PROTOCOL_VERSION,
    )
    .expect("bind legacy test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake with downgrade");
    assert_eq!(
        client.protocol_version(),
        atrium_portal_ipc::MIN_PROTOCOL_VERSION
    );

    client
        .start_output_stream(
            Some(60),
            StreamTarget::Output { output: None },
            false,
            Some(StreamCursorMode::Embedded),
        )
        .expect("stream start");
    assert_eq!(
        handler.starts.lock().unwrap().as_slice(),
        &[(StreamTarget::Output { output: None }, None)]
    );

    let addressed = client.start_output_stream(
        None,
        StreamTarget::Output {
            output: Some("HDMI-A-1".into()),
        },
        false,
        None,
    );
    assert!(
        addressed.is_err(),
        "a connector target must fail closed against a pre-29 peer"
    );
}
