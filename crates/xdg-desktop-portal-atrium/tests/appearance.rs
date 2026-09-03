//! End-to-end appearance forwarding: the compositor's desktop preferences
//! (served by the fake compositor over the real IPC socket) must reach the
//! prompter request as the contract-v6 `appearance` snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use atrium_portal_ipc::testing::{Handler, Server};
use atrium_portal_ipc::{AccentColor, ColorScheme, Contrast, DesktopPreferences, SettingsSnapshot};
use atrium_portal_prompter::{ConfirmResponse, PrompterResponse};
use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, Value};

mod common;
use common::{
    KillOnDrop, daemon_command, fake_prompter, private_bus, read_prompter_appearance, temp_dir,
    wait_for_name, write_prompter_response,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Account";

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

/// A compositor that stays silent (socket absent): the request must omit
/// the appearance snapshot and the prompter falls back to its platform
/// query.
#[test]
fn preferences_reach_the_prompter_request() {
    let Some(bus) = private_bus() else {
        eprintln!("appearance: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("appearance-data");
    let runtime_dir = temp_dir("appearance-runtime");
    let fixture_dir = temp_dir("appearance-prompter");

    let _server = Server::start(&runtime_dir.join("tessera.sock"), Arc::new(LightCompositor))
        .expect("start fake compositor");

    let prompter = fake_prompter(&fixture_dir);
    write_prompter_response(
        &fixture_dir,
        1,
        &PrompterResponse::confirm(ConfirmResponse::Confirmed),
    );

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let account = Proxy::new(&conn, PORTAL, DESKTOP_PATH, IFACE).expect("account proxy");
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/appr1")
        .expect("request handle path");
    let options: HashMap<String, Value<'_>> = HashMap::new();
    let (response, _results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = account
        .call(
            "GetUserInformation",
            &(handle, "dev.tessera.smoke", "", options),
        )
        .expect("GetUserInformation");
    assert_eq!(response, 0);

    let appearance = read_prompter_appearance(&fixture_dir, 1);
    let appearance = appearance.expect("the request must carry an appearance snapshot");
    assert_eq!(appearance.color_scheme, ColorScheme::Light);
    assert_eq!(
        appearance.accent_color,
        Some(AccentColor {
            red: 43,
            green: 101,
            blue: 232
        })
    );
    assert_eq!(appearance.contrast, Contrast::High);
    assert!(appearance.reduced_motion);

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// Without a compositor socket the backend's un-primed store still sends
/// a snapshot — the all-defaults one (`system`, no accent). That is
/// deliberate: a `system` scheme defers to the prompter's platform query
/// exactly like an absent snapshot would, and the default projection is
/// total over the preferences. What must NOT happen is a bogus concrete
/// scheme or accent leaking from an un-primed store.
#[test]
fn missing_compositor_sends_the_default_snapshot() {
    let Some(bus) = private_bus() else {
        eprintln!("appearance: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("appearance-none-data");
    let runtime_dir = temp_dir("appearance-none-runtime");
    let fixture_dir = temp_dir("appearance-none-prompter");

    let prompter = fake_prompter(&fixture_dir);
    write_prompter_response(
        &fixture_dir,
        1,
        &PrompterResponse::confirm(ConfirmResponse::Confirmed),
    );

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let account = Proxy::new(&conn, PORTAL, DESKTOP_PATH, IFACE).expect("account proxy");
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/appr2")
        .expect("request handle path");
    let options: HashMap<String, Value<'_>> = HashMap::new();
    let (response, _results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = account
        .call(
            "GetUserInformation",
            &(handle, "dev.tessera.smoke", "", options),
        )
        .expect("GetUserInformation");
    assert_eq!(response, 0);

    // The un-primed store's snapshot is all-defaults: scheme `system`
    // (which the prompter resolves through its platform query), no accent
    // override, both accessibility flags clear.
    let appearance = read_prompter_appearance(&fixture_dir, 1)
        .expect("the backend always sends an appearance snapshot");
    assert_eq!(appearance.color_scheme, ColorScheme::System);
    assert_eq!(appearance.accent_color, None);
    assert_eq!(appearance.contrast, Contrast::Normal);
    assert!(!appearance.reduced_motion);

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}
