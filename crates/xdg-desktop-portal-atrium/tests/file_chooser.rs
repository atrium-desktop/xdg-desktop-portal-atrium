//! End-to-end FileChooser exercise using the real daemon on a private bus and
//! a pipe-compatible fake prompter. No compositor or display participates.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use atrium_arca_contract::{
    BytePath, Choice, FileChooserMode, FileChooserRequest, FileChooserResponse, FileFilter,
    FilterRule, FilterRuleKind,
};
use zbus::{Connection, Proxy};
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::{
    KillOnDrop, daemon_command, die_with_parent, e2e_required, fake_chooser, frontend_binary,
    frontend_prerequisites, private_bus, temp_dir, wait_for_frontend, wait_for_name,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.FileChooser";

fn chooser(conn: &Connection) -> Proxy<'_> {
    bus::proxy(conn, PORTAL, DESKTOP_PATH, IFACE)
}

fn call_chooser(
    proxy: &Proxy<'_>,
    method: &str,
    parent: &str,
    title: &str,
    options: HashMap<String, Variant<'_>>,
    serial: u32,
) -> (u32, HashMap<String, OwnedVariant>) {
    let handle = ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/1/fc{serial}"
    ))
    .expect("request handle path");
    bus::try_call(
        proxy,
        method,
        &(handle, "dev.tessera.smoke", parent, title, options),
    )
    .unwrap_or_else(|error| panic!("{method} must succeed at the bus level: {error}"))
}

fn result_uris(results: &HashMap<String, OwnedVariant>) -> Vec<String> {
    let uris = results
        .get("uris")
        .unwrap_or_else(|| panic!("results must contain uris: {results:?}"));
    Vec::<String>::try_from(Variant::from(uris.clone())).expect("uris is a string array")
}

/// Write one scripted arca reply: the versioned envelope ADR-0027 defines.
fn write_response(directory: &Path, index: u32, response: &FileChooserResponse) {
    let envelope = serde_json::json!({
        "version": atrium_arca_contract::ARCA_CONTRACT_VERSION,
        "response": response,
    });
    std::fs::write(
        directory.join(format!("response-{index}.json")),
        serde_json::to_vec(&envelope).expect("serialize the arca reply"),
    )
    .expect("write the arca reply");
}

/// Read and decode one request the fake chooser recorded.
fn read_request(directory: &Path, index: u32) -> FileChooserRequest {
    let bytes = std::fs::read(directory.join(format!("request-{index}.json")))
        .expect("read the recorded chooser request");
    let envelope: serde_json::Value =
        serde_json::from_slice(&bytes).expect("decode the chooser envelope");
    assert_eq!(
        envelope["version"],
        atrium_arca_contract::ARCA_CONTRACT_VERSION,
        "the chooser must speak the contract version"
    );
    serde_json::from_value(envelope["request"].clone()).expect("decode the chooser request")
}

#[test]
fn file_chooser_end_to_end() {
    let Some(bus) = private_bus() else {
        eprintln!("file_chooser_end_to_end: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("data");
    let runtime_dir = temp_dir("runtime");
    let fixture_dir = temp_dir("prompter");
    let prompter = fake_chooser(&fixture_dir);

    let image_filter = FileFilter {
        label: "Images".into(),
        rules: vec![
            FilterRule {
                kind: FilterRuleKind::Glob,
                value: "image/*".into(),
            },
            FilterRule {
                kind: FilterRuleKind::Mime,
                value: "image/png".into(),
            },
        ],
    };
    write_response(
        &fixture_dir,
        1,
        &FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/fake-chosen.png")],
            current_filter: Some(image_filter.clone()),
            choices: vec![("encoding".into(), "utf8".into())],
        },
    );
    write_response(
        &fixture_dir,
        2,
        &FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/fake-save.png")],
            current_filter: None,
            choices: Vec::new(),
        },
    );
    write_response(
        &fixture_dir,
        3,
        &FileChooserResponse::Selected {
            // SaveFiles collision/name processing belongs to the prompter;
            // the backend receives final paths only.
            paths: vec![
                BytePath::from_path("/chosen/dir/one.txt"),
                BytePath::from_path("/chosen/dir/two.txt"),
            ],
            current_filter: None,
            choices: Vec::new(),
        },
    );
    write_response(&fixture_dir, 4, &FileChooserResponse::Cancelled);
    write_response(
        &fixture_dir,
        6,
        &FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/concurrent.txt")],
            current_filter: None,
            choices: Vec::new(),
        },
    );

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("ATRIUM_FILE_CHOOSER_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));

    wait_for_name(&conn, PORTAL);
    let chooser_proxy = chooser(&conn);
    // OpenFile exercises every v3 option whose old compositor picker lost.
    let filters = vec![(
        "Images".to_owned(),
        vec![(0u32, "image/*".to_owned()), (1u32, "image/png".to_owned())],
    )];
    let current_filter = filters[0].clone();
    let choices = vec![(
        "encoding".to_owned(),
        "Encoding".to_owned(),
        vec![("utf8".to_owned(), "UTF-8".to_owned())],
        "utf8".to_owned(),
    )];
    let mut options = HashMap::new();
    options.insert("modal".to_owned(), Variant::from(false));
    options.insert("multiple".to_owned(), Variant::from(true));
    options.insert("accept_label".to_owned(), Variant::from("Import"));
    options.insert("filters".to_owned(), Variant::from(filters));
    options.insert("current_filter".to_owned(), Variant::from(current_filter));
    options.insert("choices".to_owned(), Variant::from(choices));
    options.insert(
        "current_folder".to_owned(),
        Variant::from(b"/tmp/images\0".to_vec()),
    );
    let (response, results) = call_chooser(
        &chooser_proxy,
        "OpenFile",
        "wayland:parent-token",
        "Import image",
        options,
        1,
    );
    assert_eq!(response, 0, "OpenFile must succeed: {results:?}");
    assert_eq!(result_uris(&results), ["file:///tmp/fake-chosen.png"]);
    assert!(results.contains_key("current_filter"));
    assert!(results.contains_key("choices"));
    let request = read_request(&fixture_dir, 1);
    assert_eq!(request.mode, FileChooserMode::OpenFile);
    assert_eq!(
        request.parent_window.as_deref(),
        Some("wayland:parent-token")
    );
    assert!(!request.modal);
    assert!(request.multiple);
    assert_eq!(request.accept_label.as_deref(), Some("Import"));
    assert_eq!(request.current_filter, Some(image_filter));
    assert_eq!(
        request.choices,
        [Choice {
            id: "encoding".into(),
            label: "Encoding".into(),
            options: vec![("utf8".into(), "UTF-8".into())],
            selected: "utf8".into(),
        }]
    );

    // SaveFile preserves current_file as one semantic value instead of
    // reconstructing it from lossy UTF-8 folder/name pieces.
    let mut options = HashMap::new();
    options.insert(
        "current_file".to_owned(),
        Variant::from(b"/tmp/existing.png\0".to_vec()),
    );
    let (response, results) = call_chooser(&chooser_proxy, "SaveFile", "", "Save it", options, 2);
    assert_eq!(response, 0, "SaveFile must succeed: {results:?}");
    assert_eq!(result_uris(&results), ["file:///tmp/fake-save.png"]);
    let request = read_request(&fixture_dir, 2);
    assert_eq!(request.mode, FileChooserMode::SaveFile);
    assert_eq!(
        request.current_file.unwrap().to_path_buf(),
        Path::new("/tmp/existing.png")
    );

    let mut options = HashMap::new();
    options.insert(
        "files".to_owned(),
        Variant::from(vec![b"one.txt\0".to_vec(), b"two.txt\0".to_vec()]),
    );
    let (response, results) =
        call_chooser(&chooser_proxy, "SaveFiles", "", "Save them", options, 3);
    assert_eq!(response, 0, "SaveFiles must succeed: {results:?}");
    assert_eq!(
        result_uris(&results),
        ["file:///chosen/dir/one.txt", "file:///chosen/dir/two.txt"]
    );
    let request = read_request(&fixture_dir, 3);
    assert_eq!(request.mode, FileChooserMode::SaveFiles);
    assert_eq!(
        request
            .files
            .iter()
            .map(BytePath::to_path_buf)
            .collect::<Vec<_>>(),
        [Path::new("one.txt"), Path::new("two.txt")]
    );

    let (response, _) = call_chooser(
        &chooser_proxy,
        "OpenFile",
        "",
        "Cancel it",
        HashMap::new(),
        4,
    );
    assert_eq!(response, 1);

    // Request.Close is an active cancellation boundary, and a prompter that
    // stays alive must not serialize unrelated clients behind it.
    let bus_address = bus.address().to_owned();
    let started = std::time::Instant::now();
    let pending = std::thread::spawn(move || {
        let connection = bus::connect(&bus_address);
        let chooser = chooser(&connection);
        call_chooser(&chooser, "OpenFile", "", "Close it", HashMap::new(), 5)
    });
    let request_file = fixture_dir.join("request-5.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !request_file.is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "prompter did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let bus_address = bus.address().to_owned();
    let (concurrent_tx, concurrent_rx) = std::sync::mpsc::channel();
    let concurrent = std::thread::spawn(move || {
        let connection = bus::connect(&bus_address);
        let chooser = chooser(&connection);
        let result = call_chooser(&chooser, "OpenFile", "", "Concurrent", HashMap::new(), 6);
        concurrent_tx.send(result).unwrap();
    });
    let (response, results) = concurrent_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("a blocked chooser must not serialize an unrelated client");
    assert_eq!(response, 0);
    assert_eq!(result_uris(&results), ["file:///tmp/concurrent.txt"]);
    concurrent.join().unwrap();

    let request = bus::proxy(
        &conn,
        PORTAL,
        "/org/freedesktop/portal/desktop/request/1/fc5",
        "org.freedesktop.impl.portal.Request",
    );
    bus::call::<(), ()>(&request, "Close", &());
    let (response, _) = pending.join().unwrap();
    assert_eq!(response, 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    let _ = std::fs::remove_dir_all(data_dir);
    let _ = std::fs::remove_dir_all(runtime_dir);
    let _ = std::fs::remove_dir_all(fixture_dir);
}

#[test]
fn public_frontend_routes_file_chooser_and_returns_response() {
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
    let _frontend = frontend_binary();
    let conn = bus.connect();

    let root = temp_dir("file-chooser-frontend");
    let backend_data = root.join("backend-data");
    let frontend_data = root.join("frontend-data");
    let portal_data = root.join("portal-data");
    let config_home = root.join("config");
    let runtime_dir = root.join("runtime");
    let fixture_dir = root.join("prompter");
    let portal_dir = portal_data.join("xdg-desktop-portal/portals");
    let config_dir = config_home.join("xdg-desktop-portal");
    for directory in [
        &backend_data,
        &frontend_data,
        &runtime_dir,
        &fixture_dir,
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
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.FileChooser=atrium\n",
    )
    .expect("stage frontend routing");
    std::fs::write(
        config_dir.join("atrium-portals.conf"),
        "[preferred]\ndefault=atrium\norg.freedesktop.impl.portal.FileChooser=atrium\n",
    )
    .expect("stage frontend routing compatibility");

    let prompter = fake_chooser(&fixture_dir);
    write_response(
        &fixture_dir,
        1,
        &FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/public-file-chooser.txt")],
            current_filter: None,
            choices: Vec::new(),
        },
    );

    let backend_log = root.join("backend.log");
    let mut backend = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-atrium"));
    backend
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", &backend_data)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("ATRIUM_FILE_CHOOSER_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir)
        .env("RUST_LOG", "debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&backend_log).expect("backend log"),
        ));
    let _backend = KillOnDrop(backend.spawn().expect("spawn Tessera backend"));
    wait_for_name(&conn, PORTAL);

    let frontend_log = root.join("frontend.log");
    let frontend = frontend_binary().expect("checked by frontend_prerequisites");
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

    let chooser = bus::proxy(
        &conn,
        "org.freedesktop.portal.Desktop",
        DESKTOP_PATH,
        "org.freedesktop.portal.FileChooser",
    );
    let token = "atrium_file_chooser_frontend_1";
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

    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert("handle_token".to_owned(), Variant::from(token));
    options.insert("multiple".to_owned(), Variant::from(false));
    let returned: OwnedObjectPath = bus::try_call::<_, OwnedObjectPath>(
        &chooser,
        "OpenFile",
        &("", "Choose through frontend", options),
    )
    .unwrap_or_else(|error| {
        let log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
        panic!("public OpenFile failed: {error}\nfrontend log:\n{log}")
    });
    assert_eq!(returned.as_str(), expected_handle);

    let response = bus::next_signal(&mut responses);
    let (code, results): (u32, HashMap<String, OwnedVariant>) =
        response.body().deserialize().expect("Response body");
    if code != 0 {
        let backend_log = std::fs::read_to_string(&backend_log).unwrap_or_default();
        let frontend_log = std::fs::read_to_string(&frontend_log).unwrap_or_default();
        panic!(
            "public OpenFile returned {code}\nbackend log:\n{backend_log}\nfrontend log:\n{frontend_log}"
        );
    }
    assert_eq!(
        result_uris(&results),
        ["file:///tmp/public-file-chooser.txt"]
    );
    let recorded = read_request(&fixture_dir, 1);
    assert_eq!(recorded.mode, FileChooserMode::OpenFile);
    assert_eq!(recorded.title, "Choose through frontend");

    std::fs::remove_dir_all(root).ok();
}
