//! End-to-end password-unlock tests for the native Secret portal. A real
//! backend delegates masked input to a Portal-owned prompter and writes the
//! derived portal secret to a client-provided pipe. No Tessera IPC is involved.

use std::time::Duration;

use atrium_portal_prompter::{PromptRequest, PrompterResponse, SecretRequest, SecretResponse};
use argon2::Argon2;
use argon2::password_hash::SaltString;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use zbus::blocking::Proxy;
use zbus::zvariant::{Fd, ObjectPath, Value};

mod common;
use common::{
    KillOnDrop, daemon_command, fake_prompter, pipe_pair, private_bus, read_all_with_timeout,
    read_prompter_request, temp_dir, wait_for_name, write_prompter_response,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.atrium";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const PASSWORD: &str = "hunter2";
/// Fixed test salt ("somesalt" in base64).
const SALT_B64: &str = "c29tZXNhbHQ";

fn write_password_vault(secrets_dir: &std::path::Path) {
    std::fs::create_dir_all(secrets_dir).expect("create secrets dir");
    std::fs::write(secrets_dir.join("vault.salt"), SALT_B64).expect("write salt");

    let salt = SaltString::from_b64(SALT_B64).expect("parse salt");
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(PASSWORD.as_bytes(), salt.as_str().as_bytes(), &mut key)
        .expect("derive fixture key");

    let data = serde_json::json!({
        "collections": [{
            "label": "Login",
            "id": "login",
            "items": [{
                "id": "i01",
                "label": "token",
                "attributes": { "k": "v" },
                "secret": [115, 51, 99, 114, 51, 116]
            }]
        }]
    });
    let plaintext = serde_json::to_vec(&data).expect("serialize fixture");
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).expect("encrypt");
    let mut file = nonce.to_vec();
    file.extend_from_slice(&ciphertext);
    let vault_path = secrets_dir.join("vault.enc");
    std::fs::write(&vault_path, file).expect("write vault");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o600))
        .expect("make vault private");
}

fn expected_portal_secret_password_mode() -> [u8; 32] {
    let salt = SaltString::from_b64(SALT_B64).expect("parse salt");
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(PASSWORD.as_bytes(), salt.as_str().as_bytes(), &mut key)
        .expect("derive fixture key");
    let hk = Hkdf::<Sha256>::new(None, &key);
    let mut out = [0u8; 32];
    hk.expand(b"atrium.portal.Secret/v1\0dev.tessera.locked", &mut out)
        .expect("expand");
    out
}

/// Run RetrieveSecret against a locked vault. `None` means dbus-daemon is
/// unavailable and the caller should skip.
fn retrieve_secret_while_locked(answer: SecretResponse) -> Option<(u32, Vec<u8>, SecretRequest)> {
    let bus = private_bus()?;
    let conn = bus.connect();
    let data_dir = temp_dir("secret-data");
    write_password_vault(&data_dir.join("aegis/secrets"));
    let runtime_dir = temp_dir("secret-runtime");
    let fixture_dir = temp_dir("secret-prompter");
    let prompter = fake_prompter(&fixture_dir);
    write_prompter_response(&fixture_dir, 1, &PrompterResponse::secret(answer));

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("ATRIUM_PORTAL_PROMPTER", &prompter)
        .env("ATRIUM_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let portal = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    )
    .expect("portal proxy");
    let (read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/locked")
        .expect("request handle path");
    let (response, _): (
        u32,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) = portal
        .call(
            "RetrieveSecret",
            &(
                handle,
                "dev.tessera.locked",
                fd,
                std::collections::HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("RetrieveSecret call");
    let bytes = read_all_with_timeout(read_end, Duration::from_secs(5));
    let PromptRequest::Secret(request) = read_prompter_request(&fixture_dir, 1) else {
        panic!("Secret must issue a masked-input prompt");
    };

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let _ = std::fs::remove_dir_all(&fixture_dir);
    Some((response, bytes, request))
}

#[test]
fn retrieve_secret_while_locked_prompts_then_delivers() {
    let Some((response, bytes, prompt)) = retrieve_secret_while_locked(SecretResponse::Secret {
        value: PASSWORD.to_string(),
    }) else {
        eprintln!("retrieve secret locked: no dbus-daemon, skipping");
        return;
    };
    assert_eq!(response, 0, "RetrieveSecret must succeed after unlock");
    assert_eq!(bytes.as_slice(), &expected_portal_secret_password_mode());
    assert_eq!(prompt.title, "Unlock Keyring");
    assert!(
        prompt
            .reason
            .is_some_and(|reason| reason.contains("locked"))
    );
}

#[test]
fn retrieve_secret_dismissed_reports_cancelled() {
    let Some((response, bytes, _)) = retrieve_secret_while_locked(SecretResponse::Cancelled) else {
        eprintln!("retrieve secret dismissed: no dbus-daemon, skipping");
        return;
    };
    assert_eq!(response, 1, "a dismissed prompt must report cancelled");
    assert!(bytes.is_empty(), "dismissal must not write secret bytes");
}
