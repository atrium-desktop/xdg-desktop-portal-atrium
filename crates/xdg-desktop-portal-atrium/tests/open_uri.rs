//! OpenURI portal: routing metadata and the compositor picking path.
//!
//! `OpenURI` with `ask=true` reuses the AppChooser surface (ADR-0026): the
//! compositor must receive the resolved desktop ids as a `PickApp`, and the
//! confirmed id must be the one launched. These tests drive the real daemon
//! against a fake compositor; a launched application is a `sh -c` script that
//! drops a marker file, so the test observes the launch without a real GUI.

use std::collections::HashMap;

use atrium_portal_ipc::AppPickResult;
use zbus::{ObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, daemon_command, e2e_required, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.OpenURI";

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/tessera-portals.conf");

#[test]
fn open_uri_is_served_by_atrium() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == IFACE),
        "the Portal-owned OpenURI must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{IFACE}=atrium"))
    );
}

/// Two `x-scheme-handler/smoke` handlers. `chosen.desktop` writes a marker so
/// the test can prove which one ran; `other.desktop` writes nothing.
fn seed_handlers(data_dir: &std::path::Path, marker: &std::path::Path) -> String {
    let applications = data_dir.join("applications");
    std::fs::create_dir_all(&applications).expect("applications dir");
    std::fs::write(
        applications.join("chosen.desktop"),
        // The Exec field is split with Desktop-Entry quoting (double quotes
        // only), so the inner command is quoted with `"`. The marker path is
        // absolute and space-free.
        format!(
            "[Desktop Entry]\nName=Chosen\nExec=sh -c \"echo ran > {}\" %u\nMimeType=x-scheme-handler/smoke;\n",
            marker.display()
        ),
    )
    .expect("chosen entry");
    std::fs::write(
        applications.join("other.desktop"),
        "[Desktop Entry]\nName=Other\nExec=true %u\nMimeType=x-scheme-handler/smoke;\n",
    )
    .expect("other entry");
    marker.display().to_string()
}

fn open_uri(conn: &zbus::Connection, uri: &str) -> (u32, HashMap<String, OwnedVariant>) {
    let open = bus::proxy(conn, PORTAL, DESKTOP_PATH, IFACE);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/uri1")
        .expect("request handle path");
    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert("ask".to_owned(), Variant::from(true));
    bus::call(
        &open,
        "OpenURI",
        &(handle, "dev.tessera.smoke", "", uri, options),
    )
}

#[test]
fn a_confirmed_choice_launches_that_application() {
    let Some(bus) = private_bus() else {
        if e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("OpenURI E2E: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("open-uri-data");
    let runtime_dir = temp_dir("open-uri-runtime");
    let marker = temp_dir("open-uri-marker").join("ran");
    std::fs::create_dir_all(marker.parent().unwrap()).expect("marker dir");
    seed_handlers(&data_dir, &marker);

    let (server, fake) = FakeCompositor::start(&runtime_dir.join("tessera.sock"))
        .expect("bind the fake compositor IPC");
    fake.app_answers.lock().unwrap().push(AppPickResult::App {
        id: "chosen.desktop".to_owned(),
    });
    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn the portal daemon"),
    );
    wait_for_name(&conn, PORTAL);

    let (response, results) = open_uri(&conn, "smoke://payload");
    assert_eq!(response, 0, "a launched application reports success");
    assert!(results.is_empty(), "OpenURI returns no results");

    // The compositor was offered the resolved handlers and nothing but ids.
    let picks = fake.app_picks.lock().unwrap().clone();
    assert_eq!(picks.len(), 1, "exactly one pick was requested");
    let mut offered = picks[0].0.clone();
    offered.sort();
    assert_eq!(offered, ["chosen.desktop", "other.desktop"]);

    // The confirmed application actually ran.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the confirmed application was never launched"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "ran");

    drop(server);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_dismissed_picker_launches_nothing() {
    let Some(bus) = private_bus() else {
        if e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("OpenURI E2E: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("open-uri-cancel-data");
    let runtime_dir = temp_dir("open-uri-cancel-runtime");
    let marker = temp_dir("open-uri-cancel-marker").join("ran");
    std::fs::create_dir_all(marker.parent().unwrap()).expect("marker dir");
    seed_handlers(&data_dir, &marker);

    let (server, fake) = FakeCompositor::start(&runtime_dir.join("tessera.sock"))
        .expect("bind the fake compositor IPC");
    fake.app_answers
        .lock()
        .unwrap()
        .push(AppPickResult::Cancelled);
    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn the portal daemon"),
    );
    wait_for_name(&conn, PORTAL);

    let (response, results) = open_uri(&conn, "smoke://payload");
    assert_eq!(response, 1, "dismissal answers 1");
    assert!(results.is_empty());
    // Give a wrongly-launched child a moment to show up before asserting.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !marker.exists(),
        "nothing may be launched when the picker was dismissed"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
