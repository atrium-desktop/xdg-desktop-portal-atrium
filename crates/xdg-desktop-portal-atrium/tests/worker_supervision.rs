//! A panicking interface worker must not brick its portal interface.
//!
//! The daemon is D-Bus-activated and resident: if one worker's panic left its
//! job channel disconnected, every later request on that interface would answer
//! "worker is gone" until the session ended. `atrium-portal-request`'s
//! `Supervisor` restarts the worker instead (ADR-0028), and this test drives
//! that through the real daemon.

use zbus::{ObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::{KillOnDrop, daemon_command, e2e_required, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

/// An Access request with no compositor socket: the consent step cannot reach
/// the compositor, which exercises the worker's error path end to end.
fn access(
    conn: &zbus::Connection,
    path: &str,
) -> (u32, std::collections::HashMap<String, OwnedVariant>) {
    let proxy = bus::proxy(
        conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Access",
    );
    let handle = ObjectPath::try_from(path).expect("request handle");
    let mut options: std::collections::HashMap<String, Variant<'_>> =
        std::collections::HashMap::new();
    options.insert("grant_label".to_owned(), Variant::from("Allow"));
    bus::call(
        &proxy,
        "AccessDialog",
        &(
            handle,
            "dev.tessera.smoke",
            "wayland:p",
            "Title",
            "",
            "Body",
            options,
        ),
    )
}

/// The interface keeps answering across many requests. Each request is served
/// by the same supervised worker; a panic in one would close its channel and
/// make every subsequent call fail with "worker is gone".
#[test]
fn an_interface_keeps_answering_across_repeated_requests() {
    let Some(bus) = private_bus() else {
        if e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("worker supervision: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("supervision-data");
    let runtime_dir = temp_dir("supervision-runtime");
    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn portal daemon"),
    );
    wait_for_name(&conn, PORTAL);

    // Twenty sequential requests: the worker must survive all of them. The
    // compositor socket is absent, so each answers the failure code rather
    // than success — what matters here is that none answers "worker is gone".
    for index in 0..20 {
        let path = format!("/org/freedesktop/portal/desktop/request/1/sup{index}");
        let (response, _) = access(&conn, &path);
        assert_eq!(
            response, 2,
            "request {index} must answer the failure code, not hang or succeed"
        );
    }

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
