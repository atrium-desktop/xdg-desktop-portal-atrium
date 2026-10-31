//! End-to-end exercise of the Email backend: the real `xdg-desktop-portal-atrium` daemon
//! on a private session bus (see `tests/common/`) with `ATRIUM_PORTAL_MAILER`
//! pointed at a recorder script, so the backend-ABI `attachments` URI and
//! xdg-email hand-off are asserted without opening a mail client.
//!
//! ```sh
//! cargo test -p xdg-desktop-portal-atrium --test email
//! ```
//!
//! Without `dbus-daemon` available the test skips cleanly.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::process::Command;
use std::time::{Duration, Instant};

use zbus::{Fd, ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::{
    KillOnDrop, die_with_parent, e2e_required, frontend_binary, frontend_prerequisites,
    private_bus, temp_dir, wait_for_frontend, wait_for_name,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Email";

/// Poll the recorder file until the mailer ran (or fail after 5 s).
fn recorded_args(record_file: &std::path::Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(args) = std::fs::read_to_string(record_file) {
            return args;
        }
        assert!(
            Instant::now() < deadline,
            "the mailer was not invoked within 5 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn compose_email_hands_off_to_the_mailer() {
    let Some(bus) = private_bus() else {
        eprintln!("compose_email_hands_off_to_the_mailer: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("data");
    let runtime_dir = temp_dir("runtime");
    let record_file = runtime_dir.join("mailer-args.txt");
    let activation_file = runtime_dir.join("activation-token.txt");

    // A recorder standing in for xdg-email: appends its argv, one per line.
    let recorder = runtime_dir.join("record-mailer.sh");
    std::fs::write(
        &recorder,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nprintf '%s' \"$XDG_ACTIVATION_TOKEN\" > {}\n",
            record_file.display(),
            activation_file.display()
        ),
    )
    .expect("write recorder");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder");
    }

    // Like common::spawn_daemon, plus the mailer override.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-atrium"));
    cmd.env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", &data_dir)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("ATRIUM_PORTAL_MAILER", &recorder)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _daemon = KillOnDrop(cmd.spawn().expect("spawn xdg-desktop-portal-atrium"));

    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);
    let email = bus::proxy(&conn, PORTAL, DESKTOP_PATH, IFACE);

    // The public portal turns attachment fds into local file URIs before it
    // calls the backend. Exercise exactly that backend ABI here.
    let payload_file = runtime_dir.join("payload with space.bin");
    std::fs::write(&payload_file, b"attached-bytes").expect("write payload");
    let attachment_uri = format!(
        "file://{}",
        payload_file.to_string_lossy().replace(' ', "%20")
    );

    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert("address".to_string(), Variant::from("to@example.com"));
    options.insert(
        "cc".to_string(),
        Variant::from(vec!["carbon@example.com".to_string()]),
    );
    options.insert("subject".to_string(), Variant::from("portal subject"));
    options.insert("body".to_string(), Variant::from("portal body"));
    options.insert(
        "activation_token".to_string(),
        Variant::from("test-activation-token"),
    );
    options.insert(
        "attachments".to_string(),
        Variant::from(vec![attachment_uri]),
    );

    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/mail1")
        .expect("request handle path");
    let (response, _results): (u32, HashMap<String, OwnedVariant>) = bus::call(
        &email,
        "ComposeEmail",
        &(handle, "dev.tessera.smoke", "", options),
    );
    assert_eq!(response, 0, "ComposeEmail must report success");

    let args = recorded_args(&record_file);
    let lines: Vec<&str> = args.lines().collect();
    let flag_value = |flag: &str| {
        let at = lines
            .iter()
            .position(|arg| *arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in {args}"));
        lines[at + 1]
    };
    assert_eq!(flag_value("--cc"), "carbon@example.com");
    assert_eq!(flag_value("--subject"), "portal subject");
    assert_eq!(flag_value("--body"), "portal body");
    let attach = flag_value("--attach");
    assert_eq!(attach, payload_file.to_str().expect("utf8 payload path"));
    assert_eq!(
        std::fs::read(attach).expect("staged attachment"),
        b"attached-bytes"
    );
    assert_eq!(lines.last().copied(), Some("mailto:to@example.com"));
    assert_eq!(
        std::fs::read_to_string(&activation_file).expect("activation token record"),
        "test-activation-token"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn public_frontend_translates_attachment_fds_for_the_backend() {
    let Some(bus) = private_bus() else {
        eprintln!("public frontend test: no dbus-daemon, skipping");
        return;
    };
    // The frontend cannot start without a service-activation helper; report
    // that as a skip instead of spending its 25 s timeout on a failure it
    // could never pass.
    if let Err(reason) = frontend_prerequisites(&bus) {
        if e2e_required() {
            panic!("required public-frontend prerequisite failed: {reason}");
        }
        eprintln!("public frontend test: {reason}, skipping");
        return;
    }
    let frontend = frontend_binary().expect("checked by frontend_prerequisites");
    let conn = bus.connect();

    let root = temp_dir("email-frontend");
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
        std::fs::create_dir_all(directory).expect("create frontend fixture directory");
    }
    std::fs::write(
        portal_dir.join("atrium.portal"),
        include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal"),
    )
    .expect("stage portal metadata");
    std::fs::write(
        // 1.18 only consults the desktop-specific filename when
        // XDG_CURRENT_DESKTOP is non-empty. Mirror the installed package.
        config_dir.join("tessera-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.Email=atrium\n",
    )
    .expect("stage frontend routing");
    std::fs::write(
        config_dir.join("atrium-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.Email=atrium\n",
    )
    .expect("stage frontend routing compatibility");

    let record_file = runtime_dir.join("public-mailer-args.txt");
    let backend_log = root.join("backend.log");
    let recorder = runtime_dir.join("record-public-mailer.sh");
    std::fs::write(
        &recorder,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\n",
            record_file.display()
        ),
    )
    .expect("write recorder");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder");
    }

    let mut backend = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-atrium"));
    backend
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", &backend_data)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("ATRIUM_PORTAL_MAILER", &recorder)
        .env("RUST_LOG", "debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(
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

    let email = bus::proxy(
        &conn,
        "org.freedesktop.portal.Desktop",
        DESKTOP_PATH,
        "org.freedesktop.portal.Email",
    );
    let token = "atrium_email_frontend_1";
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

    let attachment_path = runtime_dir.join("frontend attachment.bin");
    std::fs::write(&attachment_path, b"through-the-real-frontend").expect("attachment payload");
    let attachment_file = std::fs::File::open(&attachment_path).expect("open attachment");
    // SAFETY: ownership of this raw descriptor moves exactly once.
    let attachment: Fd<'_> =
        Fd::from(unsafe { OwnedFd::from_raw_fd(attachment_file.into_raw_fd()) });
    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert("handle_token".to_string(), Variant::from(token));
    options.insert("subject".to_string(), Variant::from("frontend boundary"));
    options.insert(
        "attachment_fds".to_string(),
        Variant::from(vec![attachment]),
    );

    let returned: OwnedObjectPath = bus::try_call(&email, "ComposeEmail", &("", options))
        .unwrap_or_else(|error| {
            let log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
            panic!("public ComposeEmail failed: {error}\nfrontend log:\n{log}")
        });
    assert_eq!(returned.as_str(), expected_handle);
    let response = bus::next_signal(&mut responses);
    let (code, _results): (u32, HashMap<String, OwnedVariant>) =
        response.body().deserialize().expect("Response body");
    if code != 0 {
        let backend_log = std::fs::read_to_string(&backend_log).unwrap_or_default();
        let frontend_log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
        panic!(
            "public ComposeEmail returned {code}\nbackend log:\n{backend_log}\nfrontend log:\n{frontend_log}"
        );
    }

    let args = recorded_args(&record_file);
    let lines: Vec<&str> = args.lines().collect();
    let attach_at = lines
        .iter()
        .position(|argument| *argument == "--attach")
        .unwrap_or_else(|| panic!("frontend attachment did not reach backend: {args}"));
    assert_eq!(lines[attach_at + 1], attachment_path.to_string_lossy());

    std::fs::remove_dir_all(root).ok();
}
