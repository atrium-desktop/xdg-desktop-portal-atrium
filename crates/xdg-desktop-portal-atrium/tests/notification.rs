//! Notification portal: routing metadata and the compositor delivery path.
//!
//! ADR-0026 gives notifications to the compositor's queue, so `AddNotification`
//! must reach it as a `Notify` command carrying the portal's per-application
//! id (the compositor stores it as `external_id`). These tests drive the real
//! daemon against a fake compositor.

use std::collections::HashMap;

use atrium_portal_ipc::Command;
use zbus::Proxy;
use zbus::Variant;

mod common;
use common::bus;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, daemon_command, e2e_required, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Notification";

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/tessera-portals.conf");

#[test]
fn notification_is_served_by_atrium() {
    let interface = "org.freedesktop.impl.portal.Notification";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == interface),
        "the Portal-owned Notification must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=atrium"))
    );
}

fn notifications(conn: &zbus::Connection) -> Proxy<'_> {
    bus::proxy(conn, PORTAL, DESKTOP_PATH, IFACE)
}

/// Start the daemon against a fake compositor, returning the guards and the
/// fake. `None` when no private bus is available.
fn start() -> Option<(
    zbus::Connection,
    std::sync::Arc<FakeCompositor>,
    atrium_portal_ipc::testing::Server,
    KillOnDrop,
    common::PrivateBus,
)> {
    let Some(bus) = private_bus() else {
        if e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("notification E2E: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("notify-data");
    let runtime_dir = temp_dir("notify-runtime");
    let (server, fake) = FakeCompositor::start(&runtime_dir.join("tessera.sock"))
        .expect("bind the fake compositor IPC");
    let daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn the portal daemon"),
    );
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);
    Some((conn, fake, server, daemon, bus))
}

#[test]
fn add_notification_reaches_the_compositor_queue() {
    let Some((conn, fake, _server, _daemon, _bus)) = start() else {
        return;
    };
    let mut notification: HashMap<String, Variant<'_>> = HashMap::new();
    notification.insert("title".to_owned(), Variant::from("Backup complete"));
    notification.insert("body".to_owned(), Variant::from("12 files archived."));

    bus::run(
        notifications(&conn)
            .call::<_, _, ()>("AddNotification", &("dev.tessera.App", "n1", notification)),
    )
    .expect("notification delivery must succeed");

    // The delivery runs on a short-lived thread, so wait for the command.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(Command::Notify {
            summary,
            body,
            app_id,
            external_id,
        }) = fake.commands.lock().unwrap().first().cloned()
        {
            assert_eq!(summary, "Backup complete");
            assert_eq!(body, "12 files archived.");
            assert_eq!(app_id.as_deref(), Some("dev.tessera.App"));
            // The compositor matches a later withdrawal by this id.
            assert_eq!(external_id.as_deref(), Some("n1"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the notification never reached the compositor"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_notification_without_text_is_rejected() {
    let Some((conn, fake, _server, _daemon, _bus)) = start() else {
        return;
    };
    let empty: HashMap<String, Variant<'_>> = HashMap::new();
    let error = bus::run(
        notifications(&conn).call::<_, _, ()>("AddNotification", &("dev.tessera.App", "n1", empty)),
    )
    .expect_err("a notification needs a title or a body");
    assert!(
        format!("{error}").contains("title or a body"),
        "unexpected error: {error}"
    );
    assert!(fake.commands.lock().unwrap().is_empty());
}

#[test]
fn markup_body_is_stripped_before_delivery() {
    let Some((conn, fake, _server, _daemon, _bus)) = start() else {
        return;
    };
    let mut notification: HashMap<String, Variant<'_>> = HashMap::new();
    notification.insert(
        "markup-body".to_owned(),
        Variant::from("<b>Bold</b> &amp; plain"),
    );
    bus::run(
        notifications(&conn)
            .call::<_, _, ()>("AddNotification", &("dev.tessera.App", "n1", notification)),
    )
    .expect("notification delivery must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(Command::Notify { body, .. }) = fake.commands.lock().unwrap().first().cloned() {
            assert_eq!(body, "Bold & plain");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the notification never reached the compositor"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
