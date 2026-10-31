//! End-to-end Account backend exercise. The real daemon runs on a private
//! session bus and delegates consent to compositor chrome (ADR-0026), driven
//! here by a fake compositor speaking the real scoped IPC protocol.

use std::collections::HashMap;

use atrium_portal_ipc::ConfirmPickResult;
use zbus::{ObjectPath, OwnedVariant, Variant};

mod common;
use common::bus;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.Account";

/// One observed consent question.
type Consent = (String, String);

/// Run one GetUserInformation against a compositor that answers `answer`.
fn get_user_information(
    answer: ConfirmPickResult,
    with_avatar: bool,
) -> Option<(u32, HashMap<String, OwnedVariant>, Consent)> {
    let Some(bus) = private_bus() else {
        eprintln!("account: no dbus-daemon, skipping");
        return None;
    };
    let conn = bus.connect();

    let data_dir = temp_dir("account-data");
    let runtime_dir = temp_dir("account-runtime");
    if with_avatar {
        let avatars = data_dir.join("tessera/avatars");
        std::fs::create_dir_all(&avatars).expect("avatar dir");
        std::fs::write(avatars.join("face.png"), b"png").expect("avatar fixture");
    }
    let (server, fake) = FakeCompositor::start(&runtime_dir.join("tessera.sock"))
        .expect("bind the fake compositor IPC");
    fake.consent_answers.lock().unwrap().push(answer);

    let _daemon = KillOnDrop(
        daemon_command(&bus, &data_dir, &runtime_dir)
            .spawn()
            .expect("spawn portal daemon"),
    );
    wait_for_name(&conn, PORTAL);
    // The backend is private: only the frontend may call it.
    common::claim_frontend(&conn);

    let account = bus::proxy(&conn, PORTAL, DESKTOP_PATH, IFACE);
    let mut options: HashMap<String, Variant<'_>> = HashMap::new();
    options.insert(
        "reason".to_string(),
        Variant::from("Allows your personal information to be included in recipes you share."),
    );
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/acc1")
        .expect("request handle path");
    let (response, results): (u32, HashMap<String, OwnedVariant>) = bus::call(
        &account,
        "GetUserInformation",
        &(
            handle,
            "dev.tessera.smoke",
            "wayland:account-parent",
            options,
        ),
    );

    let consent = fake
        .confirms
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("Account must ask the compositor for consent");

    let result = (response, results, consent);
    drop(server);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    Some(result)
}

#[test]
fn consent_shares_the_identity_with_avatar() {
    let Some((response, results, (title, body))) =
        get_user_information(ConfirmPickResult::Confirmed, true)
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
    assert_eq!(title, "Share Personal Information");
    assert!(body.contains("dev.tessera.smoke"), "{body}");
    assert!(
        body.contains("recipes"),
        "the reason reaches the dialog: {body}"
    );
}

#[test]
fn a_declined_consent_releases_nothing() {
    let Some((response, results, _)) = get_user_information(ConfirmPickResult::Cancelled, false)
    else {
        return;
    };
    assert_eq!(response, 1, "a declined consent must answer 1");
    assert!(results.is_empty());
}

#[test]
fn an_unreachable_compositor_fails_closed() {
    let Some((response, results, _)) = ({
        // No fake compositor is bound, so the consent question cannot be asked.
        let Some(bus) = private_bus() else {
            eprintln!("account: no dbus-daemon, skipping");
            return;
        };
        let conn = bus.connect();
        let data_dir = temp_dir("account-noipc-data");
        let runtime_dir = temp_dir("account-noipc-runtime");
        let _daemon = KillOnDrop(
            daemon_command(&bus, &data_dir, &runtime_dir)
                .spawn()
                .expect("spawn portal daemon"),
        );
        wait_for_name(&conn, PORTAL);
        // The backend is private: only the frontend may call it.
        common::claim_frontend(&conn);
        let account = bus::proxy(&conn, PORTAL, DESKTOP_PATH, IFACE);
        let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/acc2")
            .expect("request handle path");
        let (response, results): (u32, HashMap<String, OwnedVariant>) = bus::call(
            &account,
            "GetUserInformation",
            &(
                handle,
                "dev.tessera.smoke",
                "",
                HashMap::<String, Variant<'_>>::new(),
            ),
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&runtime_dir);
        Some((response, results, (String::new(), String::new())))
    }) else {
        return;
    };
    assert_eq!(
        response, 2,
        "an unaskable consent must report failure, never success"
    );
    assert!(results.is_empty());
}
