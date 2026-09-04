//! End-to-end smoke test for the native Secret portal. The test starts the
//! real backend on a private session bus, verifies that the incomplete
//! Secret Service compatibility API is not exposed, and exercises fd-based
//! secret delivery through a Portal-owned fake sigil daemon.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use zbus::blocking::Proxy;
use zbus::zvariant::{Fd, ObjectPath, OwnedObjectPath, Value};

mod common;
use common::{
    FakeSigil, FakeSigilResponse, KillOnDrop, e2e_required, pipe_pair, private_bus,
    read_all_with_timeout, spawn_daemon, temp_dir, wait_for_name,
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

    let fdo = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .expect("D-Bus proxy");
    let compat_owned: bool = fdo
        .call("NameHasOwner", &("org.freedesktop.secrets",))
        .expect("NameHasOwner");
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

    let portal = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    )
    .expect("portal secret proxy");
    let version: u32 = portal.get_property("version").expect("version property");
    assert_eq!(version, 1);

    for (interface, expected) in [
        ("org.freedesktop.impl.portal.Settings", 1),
        ("org.freedesktop.impl.portal.Screenshot", 3),
        ("org.freedesktop.impl.portal.ScreenCast", 6),
    ] {
        let proxy = Proxy::new(&conn, PORTAL, DESKTOP_PATH, interface).expect("backend proxy");
        let version: u32 = proxy
            .get_property("version")
            .unwrap_or_else(|error| panic!("{interface} must expose version: {error}"));
        assert_eq!(version, expected, "{interface} version");
    }

    let lockdown = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Lockdown",
    )
    .expect("lockdown proxy");
    for property in [
        "disable-printing",
        "disable-save-to-disk",
        "disable-application-handlers",
        "disable-location",
        "disable-camera",
        "disable-microphone",
        "disable-sound-output",
    ] {
        let restricted: bool = lockdown
            .get_property(property)
            .unwrap_or_else(|error| panic!("Lockdown must expose {property}: {error}"));
        assert!(!restricted, "Lockdown {property} must be permissive");
    }
    lockdown
        .set_property("disable-printing", true)
        .expect("Lockdown property must be writable");
    // Read through a fresh proxy so this checks the service rather than the
    // first proxy's property cache racing the PropertiesChanged signal.
    let lockdown_after_set = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Lockdown",
    )
    .expect("fresh lockdown proxy");
    let printing_disabled: bool = lockdown_after_set
        .get_property("disable-printing")
        .expect("updated Lockdown property");
    assert!(printing_disabled);

    let introspectable = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.DBus.Introspectable",
    )
    .expect("introspection proxy");
    let xml: String = introspectable
        .call("Introspect", &())
        .expect("backend introspection");
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
    let (response, _results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = portal
        .call(
            "RetrieveSecret",
            &(
                handle,
                "dev.tessera.smoke",
                fd,
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("RetrieveSecret");
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
    let frontend = [
        "/usr/libexec/xdg-desktop-portal",
        "/usr/lib/xdg-desktop-portal",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.is_file());
    let Some(frontend) = frontend else {
        assert!(
            !e2e_required(),
            "required xdg-desktop-portal frontend is unavailable"
        );
        eprintln!("public Secret test: xdg-desktop-portal unavailable, skipping");
        return;
    };
    let Some(bus) = private_bus() else {
        eprintln!("public Secret test: dbus-daemon unavailable, skipping");
        return;
    };
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
        config_dir.join("atrium-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.Secret=atrium\n",
    )
    .expect("stage portal routing");

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

    let frontend_log = root.join("frontend.log");
    let mut frontend_command = Command::new(frontend);
    frontend_command
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_CURRENT_DESKTOP", "atrium")
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
    let _frontend = KillOnDrop(
        frontend_command
            .spawn()
            .expect("spawn xdg-desktop-portal frontend"),
    );
    wait_for_name(&conn, "org.freedesktop.portal.Desktop");

    let secret = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        DESKTOP_PATH,
        "org.freedesktop.portal.Secret",
    )
    .expect("public Secret proxy");
    let token = "atrium_secret_frontend_1";
    let sender = conn
        .unique_name()
        .expect("private bus connection has a unique name")
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let expected_handle = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        expected_handle.as_str(),
        "org.freedesktop.portal.Request",
    )
    .expect("public request proxy");
    let mut responses = request
        .receive_signal("Response")
        .expect("subscribe Response");

    let (read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let options = HashMap::from([("handle_token".to_owned(), Value::from(token))]);
    let returned: OwnedObjectPath = secret
        .call("RetrieveSecret", &(fd, options))
        .unwrap_or_else(|error| {
            let log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
            panic!("public RetrieveSecret failed: {error}\nfrontend log:\n{log}")
        });
    assert_eq!(returned.as_str(), expected_handle);
    let response = responses.next().expect("portal must emit Response");
    let (code, _results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) =
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
