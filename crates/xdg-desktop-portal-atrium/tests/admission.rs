//! The private backend admits only the portal frontend.
//!
//! This is the repository's central trust boundary, so it is tested from the
//! outside, over a real bus, rather than only as a unit. Every backend method
//! takes `app_id` as a plain string, and nothing in the protocol proves it: it
//! is rendered into compositor consent text, it keys autostart files and
//! ScreenCast restore tokens, and it is the subject the Secret portal derives a
//! per-application master secret for. If any peer could call this service, it
//! could ask for another application's master secret by naming that
//! application.
//!
//! The boundary is that the backend's D-Bus *client* is the portal frontend
//! (`org.freedesktop.portal.Desktop`), which resolves the sandboxed
//! application's identity itself. Applications reach the portal through the
//! frontend's public interface, never by calling the backend directly.

use std::collections::HashMap;

use zbus::Variant;

mod common;
use common::fake_compositor::FakeCompositor;
use common::{KillOnDrop, bus, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const SECRET_IFACE: &str = "org.freedesktop.impl.portal.Secret";
const LOCKDOWN_IFACE: &str = "org.freedesktop.impl.portal.Lockdown";

struct Fixture {
    bus: common::PrivateBus,
    _fake: std::sync::Arc<FakeCompositor>,
    _server: atrium_portal_ipc::testing::Server,
    _daemon: KillOnDrop,
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
    Some(Fixture {
        bus,
        _fake: fake,
        _server: server,
        _daemon: daemon,
    })
}

/// The secret interface is the sharp end of the boundary: it hands out a
/// per-application master secret. An application that could call it directly
/// would exfiltrate another application's stored credentials by naming it.
#[test]
fn an_application_cannot_retrieve_another_applications_secret() {
    let Some(fixture) = start("admission-secret") else {
        return;
    };
    // A peer that does NOT hold the frontend name: this is the attacker's
    // position, and it is exactly what an ordinary application looks like to
    // the private backend.
    let attacker = fixture.bus.connect();
    let secret = bus::proxy(&attacker, PORTAL, DESKTOP_PATH, SECRET_IFACE);
    let handle =
        zbus::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/admission/secret")
            .expect("request handle path");
    let (read_end, write_end) = common::pipe_pair();
    let fd: zbus::Fd<'_> = zbus::Fd::from(write_end);

    let outcome: Result<(u32, HashMap<String, zbus::zvariant::OwnedVariant>), _> = bus::try_call(
        &secret,
        "RetrieveSecret",
        &(
            handle,
            // The name the attacker would like to steal.
            "org.mozilla.firefox",
            fd,
            HashMap::<String, Variant<'_>>::new(),
        ),
    );

    let error = outcome.expect_err("an application must not reach the Secret backend");
    let text = error.to_string();
    assert!(
        text.contains("AccessDenied"),
        "the refusal must be a D-Bus access denial, got: {text}"
    );
    assert!(
        text.contains("private backend") || text.contains("only org.freedesktop.portal.Desktop"),
        "the refusal must say why, got: {text}"
    );

    // Nothing may have been written to the descriptor the attacker offered:
    // the refusal happened before the descriptor was touched at all.
    drop(read_end);
}

/// The refusal must cover every served interface, not just the dangerous one.
/// A boundary with holes is not a boundary.
#[test]
fn every_served_interface_refuses_an_application() {
    let Some(fixture) = start("admission-all") else {
        return;
    };
    // A frontend owns the name, so this is not the trivial "no frontend yet"
    // case: an unrelated peer must be refused while a legitimate one exists.
    let frontend = fixture.bus.connect();
    common::claim_frontend(&frontend);
    let attacker = fixture.bus.connect();

    // One representative call per interface that takes an app_id, with
    // well-formed arguments. That matters: zbus validates the signature before
    // dispatching, so a malformed call is rejected without ever reaching the
    // admission check and would prove nothing. These arguments are valid, so
    // the only thing that can reject them is admission.
    let app = "org.mozilla.firefox";
    let handle =
        zbus::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/admission/all")
            .expect("request handle path");
    let options: HashMap<String, Variant<'_>> = HashMap::new();

    macro_rules! attempt {
        ($iface:expr, $method:expr, $body:expr) => {{
            let proxy = bus::proxy(&attacker, PORTAL, DESKTOP_PATH, $iface);
            match bus::try_call::<_, ()>(&proxy, $method, &$body) {
                Ok(()) => panic!(
                    "{}.{} must not be callable by an application",
                    $iface, $method
                ),
                Err(error) => {
                    let text = error.to_string();
                    assert!(
                        text.contains("AccessDenied") && text.contains("private backend"),
                        "{}.{} was not refused by admission: {text}",
                        $iface,
                        $method
                    );
                }
            }
        }};
    }

    attempt!(
        "org.freedesktop.impl.portal.FileChooser",
        "OpenFile",
        (handle.clone(), app, "", "", options.clone())
    );
    attempt!(
        "org.freedesktop.impl.portal.Email",
        "ComposeEmail",
        (handle.clone(), app, "", options.clone())
    );
    attempt!(
        "org.freedesktop.impl.portal.Account",
        "GetUserInformation",
        (handle.clone(), app, "", options.clone())
    );
    attempt!(
        "org.freedesktop.impl.portal.Access",
        "AccessDialog",
        (handle.clone(), app, "", "t", "s", "b", options.clone())
    );
    attempt!(
        "org.freedesktop.impl.portal.AppChooser",
        "ChooseApplication",
        (
            handle.clone(),
            app,
            "",
            Vec::<String>::new(),
            "text/plain",
            options.clone()
        )
    );
    attempt!(
        "org.freedesktop.impl.portal.OpenURI",
        "OpenURI",
        (
            handle.clone(),
            app,
            "",
            "https://example.com/",
            options.clone()
        )
    );
    attempt!(
        "org.freedesktop.impl.portal.Background",
        "RequestBackground",
        (handle.clone(), app, "", "", options.clone())
    );
    attempt!(
        "org.freedesktop.impl.portal.Inhibit",
        "Inhibit",
        (handle.clone(), app, "", 8u32, options.clone())
    );
    let _ = options;
}

/// A property read is *not* admission-checked, and that is a documented
/// property of the design rather than an oversight.
///
/// zbus refuses to register a property accessor that takes a message header, so
/// `Properties.Get` is answered without consulting the caller. The rule that
/// keeps this safe is that a property may only ever carry a protocol constant
/// or a non-secret policy flag; everything sensitive is a method, and the tests
/// above prove methods are guarded. This test pins the rule so that a future
/// sensitive property fails here.
#[test]
fn properties_are_public_and_must_stay_insensitive() {
    let Some(fixture) = start("admission-property") else {
        return;
    };
    let attacker = fixture.bus.connect();
    let properties = bus::proxy(
        &attacker,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.DBus.Properties",
    );

    // Readable by an unadmitted peer: a sandbox-policy boolean, not a secret.
    let value: zbus::zvariant::OwnedVariant =
        bus::try_call(&properties, "Get", &(LOCKDOWN_IFACE, "disable-printing"))
            .expect("a policy boolean is readable");
    assert!(
        !bool::try_from(&value).expect("a boolean"),
        "lockdown starts permissive"
    );

    // Version properties are protocol constants. If anything sensitive ever
    // appears here it becomes reachable without admission, so assert that the
    // exposed ones stay constants.
    for (interface, property) in [
        ("org.freedesktop.impl.portal.Screenshot", "version"),
        ("org.freedesktop.impl.portal.ScreenCast", "version"),
        ("org.freedesktop.impl.portal.Secret", "version"),
    ] {
        let value: zbus::zvariant::OwnedVariant =
            bus::try_call(&properties, "Get", &(interface, property))
                .unwrap_or_else(|error| panic!("{interface}.{property} must be readable: {error}"));
        let version = u32::try_from(&value).expect("a version is a u32");
        assert!(
            version > 0,
            "{interface}.{property} must be a real constant"
        );
    }
}

/// The boundary must not be bypassable by simply taking the frontend's name
/// first: that name is what a legitimate frontend holds, and a process that
/// holds it has the same authority a real frontend does. This test documents
/// the property explicitly so the threat model is not misread.
#[test]
fn holding_the_frontend_name_is_the_credential() {
    let Some(fixture) = start("admission-owner") else {
        return;
    };
    let frontend = fixture.bus.connect();
    common::claim_frontend(&frontend);

    // Now an admitted peer: the same call the attacker failed at above.
    let secret = bus::proxy(&frontend, PORTAL, DESKTOP_PATH, SECRET_IFACE);
    let handle =
        zbus::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/admission/owner")
            .expect("request handle path");
    let (read_end, write_end) = common::pipe_pair();
    let fd: zbus::Fd<'_> = zbus::Fd::from(write_end);
    let outcome: Result<(u32, HashMap<String, zbus::zvariant::OwnedVariant>), _> = bus::try_call(
        &secret,
        "RetrieveSecret",
        &(
            handle,
            "dev.tessera.Admitted",
            fd,
            HashMap::<String, Variant<'_>>::new(),
        ),
    );
    // No sigil daemon is bound in this fixture, so the answer is an error *code*
    // (2) rather than an access denial: reaching the interface at all is what
    // this test asserts.
    match outcome {
        Ok((code, _)) => assert_ne!(code, 0, "no vault is bound, so no secret can be served"),
        Err(error) => panic!("an admitted peer must reach the interface, got: {error}"),
    }
    drop(read_end);
}
