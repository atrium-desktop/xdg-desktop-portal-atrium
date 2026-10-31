//! End-to-end settings projection: the compositor's desktop preferences
//! (served by a fake compositor over the real IPC socket) must be what the
//! `org.freedesktop.impl.portal.Settings` interface reports.
//!
//! The backend no longer forwards an appearance snapshot to a prompt process
//! (ADR-0026): every prompt is compositor chrome, and the compositor skins its
//! own surfaces from the same preferences. What the portal still owes the
//! desktop is the *projection* of those preferences on the Settings interface,
//! which is what these tests drive.

use std::sync::Arc;

use atrium_portal_ipc::testing::{Handler, Server};
use atrium_portal_ipc::{AccentColor, ColorScheme, Contrast, DesktopPreferences, SettingsSnapshot};
use zbus::OwnedVariant;

mod common;
use common::bus;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";

/// A compositor that reports a fully-specified light appearance.
#[derive(Default)]
struct LightCompositor;

impl Handler for LightCompositor {
    fn settings(&self) -> SettingsSnapshot {
        SettingsSnapshot {
            preferences: DesktopPreferences {
                color_scheme: ColorScheme::Light,
                accent_color: Some(AccentColor {
                    red: 43,
                    green: 101,
                    blue: 232,
                }),
                contrast: Contrast::High,
                reduced_motion: true,
                ..DesktopPreferences::default()
            },
        }
    }
}

/// Read one appearance key through the backend interface. The *backend*
/// method is `Read`; `ReadOne` exists only on the frontend facade.
fn read(conn: &zbus::Connection, key: &str) -> OwnedVariant {
    let settings = bus::proxy(conn, PORTAL, DESKTOP_PATH, IFACE);
    bus::call(&settings, "Read", &(APPEARANCE, key))
}

#[test]
fn preferences_reach_the_settings_projection() {
    let Some(bus) = private_bus() else {
        eprintln!("appearance: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("appearance-data");
    let runtime_dir = temp_dir("appearance-runtime");
    let _server = Server::start(&runtime_dir.join("tessera.sock"), Arc::new(LightCompositor));

    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn portal daemon"),
    );
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    let scheme = u32::try_from(read(&conn, "color-scheme")).expect("color-scheme is a u32");
    assert_eq!(
        scheme, 2,
        "light scheme projects as 2 (system=0, dark=1, light=2)"
    );
    let contrast = u32::try_from(read(&conn, "contrast")).expect("contrast is a u32");
    assert_eq!(contrast, 1, "high contrast projects as 1");
    let accent: (f64, f64, f64) = read(&conn, "accent-color")
        .try_into()
        .expect("accent is (ddd)");
    // The accent projects as 8-bit channels divided by 255.
    let expected = (43.0 / 255.0, 101.0 / 255.0, 232.0 / 255.0);
    assert!(
        (accent.0 - expected.0).abs() < 1e-6
            && (accent.1 - expected.1).abs() < 1e-6
            && (accent.2 - expected.2).abs() < 1e-6,
        "accent {accent:?} does not match {expected:?}"
    );
    // The standardized key is a u32 flag (0/1), not a boolean.
    let reduced = u32::try_from(read(&conn, "reduced-motion")).expect("reduced-motion is a u32");
    assert_eq!(reduced, 1);

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

/// Without a compositor socket the backend serves its un-primed store: the
/// all-defaults projection (`system`, no accent), never a bogus concrete
/// scheme left over from another session.
#[test]
fn a_missing_compositor_leaves_the_default_projection() {
    let Some(bus) = private_bus() else {
        eprintln!("appearance: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("appearance-none-data");
    let runtime_dir = temp_dir("appearance-none-runtime");
    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn portal daemon"),
    );
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    let scheme = u32::try_from(read(&conn, "color-scheme")).expect("color-scheme is a u32");
    assert_eq!(scheme, 0, "an un-primed store reports `system`");
    let contrast = u32::try_from(read(&conn, "contrast")).expect("contrast is a u32");
    assert_eq!(contrast, 0, "an un-primed store reports normal contrast");

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
