//! Notification daemon mode (`--notification-daemon`): a long-lived
//! process rendering notification cards.
//!
//! Window shape is a deliberate compromise. The iris/lens stack offers only
//! plain decorated toplevels — there is no borderless, always-on-top, or
//! layer-shell surface (checked `iris/app.h`/`window.h` and the lens band
//! system, which is intra-window z-order only). So notifications appear in
//! one small "Notifications" window that stacks the cards newest-first;
//! the window opens when the first card arrives and closes when the last
//! card goes away (the daemon process stays alive on its stdin command
//! stream and reopens the window for the next card — sequential
//! `Application::run` calls are supported: the Wayland backend keeps its
//! state in a run-local struct). Closing the window itself dismisses every
//! card (each reports `Closed`).
//!
//! Cards auto-dismiss per the backend-computed `expire_hint` (low/normal
//! priority), high/urgent persist. Clicking a card body fires its default
//! action; buttons fire their action; either reports `ActionInvoked` and
//! closes the card. Escape dismisses the newest expiring card. Every close
//! path — timeout, dismiss, `Close` command, cap eviction — reports
//! `Closed`.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atrium_portal_prompter::PromptAppearance;
use atrium_portal_prompter::notify::{
    CommandFrame, EventFrame, MAX_LIVE_NOTIFICATIONS, Notification, NotifyCommand, NotifyEvent,
    read_line_bounded,
};
use lens::{Frame, Input};

use super::style::{self, metrics};
use super::{
    WindowChrome, close_window, escape_pressed, run_window_with_chrome, truncate_to_width,
};
use crate::wire::Wire;

/// One live notification card.
struct Card {
    notification: Notification,
    /// The auto-dismiss instant; `None` persists.
    deadline: Option<Instant>,
}

struct State {
    /// Newest first.
    cards: Vec<Card>,
    /// The resolved appearance the cards render with. Re-resolved from
    /// the latest `SetAppearance` command (stream v2); the platform
    /// fallback applies until the backend pushes one.
    appearance: style::ThemeInput,
    /// The last pushed appearance snapshot, kept for the window chrome
    /// (the reduced-motion flag rides the snapshot, not the palette).
    pending_appearance: Option<PromptAppearance>,
    commands: mpsc::Receiver<NotifyCommand>,
    events: mpsc::Sender<NotifyEvent>,
    shutdown: bool,
}

impl State {
    fn emit(&self, event: NotifyEvent) {
        // The writer thread disappearing means stdout is gone; nothing
        // sensible remains but to keep running.
        let _ = self.events.send(event);
    }

    /// Remove the card at `index`, reporting the close.
    fn dismiss(&mut self, index: usize) {
        let card = self.cards.remove(index);
        self.emit(NotifyEvent::Closed {
            app_id: card.notification.app_id,
            id: card.notification.id,
        });
    }

    /// The newest card the user may dismiss with Escape: low/normal cards
    /// (the ones with a deadline). High/urgent cards need an explicit
    /// click.
    fn newest_expiring(&self) -> Option<usize> {
        self.cards.iter().position(|card| card.deadline.is_some())
    }
}

/// Apply one daemon command to the card list.
fn apply_command(state: &mut State, cmd: NotifyCommand) {
    match cmd {
        NotifyCommand::Shutdown => state.shutdown = true,
        NotifyCommand::SetAppearance { appearance } => {
            state.appearance = style::ThemeInput::resolve(Some(&appearance));
            state.pending_appearance = Some(appearance);
        }
        NotifyCommand::Close { app_id, id } => {
            if let Some(index) = state
                .cards
                .iter()
                .position(|card| card.notification.app_id == app_id && card.notification.id == id)
            {
                state.dismiss(index);
            }
        }
        NotifyCommand::Notify(notification) => {
            let deadline = notification
                .expire_hint
                .map(|seconds| Instant::now() + Duration::from_secs(seconds));
            let key = (notification.app_id.clone(), notification.id.clone());
            if let Some(existing) = state
                .cards
                .iter_mut()
                .find(|card| card.notification.app_id == key.0 && card.notification.id == key.1)
            {
                // An id reuse updates the card in place (spec).
                existing.notification = notification;
                existing.deadline = deadline;
            } else {
                state.cards.insert(
                    0,
                    Card {
                        notification,
                        deadline,
                    },
                );
                if state.cards.len() > MAX_LIVE_NOTIFICATIONS {
                    // Past the cap the oldest card goes.
                    state.dismiss(state.cards.len() - 1);
                }
            }
        }
    }
}

/// Remove cards past their deadline, reporting each close. Returns whether
/// any live card still has a deadline (so the caller keeps frames coming).
fn expire_cards(state: &mut State) -> bool {
    let now = Instant::now();
    let mut index = state.cards.len();
    while index > 0 {
        index -= 1;
        if state.cards[index]
            .deadline
            .is_some_and(|deadline| deadline <= now)
        {
            state.dismiss(index);
        }
    }
    state.cards.iter().any(|card| card.deadline.is_some())
}

pub fn run_daemon(wire: Wire) -> ExitCode {
    let (commands, command_rx) = mpsc::channel::<NotifyCommand>();
    let (events, event_rx) = mpsc::channel::<NotifyEvent>();
    spawn_reader(commands);
    let writer = spawn_writer(event_rx, wire);

    let mut state = State {
        cards: Vec::new(),
        appearance: style::ThemeInput::resolve(None),
        pending_appearance: None,
        commands: command_rx,
        events,
        shutdown: false,
    };

    loop {
        // With nothing on screen, block until a command arrives.
        if state.cards.is_empty() && !state.shutdown {
            match state.commands.recv() {
                Ok(cmd) => apply_command(&mut state, cmd),
                // The reader thread is gone (stdin broke): nothing more
                // will ever arrive.
                Err(_) => break,
            }
        }
        if state.shutdown {
            break;
        }
        if state.cards.is_empty() {
            continue;
        }
        state = match run_window_with_chrome(
            "Notifications",
            WindowChrome::resizable((420, 520), (360, 240), state.pending_appearance.as_ref()),
            state,
            build,
        ) {
            Ok(state) => state,
            Err(error) => {
                log::error!("prompter: notification window failed: {error}");
                return ExitCode::FAILURE;
            }
        };
        if !state.shutdown && !state.cards.is_empty() {
            // The window's own close button: dismiss everything it showed.
            log::info!("prompter: notification window closed; dismissing all cards");
            while !state.cards.is_empty() {
                state.dismiss(state.cards.len() - 1);
            }
        }
    }
    drop(state);
    let _ = writer.join();
    ExitCode::SUCCESS
}

/// The stdin reader: bounded lines → validated commands → the channel,
/// waking the window loop after each. EOF or a `Shutdown` command ends it.
fn spawn_reader(commands: mpsc::Sender<NotifyCommand>) {
    let _ = std::thread::Builder::new()
        .name("tessera-notify-stdin".to_owned())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            loop {
                match read_line_bounded(&mut reader) {
                    Ok(None) => break,
                    Ok(Some(Err(error))) => {
                        log::warn!("prompter: {error}");
                        continue;
                    }
                    Ok(Some(Ok(line))) => match CommandFrame::decode(&line) {
                        Ok(frame) => {
                            let shutdown = matches!(frame.cmd, NotifyCommand::Shutdown);
                            if commands.send(frame.cmd).is_err() {
                                return;
                            }
                            super::wake_main_thread();
                            if shutdown {
                                return;
                            }
                        }
                        Err(error) => log::warn!("prompter: rejecting command: {error}"),
                    },
                    Err(error) => {
                        log::warn!("prompter: command stream failed: {error}");
                        break;
                    }
                }
            }
            // stdin EOF: the backend is done (or gone); shut down.
            let _ = commands.send(NotifyCommand::Shutdown);
            super::wake_main_thread();
        });
}

/// The protocol writer: one event per line on the private wire, until the
/// channel closes.
fn spawn_writer(events: mpsc::Receiver<NotifyEvent>, wire: Wire) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tessera-notify-stdout".to_owned())
        .spawn(move || {
            let mut out = std::io::BufWriter::new(wire);
            while let Ok(event) = events.recv() {
                let Ok(line) = EventFrame::new(event).encode() else {
                    continue;
                };
                if out.write_all(&line).and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
        })
        .expect("spawn notification writer thread")
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme_for(&state.appearance));

    // Commands first: the reader thread's wake guarantees this frame runs
    // soon after a command lands.
    while let Ok(cmd) = state.commands.try_recv() {
        apply_command(state, cmd);
    }
    let timed = expire_cards(state);
    if state.shutdown || state.cards.is_empty() {
        close_window();
        return;
    }
    if timed {
        // Keep frames coming so deadlines are observed on time.
        iris::request_animation_frame();
    }
    if escape_pressed(input)
        && let Some(index) = state.newest_expiring()
    {
        state.dismiss(index);
        if state.cards.is_empty() {
            close_window();
        }
        return;
    }

    f.scroll("notifications", |f| {
        f.col()
            .gap(metrics::SPACE_S)
            .pad(metrics::SPACE_M)
            .show_flat(|f| {
                for index in 0..state.cards.len() {
                    card(state, f, index);
                }
            });
    });
}

/// One card: a pressable body (click fires the default action) over a
/// button row (action buttons, then Dismiss). The two rows are siblings —
/// lens hit-testing does not nest pressables.
fn card(state: &mut State, f: &mut Frame, index: usize) {
    let notification = state.cards[index].notification.clone();
    let key = format!("{}:{}", notification.app_id, notification.id);
    let palette = state.appearance.palette();

    let width = 420.0 - 2.0 * (metrics::SPACE_M + metrics::SPACE_S);
    let (response, ()) = f
        .row()
        .gap(metrics::SPACE_XXS)
        .pad(metrics::SPACE_S)
        .bg(palette.field)
        .rounded(metrics::RADIUS)
        .id(&format!("card-{key}"))
        .show(|f| {
            f.col().gap(metrics::SPACE_XXS).show_flat(|f| {
                if !notification.title.is_empty() {
                    f.push_style(style::title_style());
                    f.label(&truncate_to_width(f, &notification.title, width));
                    f.pop_style();
                }
                if !notification.body.is_empty() {
                    f.push_style(style::muted_style_for(&palette));
                    f.label_wrapped(&notification.body, width.max(120.0));
                    f.pop_style();
                }
            });
        });
    if response.clicked
        && let Some(action) = notification.default_action.clone()
    {
        state.emit(NotifyEvent::ActionInvoked {
            app_id: notification.app_id.clone(),
            id: notification.id.clone(),
            action,
        });
        state.dismiss(index);
        return;
    }

    f.row().gap(metrics::SPACE_S).items_center().show_flat(|f| {
        for button in &notification.buttons {
            if f.button(&button.label) {
                state.emit(NotifyEvent::ActionInvoked {
                    app_id: notification.app_id.clone(),
                    id: notification.id.clone(),
                    action: button.action.clone(),
                });
                state.dismiss(index);
                return;
            }
        }
        f.flex(1.0);
        f.spacer(0.0);
        f.push_style(style::secondary_button_style_for(&palette));
        let dismiss = f.button("Dismiss");
        f.pop_style();
        if dismiss {
            state.dismiss(index);
        }
    });
}

#[cfg(test)]
mod tests {
    use atrium_portal_prompter::notify::{NotifyButton, Priority};

    use super::*;

    fn fixture_state() -> State {
        let (_commands, command_rx) = mpsc::channel();
        let (events, _event_rx) = mpsc::channel();
        State {
            cards: Vec::new(),
            appearance: style::ThemeInput::resolve(None),
            pending_appearance: None,
            commands: command_rx,
            events,
            shutdown: false,
        }
    }

    fn notification(id: &str, expire_hint: Option<u64>) -> Notification {
        Notification {
            app_id: "dev.tessera.Test".into(),
            id: id.into(),
            title: format!("Title {id}"),
            body: String::new(),
            priority: Priority::Normal,
            default_action: None,
            buttons: Vec::new(),
            expire_hint,
        }
    }

    #[test]
    fn notify_adds_updates_and_evicts() {
        let mut state = fixture_state();
        apply_command(&mut state, NotifyCommand::Notify(notification("a", None)));
        apply_command(&mut state, NotifyCommand::Notify(notification("b", None)));
        // Newest first.
        assert_eq!(state.cards[0].notification.id, "b");
        assert_eq!(state.cards[1].notification.id, "a");

        // Reusing the id updates in place without moving the card.
        let mut updated = notification("a", Some(30));
        updated.title = "Updated".into();
        apply_command(&mut state, NotifyCommand::Notify(updated));
        assert_eq!(state.cards.len(), 2);
        assert_eq!(state.cards[1].notification.title, "Updated");
        assert!(state.cards[1].deadline.is_some());

        // Past the cap, the oldest card is evicted with a Closed event.
        for index in 0..MAX_LIVE_NOTIFICATIONS {
            apply_command(
                &mut state,
                NotifyCommand::Notify(notification(&format!("n{index}"), None)),
            );
        }
        assert_eq!(state.cards.len(), MAX_LIVE_NOTIFICATIONS);
        assert!(state.cards.iter().all(|card| card.notification.id != "b"));
    }

    #[test]
    fn close_and_expiry_report_closed_events() {
        let (events, event_rx) = mpsc::channel();
        let mut state = fixture_state();
        state.events = events;
        apply_command(&mut state, NotifyCommand::Notify(notification("a", None)));
        apply_command(
            &mut state,
            NotifyCommand::Close {
                app_id: "dev.tessera.Test".into(),
                id: "a".into(),
            },
        );
        assert!(state.cards.is_empty());
        assert_eq!(
            event_rx.try_recv().unwrap(),
            NotifyEvent::Closed {
                app_id: "dev.tessera.Test".into(),
                id: "a".into(),
            }
        );
        // Closing an unknown id is a silent no-op.
        apply_command(
            &mut state,
            NotifyCommand::Close {
                app_id: "dev.tessera.Test".into(),
                id: "ghost".into(),
            },
        );
        assert!(event_rx.try_recv().is_err());

        // A zero-second deadline expires on the next pass.
        apply_command(
            &mut state,
            NotifyCommand::Notify(notification("b", Some(0))),
        );
        assert!(!expire_cards(&mut state));
        assert!(state.cards.is_empty());
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            NotifyEvent::Closed { .. }
        ));
    }

    #[test]
    fn escape_targets_the_newest_expiring_card() {
        let mut state = fixture_state();
        apply_command(
            &mut state,
            NotifyCommand::Notify(notification("persist", None)),
        );
        apply_command(
            &mut state,
            NotifyCommand::Notify(notification("timed", Some(30))),
        );
        assert_eq!(state.newest_expiring(), Some(0));

        let mut buttoned = notification("persist2", None);
        buttoned.buttons.push(NotifyButton {
            action: "a".into(),
            label: "b".into(),
        });
        apply_command(&mut state, NotifyCommand::Notify(buttoned));
        // The persistent cards are skipped; the timed one is still newest-expiring.
        assert_eq!(state.newest_expiring(), Some(1));
    }
}
