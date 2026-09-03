//! SetWallpaper round-trip and authorization-gate tests against the
//! independent test server: the op names a staged image path, and the
//! server mirrors the real dispatch's gates (control, live lease, explicit
//! scope op, valid path, active session).
#![cfg(feature = "test-server")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atrium_portal_ipc::testing::{Handler, Server};
use atrium_portal_ipc::{Client, ConnectionCapabilities, LOCAL_PORTAL_SCOPE};

const STAGED: &str = "/run/user/1000/atrium-portal/wallpaper/current.png";

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

const CONTROL: ConnectionCapabilities = ConnectionCapabilities {
    query: true,
    control: true,
    input: false,
    session: false,
    interaction_domain: false,
};

/// Records every wallpaper path the server applies.
#[derive(Default)]
struct Recording {
    applied: Mutex<Vec<PathBuf>>,
}

impl Handler for Recording {
    fn set_wallpaper(&self, _connection: u64, path: PathBuf) -> Result<(), String> {
        self.applied.lock().unwrap().push(path);
        Ok(())
    }
}

fn scoped_client(server: &Server) -> Client {
    Client::connect_scoped_with_timeout(
        server.path(),
        CONTROL,
        LOCAL_PORTAL_SCOPE,
        Duration::from_secs(5),
    )
    .expect("handshake")
}

#[test]
fn wallpaper_path_round_trips_at_every_negotiated_version() {
    // The op predates the projection's version floor: a current server (25)
    // and a legacy 24 server both speak it.
    for (name, legacy) in [("wallpaper-v25", None), ("wallpaper-v24", Some(24))] {
        let handler = Arc::new(Recording::default());
        let server = match legacy {
            Some(version) => Server::start_legacy(&socket_path(name), handler.clone(), version),
            None => Server::start(&socket_path(name), handler.clone()),
        }
        .expect("bind test server");
        let mut client = scoped_client(&server);

        client
            .set_wallpaper(Path::new(STAGED))
            .expect("set wallpaper");

        let applied = handler.applied.lock().unwrap();
        assert_eq!(applied.as_slice(), &[PathBuf::from(STAGED)]);
    }
}

#[test]
fn wallpaper_refusal_surfaces_as_an_error() {
    struct Refusing;
    impl Handler for Refusing {
        fn set_wallpaper(&self, _connection: u64, _path: PathBuf) -> Result<(), String> {
            Err("no outputs configured".into())
        }
    }
    let server = Server::start(&socket_path("wallpaper-refuse"), Arc::new(Refusing))
        .expect("bind test server");
    let mut client = scoped_client(&server);

    let error = client
        .set_wallpaper(Path::new(STAGED))
        .expect_err("the refusal must surface");
    assert_eq!(error.to_string(), "no outputs configured");
}

#[test]
fn wallpaper_requires_the_control_capability() {
    let server = Server::start(
        &socket_path("wallpaper-no-control"),
        Arc::new(Recording::default()),
    )
    .expect("bind test server");
    // Scoped like the portal but without control: refused at the first gate.
    let mut client = Client::connect_scoped_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        LOCAL_PORTAL_SCOPE,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let error = client
        .set_wallpaper(Path::new(STAGED))
        .expect_err("query-only connections cannot set the wallpaper");
    assert!(error.to_string().contains("control capability"), "{error}");
}

#[test]
fn wallpaper_requires_a_live_lease() {
    struct Leaseless;
    impl Handler for Leaseless {
        fn grant_lease(&self) -> bool {
            false
        }
    }
    let server = Server::start(&socket_path("wallpaper-no-lease"), Arc::new(Leaseless))
        .expect("bind test server");
    let mut client = scoped_client(&server);
    assert!(client.lease().is_none());

    let error = client
        .set_wallpaper(Path::new(STAGED))
        .expect_err("a leaseless connection cannot set the wallpaper");
    assert!(error.to_string().contains("lease expired"), "{error}");
}

#[test]
fn wallpaper_requires_an_explicit_scope_op() {
    struct OtherScope;
    impl Handler for OtherScope {
        fn known_scopes(&self) -> &'static [&'static str] {
            &["focus-first"]
        }
    }
    let server = Server::start(&socket_path("wallpaper-scope"), Arc::new(OtherScope))
        .expect("bind test server");

    // A recognized scope without the op is refused even though it has
    // control and a lease.
    let mut other = Client::connect_scoped_with_timeout(
        server.path(),
        CONTROL,
        "focus-first",
        Duration::from_secs(5),
    )
    .expect("handshake");
    let error = other
        .set_wallpaper(Path::new(STAGED))
        .expect_err("a scope without the op must be refused");
    assert!(error.to_string().contains("out of scope"), "{error}");

    // Unscoped connections never inherit the op (fail-closed).
    let mut unscoped = Client::connect_with_timeout(server.path(), CONTROL, Duration::from_secs(5))
        .expect("handshake");
    let error = unscoped
        .set_wallpaper(Path::new(STAGED))
        .expect_err("an unscoped connection must be refused");
    assert!(error.to_string().contains("out of scope"), "{error}");
}

#[test]
fn wallpaper_rejects_a_locked_session() {
    struct Locked;
    impl Handler for Locked {
        fn session_active(&self) -> bool {
            false
        }
    }
    let server = Server::start(&socket_path("wallpaper-locked"), Arc::new(Locked))
        .expect("bind test server");
    let mut client = scoped_client(&server);

    let error = client
        .set_wallpaper(Path::new(STAGED))
        .expect_err("a locked session must refuse the swap");
    assert!(error.to_string().contains("locked"), "{error}");
}

#[test]
fn wallpaper_paths_are_checked_client_side() {
    let handler = Arc::new(Recording::default());
    let server =
        Server::start(&socket_path("wallpaper-paths"), handler.clone()).expect("bind test server");
    let mut client = scoped_client(&server);

    for bad in [
        PathBuf::from("relative.png"),
        PathBuf::from("/run/../etc/wall.png"),
        PathBuf::from("/run/./wall.png"),
        PathBuf::from(format!("/{}", "a".repeat(4_096))),
    ] {
        let error = client
            .set_wallpaper(&bad)
            .expect_err("invalid wallpaper path must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
    // Nothing crossed the socket: the server applied nothing.
    assert!(handler.applied.lock().unwrap().is_empty());
}
