//! A recording fake compositor for portal tests.
//!
//! ADR-0026 moved every interactive prompt except FileChooser onto compositor
//! chrome, so a portal test that exercises consent, app choice, source choice,
//! or notifications no longer scripts a prompter process: it scripts this
//! server. The fake speaks the real scoped IPC protocol through
//! `atrium_portal_ipc::testing::Server`, so the daemon under test runs its
//! production client code.
//!
//! Each prompt family records what the daemon asked and answers from a queue
//! the test fills. An empty queue answers the type's default (confirm accepted,
//! first offered application, whole desktop) so tests that do not care about a
//! particular prompt need no setup.

use std::path::Path;
use std::sync::{Arc, Mutex};

use atrium_portal_ipc::testing::{Handler, Server};
use atrium_portal_ipc::{AppPickResult, Command, ConfirmPickResult, PickKind, PickResult};

/// One recorded consent question.
pub type Consent = (String, String);

/// One recorded application pick: the offered ids plus the context line.
pub type AppPick = (Vec<String>, Option<String>, Option<String>);

/// A compositor fake that records prompts and notifications.
#[derive(Debug, Default)]
pub struct FakeCompositor {
    /// Every `PickConfirm` received, in order.
    pub confirms: Mutex<Vec<Consent>>,
    /// Every `PickApp` received, in order.
    pub app_picks: Mutex<Vec<AppPick>>,
    /// Every `PickTarget` received, in order.
    pub picks: Mutex<Vec<PickKind>>,
    /// Every `Do` command received, in order.
    pub commands: Mutex<Vec<Command>>,
    /// Queued consent answers; the default is `Confirmed`.
    pub consent_answers: Mutex<Vec<ConfirmPickResult>>,
    /// Queued application answers; the default is the first offered id.
    pub app_answers: Mutex<Vec<AppPickResult>>,
    /// Queued target answers; the default is the whole desktop.
    pub target_answers: Mutex<Vec<PickResult>>,
    /// Refuse every prompt when set (an unreachable/wedged compositor).
    pub fail_prompts: Mutex<bool>,
}

impl FakeCompositor {
    /// Start the fake on `socket`, returning the server and the shared handle.
    ///
    /// # Errors
    ///
    /// Propagates a bind/serve failure from the test server.
    pub fn start(
        socket: &Path,
    ) -> Result<(Server, Arc<FakeCompositor>), Box<dyn std::error::Error>> {
        let fake = Arc::new(FakeCompositor::default());
        let server = Server::start(socket, Arc::clone(&fake))?;
        Ok((server, fake))
    }

    fn failing(&self) -> bool {
        *self.fail_prompts.lock().unwrap()
    }
}

impl Handler for FakeCompositor {
    fn pick_confirm(
        &self,
        _conn_id: u64,
        title: String,
        body: String,
        _accept_label: Option<String>,
    ) -> Result<ConfirmPickResult, String> {
        if self.failing() {
            return Err("simulated compositor prompt failure".into());
        }
        self.confirms.lock().unwrap().push((title, body));
        Ok(self
            .consent_answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(ConfirmPickResult::Confirmed))
    }

    fn pick_app(
        &self,
        _conn_id: u64,
        choices: Vec<String>,
        subject: Option<String>,
        last_choice: Option<String>,
    ) -> Result<AppPickResult, String> {
        if self.failing() {
            return Err("simulated compositor prompt failure".into());
        }
        self.app_picks
            .lock()
            .unwrap()
            .push((choices.clone(), subject, last_choice));
        if let Some(answer) = self.app_answers.lock().unwrap().pop() {
            return Ok(answer);
        }
        choices
            .first()
            .cloned()
            .map(|id| AppPickResult::App { id })
            .ok_or_else(|| "no application was offered".to_string())
    }

    fn pick_target(&self, _conn_id: u64, kind: PickKind) -> Result<PickResult, String> {
        if self.failing() {
            return Err("simulated compositor prompt failure".into());
        }
        self.picks.lock().unwrap().push(kind);
        Ok(self
            .target_answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(PickResult::Output { connector: None }))
    }

    fn command(&self, _conn_id: u64, cmd: Command) -> Result<(), String> {
        if self.failing() {
            return Err("simulated compositor command failure".into());
        }
        self.commands.lock().unwrap().push(cmd);
        Ok(())
    }
}
