//! AppChooser portal: routing metadata and the compositor picking path.
//!
//! ADR-0026 gives application choice to compositor chrome, so
//! `ChooseApplication` must reach the compositor as a `PickApp` carrying the
//! resolved desktop-file ids, and the confirmed id must come back as the
//! portal's `{"choice": id}` result. These tests drive the real daemon against
//! a fake compositor; the routing assertion keeps the metadata honest.

use std::collections::HashMap;

use atrium_portal_ipc::AppPickResult;
use zbus::{ObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, daemon_command, e2e_required, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.AppChooser";

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/tessera-portals.conf");

#[test]
fn app_chooser_is_served_by_atrium() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == IFACE),
        "the Portal-owned AppChooser must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{IFACE}=atrium"))
    );
}

/// A hermetic XDG tree containing two `text/plain` handlers, so candidate
/// resolution produces real desktop ids for the compositor.
fn seed_applications(data_dir: &std::path::Path) {
    let applications = data_dir.join("applications");
    std::fs::create_dir_all(&applications).expect("applications dir");
    for (id, name) in [
        ("editor.desktop", "Foo Editor"),
        ("viewer.desktop", "Bar Viewer"),
    ] {
        std::fs::write(
            applications.join(id),
            format!("[Desktop Entry]\nName={name}\nExec={id} %U\nMimeType=text/plain;\n"),
        )
        .expect("desktop entry fixture");
    }
}

/// Start the daemon against a fake compositor with `answer` queued for the
/// pick, returning the connection, the fake, and the daemon guard.
fn start(
    answer: AppPickResult,
    seed: bool,
) -> Option<(
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
        eprintln!("AppChooser E2E: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("app-chooser-data");
    let runtime_dir = temp_dir("app-chooser-runtime");
    if seed {
        seed_applications(&data_dir);
    }
    let (server, fake) = FakeCompositor::start(&runtime_dir.join("tessera.sock"))
        .expect("bind the fake compositor IPC");
    fake.app_answers.lock().unwrap().push(answer);
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

fn choose_application(
    conn: &zbus::Connection,
    content_type: &str,
) -> (u32, HashMap<String, OwnedVariant>) {
    let chooser = bus::proxy(conn, PORTAL, DESKTOP_PATH, IFACE);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/app1")
        .expect("request handle path");
    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert("last_choice".to_owned(), Variant::from("viewer.desktop"));
    bus::call(
        &chooser,
        "ChooseApplication",
        &(
            handle,
            "dev.tessera.smoke",
            "wayland:parent",
            Vec::<String>::new(),
            content_type,
            options,
        ),
    )
}

#[test]
fn a_confirmed_choice_becomes_the_portal_result() {
    let Some((conn, fake, _server, _daemon, _bus)) = start(
        AppPickResult::App {
            id: "viewer.desktop".to_owned(),
        },
        true,
    ) else {
        return;
    };
    let (response, results) = choose_application(&conn, "text/plain");
    assert_eq!(response, 0, "a confirmed choice reports success");
    let choice = String::try_from(results["choice"].clone()).expect("choice is a string");
    assert_eq!(choice, "viewer.desktop");

    // The compositor saw the resolved candidates and the pre-highlight hint,
    // and nothing but desktop ids crossed the wire.
    let picks = fake.app_picks.lock().unwrap().clone();
    assert_eq!(picks.len(), 1, "exactly one pick was requested");
    let (offered, _subject, last_choice) = &picks[0];
    let mut sorted = offered.clone();
    sorted.sort();
    assert_eq!(sorted, ["editor.desktop", "viewer.desktop"]);
    assert_eq!(last_choice.as_deref(), Some("viewer.desktop"));
}

#[test]
fn a_dismissed_picker_answers_cancelled() {
    let Some((conn, fake, _server, _daemon, _bus)) = start(AppPickResult::Cancelled, true) else {
        return;
    };
    let (response, results) = choose_application(&conn, "text/plain");
    assert_eq!(response, 1, "dismissal answers 1");
    assert!(results.is_empty());
    assert_eq!(fake.app_picks.lock().unwrap().len(), 1);
}

/// A compositor that confirms an application the portal never offered must be
/// refused, not reported as success: the answer is a fault boundary.
#[test]
fn an_unoffered_application_is_refused() {
    let Some((conn, _fake, _server, _daemon, _bus)) = start(
        AppPickResult::App {
            id: "org.evil.Impostor.desktop".to_owned(),
        },
        true,
    ) else {
        return;
    };
    let (response, results) = choose_application(&conn, "text/plain");
    assert_eq!(response, 2, "an unoffered id is a failure");
    assert!(results.is_empty());
}

/// With no handler installed for the content type there is nothing to offer,
/// so the request fails before any prompt.
#[test]
fn an_unhandled_content_type_fails_without_a_prompt() {
    let Some((conn, fake, _server, _daemon, _bus)) = start(
        AppPickResult::App {
            id: "viewer.desktop".to_owned(),
        },
        false,
    ) else {
        return;
    };
    let (response, results) = choose_application(&conn, "application/x-nobody-opens");
    assert_eq!(response, 2, "no candidate means failure");
    assert!(results.is_empty());
    assert!(
        fake.app_picks.lock().unwrap().is_empty(),
        "an empty candidate list must not reach the compositor"
    );
}
