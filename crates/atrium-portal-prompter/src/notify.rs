//! The notification daemon's stream protocol (contract between the portal
//! backend and `atrium-portal-prompter --notification-daemon`).
//!
//! Unlike the one-shot prompt contract, notifications are asynchronous and
//! long-lived, so the daemon speaks newline-delimited JSON on both pipes:
//! [`CommandFrame`]s on stdin, [`EventFrame`]s on stdout. Every frame
//! carries the stream version; a version mismatch or an oversized line is
//! rejected, never panicked on.
//!
//! Cards are keyed by `(app_id, id)` — the app id rides every command and
//! event so two applications reusing an id never collide (the portal spec
//! namespaces ids per application).

use std::io;
use std::io::{BufRead as _, Read as _};

/// Version of the notification stream protocol. Version 2 adds the
/// `set_appearance` command (compositor desktop preferences for the
/// cards' look, pushed whenever settings change).
pub const NOTIFY_STREAM_VERSION: u32 = 2;

/// One JSON line past this size is rejected.
pub const MAX_NOTIFY_LINE_BYTES: usize = 64 * 1024;
/// Text bounds enforced on decode (the backend is stricter).
pub const MAX_APP_ID_BYTES: usize = 255;
pub const MAX_NOTIFICATION_ID_BYTES: usize = 255;
pub const MAX_TITLE_BYTES: usize = 1024;
pub const MAX_BODY_BYTES: usize = 4 * 1024;
pub const MAX_ACTION_BYTES: usize = 255;
pub const MAX_LABEL_BYTES: usize = 256;
pub const MAX_BUTTONS: usize = 8;
/// Live notification cards the daemon holds at once; a new card past the
/// cap evicts the oldest.
pub const MAX_LIVE_NOTIFICATIONS: usize = 64;

/// The notification priority, deciding the default auto-dismiss timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

/// One action button on a notification card.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifyButton {
    pub action: String,
    pub label: String,
}

/// One notification card's content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notification {
    pub app_id: String,
    pub id: String,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub default_action: Option<String>,
    pub buttons: Vec<NotifyButton>,
    /// Seconds after which the daemon auto-dismisses the card; `None`
    /// means it persists until the user or the application closes it.
    pub expire_hint: Option<u64>,
}

impl Notification {
    /// Reject malformed values before a card is shown.
    pub fn validate(&self) -> Result<(), String> {
        bounded("app id", &self.app_id, MAX_APP_ID_BYTES)?;
        bounded("notification id", &self.id, MAX_NOTIFICATION_ID_BYTES)?;
        if self.title.len() > MAX_TITLE_BYTES
            || self.body.len() > MAX_BODY_BYTES
            || self.title.contains('\0')
            || self.body.contains('\0')
        {
            return Err("notification title/body is oversized or contains NUL".into());
        }
        if self.title.trim().is_empty() && self.body.trim().is_empty() {
            return Err("notification has neither a title nor a body".into());
        }
        if let Some(action) = self.default_action.as_deref() {
            bounded("default action", action, MAX_ACTION_BYTES)?;
        }
        if self.buttons.len() > MAX_BUTTONS {
            return Err(format!("notification has more than {MAX_BUTTONS} buttons"));
        }
        for button in &self.buttons {
            bounded("button action", &button.action, MAX_ACTION_BYTES)?;
            bounded("button label", &button.label, MAX_LABEL_BYTES)?;
        }
        Ok(())
    }
}

fn bounded(name: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit || value.contains('\0') {
        return Err(format!("{name} is empty, oversized, or contains NUL"));
    }
    Ok(())
}

/// One command from the backend to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyCommand {
    Notify(Notification),
    Close {
        app_id: String,
        id: String,
    },
    /// Replace the appearance snapshot the cards render with (stream
    /// version 2). Sent once after the daemon starts and again whenever
    /// the compositor's desktop preferences change.
    SetAppearance {
        appearance: crate::PromptAppearance,
    },
    Shutdown,
}

/// One event from the daemon to the backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyEvent {
    /// A button or the card's default action fired; the card is closed.
    ActionInvoked {
        app_id: String,
        id: String,
        action: String,
    },
    /// The card closed for any reason: timeout, dismissal, a `Close`
    /// command, or eviction past the live-card cap.
    Closed { app_id: String, id: String },
}

/// The versioned command envelope (one per stdin line).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandFrame {
    pub v: u32,
    pub cmd: NotifyCommand,
}

impl CommandFrame {
    #[must_use]
    pub fn new(cmd: NotifyCommand) -> Self {
        Self {
            v: NOTIFY_STREAM_VERSION,
            cmd,
        }
    }

    /// Encode one line (with the trailing newline).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Decode and validate one line.
    pub fn decode(line: &[u8]) -> Result<Self, String> {
        if line.is_empty() || line.len() > MAX_NOTIFY_LINE_BYTES {
            return Err("command line is empty or oversized".into());
        }
        let frame: Self = serde_json::from_slice(line)
            .map_err(|error| format!("invalid command JSON: {error}"))?;
        if frame.v != NOTIFY_STREAM_VERSION {
            return Err(format!(
                "unsupported notification stream version {}; expected {}",
                frame.v, NOTIFY_STREAM_VERSION
            ));
        }
        match &frame.cmd {
            NotifyCommand::Notify(notification) => notification.validate()?,
            NotifyCommand::Close { app_id, id } => {
                bounded("app id", app_id, MAX_APP_ID_BYTES)?;
                bounded("notification id", id, MAX_NOTIFICATION_ID_BYTES)?;
            }
            NotifyCommand::SetAppearance { appearance } => appearance.validate()?,
            NotifyCommand::Shutdown => {}
        }
        Ok(frame)
    }
}

/// The versioned event envelope (one per stdout line).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventFrame {
    pub v: u32,
    pub event: NotifyEvent,
}

impl EventFrame {
    #[must_use]
    pub fn new(event: NotifyEvent) -> Self {
        Self {
            v: NOTIFY_STREAM_VERSION,
            event,
        }
    }

    /// Encode one line (with the trailing newline).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Decode and validate one line.
    pub fn decode(line: &[u8]) -> Result<Self, String> {
        if line.is_empty() || line.len() > MAX_NOTIFY_LINE_BYTES {
            return Err("event line is empty or oversized".into());
        }
        let frame: Self =
            serde_json::from_slice(line).map_err(|error| format!("invalid event JSON: {error}"))?;
        if frame.v != NOTIFY_STREAM_VERSION {
            return Err(format!(
                "unsupported notification stream version {}; expected {}",
                frame.v, NOTIFY_STREAM_VERSION
            ));
        }
        let (app_id, id) = match &frame.event {
            NotifyEvent::ActionInvoked { app_id, id, action } => {
                bounded("action", action, MAX_ACTION_BYTES)?;
                (app_id, id)
            }
            NotifyEvent::Closed { app_id, id } => (app_id, id),
        };
        bounded("app id", app_id, MAX_APP_ID_BYTES)?;
        bounded("notification id", id, MAX_NOTIFICATION_ID_BYTES)?;
        Ok(frame)
    }
}

/// Read one bounded line from a stream. `Ok(None)` is EOF; an oversized
/// line is drained to its newline and reported as `Err`. Both the daemon
/// (stdin) and the backend (the daemon's stdout) read their side through
/// this so a hostile or buggy peer can never grow memory unboundedly.
pub fn read_line_bounded(
    reader: &mut impl io::BufRead,
) -> io::Result<Option<Result<Vec<u8>, String>>> {
    let mut line = Vec::new();
    // One byte past the cap tells an oversized line apart from an
    // exactly-at-cap one.
    let read = reader
        .by_ref()
        .take(MAX_NOTIFY_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && line.len() > MAX_NOTIFY_LINE_BYTES {
        // Oversized: drain the rest of the line (bounded) and reject it.
        let mut rest = Vec::new();
        reader
            .by_ref()
            .take((MAX_NOTIFY_LINE_BYTES * 4) as u64)
            .read_until(b'\n', &mut rest)?;
        return Ok(Some(Err("stream line is oversized".to_owned())));
    }
    Ok(Some(Ok(line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification() -> Notification {
        Notification {
            app_id: "dev.tessera.Test".into(),
            id: "msg-1".into(),
            title: "Hello".into(),
            body: "World".into(),
            priority: Priority::Normal,
            default_action: Some("open".into()),
            buttons: vec![NotifyButton {
                action: "reply".into(),
                label: "Reply".into(),
            }],
            expire_hint: Some(10),
        }
    }

    #[test]
    fn command_frames_round_trip() {
        let set_appearance = NotifyCommand::SetAppearance {
            appearance: crate::PromptAppearance {
                color_scheme: crate::PromptColorScheme::Dark,
                accent_color: None,
                high_contrast: false,
                reduced_motion: true,
            },
        };
        for cmd in [
            NotifyCommand::Notify(notification()),
            NotifyCommand::Close {
                app_id: "dev.tessera.Test".into(),
                id: "msg-1".into(),
            },
            set_appearance,
            NotifyCommand::Shutdown,
        ] {
            let line = CommandFrame::new(cmd.clone()).encode().unwrap();
            assert!(line.ends_with(b"\n"));
            let decoded = CommandFrame::decode(&line).unwrap();
            assert_eq!(decoded.cmd, cmd);
            assert_eq!(decoded.v, NOTIFY_STREAM_VERSION);
        }
    }

    #[test]
    fn set_appearance_command_decodes_from_literal_json() {
        // The literal shape the backend writes, pinned so the two sides
        // cannot drift apart silently.
        let line = concat!(
            r#"{"v":2,"cmd":{"kind":"set_appearance","appearance":"#,
            r#"{"color_scheme":"dark","accent_color":{"red":1,"green":2,"blue":3},"#,
            r#""high_contrast":false,"reduced_motion":false}}}"#,
            "\n"
        );
        let frame = CommandFrame::decode(line.as_bytes()).unwrap();
        match frame.cmd {
            NotifyCommand::SetAppearance { appearance } => {
                assert_eq!(appearance.color_scheme, crate::PromptColorScheme::Dark);
                assert_eq!(
                    appearance.accent_color,
                    Some(crate::PromptAccent {
                        red: 1,
                        green: 2,
                        blue: 3
                    })
                );
            }
            other => panic!("expected SetAppearance, got {other:?}"),
        }
    }

    #[test]
    fn event_frames_round_trip() {
        for event in [
            NotifyEvent::ActionInvoked {
                app_id: "dev.tessera.Test".into(),
                id: "msg-1".into(),
                action: "reply".into(),
            },
            NotifyEvent::Closed {
                app_id: "dev.tessera.Test".into(),
                id: "msg-1".into(),
            },
        ] {
            let line = EventFrame::new(event.clone()).encode().unwrap();
            assert_eq!(EventFrame::decode(&line).unwrap().event, event);
        }
    }

    #[test]
    fn malformed_and_oversized_frames_are_rejected() {
        assert!(CommandFrame::decode(b"").is_err());
        assert!(CommandFrame::decode(b"not json\n").is_err());
        assert!(CommandFrame::decode(b"{\"v\":3,\"cmd\":{\"kind\":\"shutdown\"}}\n").is_err());
        assert!(CommandFrame::decode(b"{\"v\":2,\"cmd\":{\"kind\":\"explode\"}}\n").is_err());
        // A black-transparent accent is rejected on the stream too.
        assert!(
            CommandFrame::decode(
                concat!(
                    r#"{"v":2,"cmd":{"kind":"set_appearance","appearance":"#,
                    r#"{"color_scheme":"system","accent_color":{"red":0,"green":0,"blue":0},"#,
                    r#""high_contrast":false,"reduced_motion":false}}}"#,
                    "\n"
                )
                .as_bytes()
            )
            .is_err()
        );

        let mut too_many_buttons = notification();
        too_many_buttons.buttons = vec![
            NotifyButton {
                action: "a".into(),
                label: "b".into(),
            };
            MAX_BUTTONS + 1
        ];
        let line = CommandFrame::new(NotifyCommand::Notify(too_many_buttons))
            .encode()
            .unwrap();
        assert!(CommandFrame::decode(&line).is_err());

        let mut oversized_title = notification();
        oversized_title.title = "x".repeat(MAX_TITLE_BYTES + 1);
        let line = CommandFrame::new(NotifyCommand::Notify(oversized_title))
            .encode()
            .unwrap();
        assert!(CommandFrame::decode(&line).is_err());

        let mut empty = notification();
        empty.title.clear();
        empty.body.clear();
        let line = CommandFrame::new(NotifyCommand::Notify(empty))
            .encode()
            .unwrap();
        assert!(CommandFrame::decode(&line).is_err());
    }

    #[test]
    fn bounded_lines_report_oversize_and_eof() {
        let mut stream = b"short\n".to_vec();
        stream.extend_from_slice(&vec![b'x'; MAX_NOTIFY_LINE_BYTES + 10]);
        stream.push(b'\n');
        let mut reader = io::BufReader::new(stream.as_slice());

        let first = read_line_bounded(&mut reader).unwrap().unwrap().unwrap();
        assert_eq!(first, b"short\n");
        let second = read_line_bounded(&mut reader).unwrap().unwrap();
        assert!(second.is_err());
        assert!(read_line_bounded(&mut reader).unwrap().is_none());
    }
}
