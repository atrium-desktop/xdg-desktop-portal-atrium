//! Behavioural regression tests for the logind-backed Inhibit interface.
//!
//! These drive the **real daemon** over a private bus. That is the only way to
//! catch the failure this file exists for: zbus 6 dispatches interface methods
//! as tasks on the connection's own runtime, so an implementation that waits
//! synchronously inside `Inhibit` panics that task. The client then hangs until
//! its own timeout instead of receiving a reply, and — because the panic
//! happens before logind is asked — the inhibition silently never happens.
//!
//! A unit test cannot see this: it calls the interface logic directly, outside
//! the runtime the bug is about. The assertion below is therefore "the method
//! answers at all, as a D-Bus error rather than a timeout", which is exactly
//! the observable that regressed.
//!
//! `Inhibit` legitimately fails when logind is unreachable (that is the
//! documented contract: a truthful error, never a fake grant), so the test
//! accepts either `Ok` or a D-Bus error — what it refuses to accept is silence.

use std::collections::HashMap;

use zbus::Variant;

mod common;
use common::bus;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const INHIBIT_IFACE: &str = "org.freedesktop.impl.portal.Inhibit";

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/tessera-portals.conf");

#[test]
fn inhibit_is_served_by_atrium() {
    let interface = "org.freedesktop.impl.portal.Inhibit";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == interface),
        "the logind-backed Inhibit must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=atrium"))
    );
}

/// How long a served method may take before the daemon is considered wedged.
///
/// logind answers a local system-bus call in microseconds; the value is
/// generous enough for a debug build on a loaded machine and far below the
/// 15 s nextest kill, so a hang is reported as a failed assertion with a
/// useful message rather than a bare timeout.
const REPLY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// The flags that force a real logind round trip: idle (8) and suspend (4).
/// Logout/user-switch-only requests are tracked no-ops that never reach logind.
const LOGIND_BACKED_FLAGS: [u32; 3] = [8, 4, 12];

struct Fixture {
    conn: zbus::Connection,
    _fake: std::sync::Arc<FakeCompositor>,
    _server: atrium_portal_ipc::testing::Server,
    _daemon: KillOnDrop,
    _bus: common::PrivateBus,
}

fn start(tag: &str) -> Option<Fixture> {
    let bus = private_bus()?;
    let conn = bus.connect();
    let data_dir = temp_dir(&format!("{tag}-data"));
    let runtime_dir = temp_dir(&format!("{tag}-runtime"));
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
    Some(Fixture {
        conn,
        _fake: fake,
        _server: server,
        _daemon: daemon,
        _bus: bus,
    })
}

/// Call `Inhibit` and insist on a reply within [`REPLY_DEADLINE`].
///
/// Returns the D-Bus outcome so the caller can assert on its shape.
fn inhibit(
    conn: &zbus::Connection,
    request_path: &str,
    app_id: &str,
    flags: u32,
) -> Result<(), zbus::Error> {
    let proxy = bus::proxy(conn, PORTAL, DESKTOP_PATH, INHIBIT_IFACE);
    let handle = zbus::ObjectPath::try_from(request_path.to_string()).expect("valid object path");
    let app_id = app_id.to_owned();
    let options: HashMap<String, Variant<'_>> = HashMap::new();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // A detached caller: a wedged daemon must not wedge the test harness
    // itself, so the reply is bounded here rather than trusted.
    std::thread::spawn(move || {
        let reply = bus::try_call::<_, ()>(
            &proxy,
            "Inhibit",
            &(handle, app_id, String::new(), flags, options),
        );
        let _ = tx.send(reply);
    });
    match rx.recv_timeout(REPLY_DEADLINE) {
        Ok(reply) => reply,
        Err(error) => panic!(
            "Inhibit(flags {flags:#x}) produced no reply within {REPLY_DEADLINE:?} ({error}); \
             the served method blocked the connection runtime instead of awaiting"
        ),
    }
}

#[test]
fn inhibit_answers_instead_of_blocking_the_connection() {
    let Some(fixture) = start("inhibit-answer") else {
        return;
    };
    for (index, flags) in LOGIND_BACKED_FLAGS.into_iter().enumerate() {
        let path = format!("/org/freedesktop/portal/desktop/request/inhibit/answer{index}");
        // Either outcome is spec-legal: with no logind the call fails with
        // the documented honest error; with logind it takes the lock. What
        // matters is that a reply arrived at all (see `inhibit`).
        let _outcome = inhibit(&fixture.conn, &path, "dev.tessera.Inhibit", flags);
    }
}

/// The failure mode this file guards is *not* per-call: a blocked dispatch
/// task takes every other interface on the connection down with it. After the
/// inhibited calls above, an unrelated interface must still answer.
#[test]
fn other_interfaces_still_answer_after_inhibit() {
    let Some(fixture) = start("inhibit-unrelated") else {
        return;
    };
    let _ = inhibit(
        &fixture.conn,
        "/org/freedesktop/portal/desktop/request/inhibit/before",
        "dev.tessera.Inhibit",
        8,
    );

    let settings = bus::proxy(
        &fixture.conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Settings",
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        // Require a genuine value, not merely "the call returned": an
        // unrelated D-Bus error would arrive promptly even from a wedged
        // daemon and prove nothing about it still serving requests.
        let reply: Result<zbus::zvariant::OwnedVariant, _> = bus::try_call(
            &settings,
            "Read",
            &("org.freedesktop.appearance", "color-scheme"),
        );
        let _ = tx.send(reply);
    });
    let reply = rx
        .recv_timeout(REPLY_DEADLINE)
        .expect("Settings must keep answering after an Inhibit call");
    let scheme = reply.expect("Settings.Read must succeed after an Inhibit call");
    let scheme = u32::try_from(&scheme).expect("colour scheme must decode as u32");
    assert!(
        scheme <= 1,
        "colour scheme must be a known value, got {scheme}"
    );
}

/// A duplicate request handle is a protocol error, and it must be reported
/// *without* leaving a second `Close` object behind or taking a second lock.
#[test]
fn a_duplicate_handle_is_refused() {
    let Some(fixture) = start("inhibit-duplicate") else {
        return;
    };
    let path = "/org/freedesktop/portal/desktop/request/inhibit/dup";
    let flags = 8;
    let first = inhibit(&fixture.conn, path, "dev.tessera.Inhibit", flags);
    assert!(
        first.is_ok(),
        "the first Inhibit must succeed for the duplicate case to be exercised \
         (logind is required for this test): {first:?}"
    );
    let second = inhibit(&fixture.conn, path, "dev.tessera.Inhibit", flags);
    let error = second.expect_err("a duplicate request handle must be refused");
    assert!(
        error.to_string().contains("already active"),
        "unexpected error: {error}"
    );
}
