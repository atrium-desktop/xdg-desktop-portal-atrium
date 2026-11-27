//! End-to-end smoke test for the native Secret portal. The test starts the
//! real backend on a private session bus, verifies that the incomplete
//! Secret Service compatibility API is not exposed, and exercises fd-based
//! secret delivery through a Portal-owned fake sigil daemon.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use zbus::{Fd, ObjectPath, OwnedObjectPath, Variant};

mod common;
use common::bus;
use common::{
    FakeSigil, FakeSigilResponse, KillOnDrop, die_with_parent, e2e_required, frontend_binary,
    frontend_prerequisites, pipe_pair, private_bus, read_all_with_timeout, spawn_daemon, temp_dir,
    wait_for_frontend, wait_for_name,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

#[test]
fn native_secret_portal_end_to_end() {
    let Some(bus) = private_bus() else {
        eprintln!("native_secret_portal_end_to_end: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("data");
    let runtime_dir = temp_dir("runtime");
    let served: Vec<u8> = (0..32u8).collect();
    let sigil = FakeSigil::bind(&runtime_dir, FakeSigilResponse::Secret(served.clone()));
    assert!(sigil.bind_error().is_none(), "fake sigil must bind");
    let _daemon = KillOnDrop(spawn_daemon(&bus, &data_dir, &runtime_dir));
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    let fdo = bus::proxy(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    );
    let compat_owned: bool = bus::call(&fdo, "NameHasOwner", &("org.freedesktop.secrets",));
    assert!(
        !compat_owned,
        "the backend must not claim an incomplete Secret Service API"
    );

    let compat_service_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contrib/dbus-1/services/org.freedesktop.secrets.service");
    assert!(
        !compat_service_file.exists(),
        "packaging must not activate the removed compatibility API"
    );

    let portal = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    );
    let version: u32 = bus::run(portal.get_property("version")).expect("version property");
    assert_eq!(version, 1);

    for (interface, expected) in [
        ("org.freedesktop.impl.portal.Settings", 1),
        ("org.freedesktop.impl.portal.Screenshot", 3),
        ("org.freedesktop.impl.portal.ScreenCast", 6),
    ] {
        let proxy = bus::proxy(&conn, PORTAL, DESKTOP_PATH, interface);
        let version: u32 = bus::run(proxy.get_property("version"))
            .unwrap_or_else(|error| panic!("{interface} must expose version: {error}"));
        assert_eq!(version, expected, "{interface} version");
    }

    let lockdown = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Lockdown",
    );
    for property in [
        "disable-printing",
        "disable-save-to-disk",
        "disable-application-handlers",
        "disable-location",
        "disable-camera",
        "disable-microphone",
        "disable-sound-output",
    ] {
        let restricted: bool = bus::run(lockdown.get_property(property))
            .unwrap_or_else(|error| panic!("Lockdown must expose {property}: {error}"));
        assert!(!restricted, "Lockdown {property} must be permissive");
    }
    bus::run(lockdown.set_property("disable-printing", true))
        .expect("Lockdown property must be writable");
    // Read through a fresh proxy so this checks the service rather than the
    // first proxy's property cache racing the PropertiesChanged signal.
    let lockdown_after_set = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Lockdown",
    );
    let printing_disabled: bool = bus::run(lockdown_after_set.get_property("disable-printing"))
        .expect("updated Lockdown property");
    assert!(printing_disabled);

    let introspectable = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.DBus.Introspectable",
    );
    let xml: String = bus::call(&introspectable, "Introspect", &());
    for interface in ["Account", "Email", "FileChooser"] {
        let section = interface_section(&xml, interface);
        assert!(
            !section.contains("property name=\"version\""),
            "{interface} backend ABI does not define a version property"
        );
    }
    let lockdown_xml = interface_section(&xml, "Lockdown");
    assert_eq!(
        lockdown_xml.matches("access=\"readwrite\"").count(),
        7,
        "all Lockdown properties must be read-write"
    );

    let (read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/smoke")
        .expect("request handle path");
    let (response, _results): (u32, HashMap<String, zbus::zvariant::OwnedVariant>) = bus::call(
        &portal,
        "RetrieveSecret",
        &(
            handle,
            "dev.tessera.smoke",
            fd,
            HashMap::<String, Variant<'_>>::new(),
        ),
    );
    assert_eq!(response, 0, "RetrieveSecret must report success");

    let delivered = read_all_with_timeout(read_end, Duration::from_secs(5));
    assert_eq!(
        delivered.as_slice(),
        served.as_slice(),
        "the pipe must deliver the sigil-served application secret"
    );
    let observed = sigil.observed();
    assert_eq!(
        observed.len(),
        1,
        "exactly one GetApplicationSecret request"
    );
    let request = &observed[0];
    assert_eq!(request.namespace, "atrium.portal.Secret/v1");
    assert_eq!(request.subject, "dev.tessera.smoke");
    assert_eq!(request.purpose, "master-secret");

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

fn interface_section<'a>(xml: &'a str, short_name: &str) -> &'a str {
    let marker = format!("<interface name=\"org.freedesktop.impl.portal.{short_name}\">");
    let start = xml
        .find(&marker)
        .unwrap_or_else(|| panic!("missing {short_name} introspection"));
    let rest = &xml[start..];
    let end = rest
        .find("</interface>")
        .expect("interface has a closing element")
        + "</interface>".len();
    &rest[..end]
}

#[test]
fn public_frontend_delivers_the_native_secret() {
    let Some(bus) = private_bus() else {
        eprintln!("public Secret test: dbus-daemon unavailable, skipping");
        return;
    };
    if let Err(reason) = frontend_prerequisites(&bus) {
        if e2e_required() {
            panic!("required public-Secret prerequisite failed: {reason}");
        }
        eprintln!("public Secret test: {reason}, skipping");
        return;
    }
    let frontend = frontend_binary().expect("checked by frontend_prerequisites");
    let conn = bus.connect();

    let root = temp_dir("secret-frontend");
    let backend_data = root.join("backend-data");
    let frontend_data = root.join("frontend-data");
    let portal_data = root.join("portal-data");
    let config_home = root.join("config");
    let runtime_dir = root.join("runtime");
    let portal_dir = portal_data.join("xdg-desktop-portal/portals");
    let config_dir = config_home.join("xdg-desktop-portal");
    for directory in [
        &backend_data,
        &frontend_data,
        &runtime_dir,
        &portal_dir,
        &config_dir,
    ] {
        std::fs::create_dir_all(directory).expect("create public Secret fixture");
    }
    let served: Vec<u8> = (0x41..0x51).collect();
    let sigil = FakeSigil::bind(&runtime_dir, FakeSigilResponse::Secret(served.clone()));
    assert!(sigil.bind_error().is_none(), "fake sigil must bind");
    std::fs::write(
        portal_dir.join("atrium.portal"),
        include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal"),
    )
    .expect("stage portal metadata");
    std::fs::write(
        // 1.18 only consults the desktop-specific filename when
        // XDG_CURRENT_DESKTOP is non-empty. Mirror the installed package.
        config_dir.join("tessera-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.Secret=atrium\n",
    )
    .expect("stage portal routing");
    std::fs::write(
        config_dir.join("atrium-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.Secret=atrium\n",
    )
    .expect("stage portal routing compatibility");

    let backend_log = root.join("backend.log");
    let mut backend = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-atrium"));
    backend
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", &backend_data)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("RUST_LOG", "debug")
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&backend_log).expect("backend log"),
        ));
    let _backend = KillOnDrop(backend.spawn().expect("spawn Tessera backend"));
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    let frontend_log = root.join("frontend.log");
    let mut frontend_command = Command::new(frontend);
    frontend_command
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_CURRENT_DESKTOP", "tessera")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &frontend_data)
        // xdg-desktop-portal 1.18 discovers test backends through this
        // dedicated override rather than XDG_DATA_DIRS.
        .env("XDG_DESKTOP_PORTAL_DIR", &portal_dir)
        .env(
            "XDG_DATA_DIRS",
            format!("{}:/usr/local/share:/usr/share", portal_data.display()),
        )
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&frontend_log).expect("frontend log"),
        ));
    die_with_parent(&mut frontend_command);
    let _frontend = KillOnDrop(
        frontend_command
            .spawn()
            .expect("spawn xdg-desktop-portal frontend"),
    );
    if !wait_for_frontend(&conn) {
        if e2e_required() {
            panic!("the xdg-desktop-portal frontend never claimed its bus name");
        }
        eprintln!("public frontend test: frontend never claimed its name, skipping");
        return;
    }

    let secret = bus::proxy(
        &conn,
        "org.freedesktop.portal.Desktop",
        DESKTOP_PATH,
        "org.freedesktop.portal.Secret",
    );
    let token = "atrium_secret_frontend_1";
    let sender = conn
        .unique_name()
        .expect("private bus connection has a unique name")
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let expected_handle = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = bus::proxy(
        &conn,
        "org.freedesktop.portal.Desktop",
        expected_handle.as_str(),
        "org.freedesktop.portal.Request",
    );
    let mut responses = bus::run(request.receive_signal("Response")).expect("subscribe Response");

    let (read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let options = HashMap::from([("handle_token".to_owned(), Variant::from(token))]);
    let returned: OwnedObjectPath = bus::try_call(&secret, "RetrieveSecret", &(fd, options))
        .unwrap_or_else(|error| {
            let log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
            panic!("public RetrieveSecret failed: {error}\nfrontend log:\n{log}")
        });
    assert_eq!(returned.as_str(), expected_handle);
    let response = bus::next_signal(&mut responses);
    let (code, _results): (u32, HashMap<String, zbus::zvariant::OwnedVariant>) =
        response.body().deserialize().expect("Response body");
    if code != 0 {
        let backend_log = std::fs::read_to_string(&backend_log).unwrap_or_default();
        let frontend_log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
        panic!(
            "public RetrieveSecret returned {code}\nbackend log:\n{backend_log}\nfrontend log:\n{frontend_log}"
        );
    }
    let delivered = read_all_with_timeout(read_end, Duration::from_secs(5));
    // Host callers have an empty app id. The sigil daemon derives distinct
    // per-subject keys; this fixture proves the delivered bytes are exactly
    // what the daemon served.
    assert_eq!(delivered.as_slice(), served.as_slice());

    std::fs::remove_dir_all(root).ok();
}

/// A locked vault is unbounded in wall-clock time, so the sigil round trip
/// must never run on the connection's runtime.
///
/// The failure this guards is *daemon-wide*: zbus 6 dispatches interface
/// methods as tasks on the connection's shared runtime, so an interface that
/// waits for sigil inside its served method takes every other portal
/// interface down with it. A wedged sigil (accepts the request, never answers)
/// is therefore the sharpest probe: the Secret call itself is expected to be
/// outstanding, but unrelated interfaces must keep answering.
#[test]
fn a_wedged_sigil_does_not_stall_other_interfaces() {
    let Some(bus) = private_bus() else {
        eprintln!("a_wedged_sigil_does_not_stall_other_interfaces: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("stall-data");
    let runtime_dir = temp_dir("stall-runtime");
    let sigil = FakeSigil::bind(&runtime_dir, FakeSigilResponse::Stall);
    assert!(sigil.bind_error().is_none(), "fake sigil must bind");
    let _daemon = KillOnDrop(spawn_daemon(&bus, &data_dir, &runtime_dir));
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    // Leave a RetrieveSecret outstanding against the stalled sigil.
    let secret = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    );
    let (read_end, write_end) = pipe_pair();
    std::thread::spawn(move || {
        let fd: Fd<'_> = Fd::from(write_end);
        let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/stall/secret")
            .expect("request handle path");
        let _outcome: Result<(u32, HashMap<String, zbus::zvariant::OwnedVariant>), _> =
            bus::try_call(
                &secret,
                "RetrieveSecret",
                &(
                    handle,
                    "dev.tessera.stall",
                    fd,
                    HashMap::<String, Variant<'_>>::new(),
                ),
            );
    });

    // Wait until the request has actually reached the stalled sigil. A fixed
    // sleep would let the test pass while the request was still in flight
    // before the backend, proving nothing about the interface.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sigil.observed().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the RetrieveSecret never reached sigil; the stall was not exercised"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The probes: interfaces that owe nothing to sigil must still answer.
    //
    // Each probe asserts a *real* reply, not merely that the call returned:
    // an unrelated D-Bus error (wrong method name, unknown interface) would
    // arrive promptly even from a wedged daemon and prove nothing.
    // `Lockdown` exposes kebab-case read-write *properties* (the backend ABI
    // spelling), not methods; `Settings.Read` answers the colour scheme.
    let lockdown = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Lockdown",
    );
    let disabled: bool = probe("Lockdown.disable-printing", move || {
        bus::run(lockdown.get_property("disable-printing"))
    });
    assert!(!disabled, "Lockdown must start without printing disabled");

    let settings = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Settings",
    );
    // The backend's `Read` returns a variant even though the colour scheme
    // itself is a `u32`; decoding the variant proves a real value came back.
    let scheme: zbus::zvariant::OwnedVariant = probe("Settings.Read", move || {
        bus::try_call(
            &settings,
            "Read",
            &("org.freedesktop.appearance", "color-scheme"),
        )
    });
    let scheme = u32::try_from(&scheme).expect("colour scheme must decode as u32");
    assert!(
        scheme <= 1,
        "colour scheme must be a known value, got {scheme}"
    );

    // The stalled request is still outstanding: sigil was asked and never
    // answered, and the caller has received nothing. This is the premise the
    // probes above depend on, so assert it rather than assume it.
    assert_eq!(
        sigil.observed().len(),
        1,
        "exactly one GetApplicationSecret request should be stalled"
    );
    // Nothing was written to the caller's pipe, because sigil never answered.
    drop(read_end);
}

/// Run `call` on a detached thread and require a genuine reply within 5 s.
///
/// A wedged daemon produces no reply at all, which is what the deadline
/// catches; a *wrong* call produces an error promptly, which the caller's own
/// type assertion then rejects.
fn probe<T, F>(label: &str, call: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, zbus::Error> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(call());
    });
    let reply = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|error| {
        panic!(
            "{label} did not answer within 5s while a RetrieveSecret was outstanding ({error}); \
             the secret work is blocking the connection runtime"
        )
    });
    reply.unwrap_or_else(|error| panic!("{label} failed: {error}"))
}

#[test]
fn sigil_socket_path_override_and_desynced_status_are_honored() {
    let Some(bus) = private_bus() else {
        eprintln!(
            "sigil_socket_path_override_and_desynced_status_are_honored: no dbus-daemon, skipping"
        );
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("sigil-override-data");
    let runtime_dir = temp_dir("sigil-override-runtime");
    let custom_dir = temp_dir("sigil-custom");
    let custom_sock = custom_dir.join("custom-native.sock");

    let sigil = FakeSigil::bind_at(&custom_sock, FakeSigilResponse::Desynced);
    assert!(sigil.bind_error().is_none(), "fake sigil must bind");

    let mut cmd = common::daemon_command(&bus, &data_dir, &runtime_dir);
    cmd.env("SIGIL_SOCKET_PATH", &custom_sock);
    let _daemon = KillOnDrop(cmd.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);
    common::claim_frontend(&conn);

    let portal = bus::proxy(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    );
    let (_read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/desynced")
        .expect("request handle path");
    let (response, _results): (u32, HashMap<String, zbus::zvariant::OwnedVariant>) = bus::call(
        &portal,
        "RetrieveSecret",
        &(
            handle,
            "dev.tessera.desynced",
            fd,
            HashMap::<String, Variant<'_>>::new(),
        ),
    );
    assert_eq!(
        response, 2,
        "a desynchronized vault must surface as portal error code 2"
    );
    assert_eq!(
        sigil.observed().len(),
        1,
        "portal must connect to SIGIL_SOCKET_PATH rather than XDG_RUNTIME_DIR"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&custom_dir);
}

#[test]
fn request_close_aborts_in_flight_sigil_wait_promptly() {
    let Some(bus) = private_bus() else {
        eprintln!("request_close_aborts_in_flight_sigil_wait_promptly: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("sigil-cancel-data");
    let runtime_dir = temp_dir("sigil-cancel-runtime");
    let sigil = FakeSigil::bind(&runtime_dir, FakeSigilResponse::Stall);
    assert!(sigil.bind_error().is_none(), "fake sigil must bind");
    let _daemon = KillOnDrop(spawn_daemon(&bus, &data_dir, &runtime_dir));
    wait_for_name(&conn, PORTAL);
    common::claim_frontend(&conn);

    let handle_path = "/org/freedesktop/portal/desktop/request/cancel/secret1";
    let call_conn = conn.clone();
    let started = std::time::Instant::now();
    let (read_end, write_end) = pipe_pair();
    let worker = std::thread::spawn(move || {
        let portal = bus::proxy(
            &call_conn,
            PORTAL,
            DESKTOP_PATH,
            "org.freedesktop.impl.portal.Secret",
        );
        let fd: Fd<'_> = Fd::from(write_end);
        let handle = ObjectPath::try_from(handle_path).expect("request handle path");
        bus::call::<_, (u32, HashMap<String, zbus::zvariant::OwnedVariant>)>(
            &portal,
            "RetrieveSecret",
            &(
                handle,
                "dev.tessera.cancel",
                fd,
                HashMap::<String, Variant<'_>>::new(),
            ),
        )
    });

    // Wait until the request is actively parked on the stalled sigil socket.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sigil.observed().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "RetrieveSecret never reached fake sigil"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Issue Request.Close while sigil is stalled: the portal must abort the
    // socket wait immediately and answer 1 (cancelled).
    let req_proxy = bus::proxy(
        &conn,
        PORTAL,
        handle_path,
        "org.freedesktop.impl.portal.Request",
    );
    bus::call::<(), ()>(&req_proxy, "Close", &());

    let (code, _) = worker.join().expect("RetrieveSecret call thread");
    assert_eq!(
        code, 1,
        "Request.Close during sigil wait must answer 1 (cancelled)"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation must not wait for the 300s socket timeout"
    );
    drop(read_end);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
