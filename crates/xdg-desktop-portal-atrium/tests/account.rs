//! End-to-end Account backend exercise. The real daemon runs on a private
//! session bus and delegates consent to a pipe-compatible Portal prompter;
//! no compositor socket participates.

use std::collections::HashMap;

use atrium_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptRequest, PrompterResponse};
use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

mod common;
use common::{
    KillOnDrop, daemon_command, fake_prompter, private_bus, read_prompter_request, temp_dir,
    wait_for_name, write_prompter_response,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Account";

/// Run one GetUserInformation against a scripted Portal prompt.
fn get_user_information(
    answer: ConfirmResponse,
    with_avatar: bool,
) -> Option<(u32, HashMap<String, OwnedValue>, ConfirmRequest)> {
    let Some(bus) = private_bus() else {
        eprintln!("account: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("account-data");
    let runtime_dir = temp_dir("account-runtime");
    let fixture_dir = temp_dir("account-prompter");
    if with_avatar {
        let avatars = data_dir.join("tessera/avatars");
        std::fs::create_dir_all(&avatars).expect("avatar dir");
        std::fs::write(avatars.join("face.png"), b"png").expect("avatar fixture");
    }
    let prompter = fake_prompter(&fixture_dir);
    write_prompter_response(&fixture_dir, 1, &PrompterResponse::confirm(answer));

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let account = Proxy::new(&conn, PORTAL, DESKTOP_PATH, IFACE).expect("account proxy");
    let mut options: HashMap<String, Value<'_>> = HashMap::new();
    options.insert(
        "reason".to_string(),
        Value::from("Allows your personal information to be included in recipes you share."),
    );
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/acc1")
        .expect("request handle path");
    let (response, results): (u32, HashMap<String, OwnedValue>) = account
        .call(
            "GetUserInformation",
            &(
                handle,
                "dev.tessera.smoke",
                "wayland:account-parent",
                options,
            ),
        )
        .expect("GetUserInformation");
    let PromptRequest::Confirm(request) = read_prompter_request(&fixture_dir, 1) else {
        panic!("Account must issue a confirmation prompt");
    };

    let result = (response, results, request);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&fixture_dir);
    Some(result)
}

#[test]
fn consent_shares_the_identity_with_avatar() {
    let Some((response, results, prompt)) = get_user_information(ConfirmResponse::Confirmed, true)
    else {
        return;
    };
    assert_eq!(response, 0, "an affirmative answer must report success");
    let id = String::try_from(results["id"].clone()).expect("id is a string");
    let name = String::try_from(results["name"].clone()).expect("name is a string");
    assert!(!id.is_empty() && !name.is_empty());
    let image = String::try_from(results["image"].clone()).expect("image is a string");
    assert!(
        image.starts_with("file://") && image.contains("tessera/avatars/face.png"),
        "the avatar URI must point at the canonical location: {image}"
    );
    assert_eq!(prompt.title, "Share Personal Information");
    assert!(prompt.body.contains("dev.tessera.smoke"), "{}", prompt.body);
    assert!(
        prompt.body.contains("recipes"),
        "the reason reaches the dialog: {}",
        prompt.body
    );
    assert_eq!(
        prompt.parent_window.as_deref(),
        Some("wayland:account-parent")
    );
}

#[test]
fn a_declined_consent_releases_nothing() {
    let Some((response, results, _)) = get_user_information(ConfirmResponse::Cancelled, false)
    else {
        return;
    };
    assert_eq!(response, 1, "a declined consent must answer 1");
    assert!(results.is_empty());
}
