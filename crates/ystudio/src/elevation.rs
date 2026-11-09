//! The elevated session behind automatic live checks. Starting a run that has
//! system-context live tests shows one UAC prompt; a hidden worker then runs
//! those tests until the run ends. Its state is shown only here, in the Studio.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use eframe::egui::{RichText, Ui};
use ystudio_core::elevation::{self, LaunchError, Session};
use ystudio_core::harness::InstallTestOptions;
use ystudio_core::process::{RunningProcess, fmt_duration};

use crate::app::StudioApp;
use crate::theme;

pub const SHIELD: &str = "\u{EA18}";

#[derive(Default)]
pub enum State {
    #[default]
    Off,
    /// The UAC prompt is up, or the worker is connecting.
    Waiting { rx: Receiver<Result<Session, LaunchError>>, since: Instant },
    Ready { session: Session, since: Instant, tests: usize },
    /// Declined, failed or ended by the user: system-context live tests are
    /// blocked until a retry.
    Unavailable(String),
}

pub enum Live {
    Started(Box<RunningProcess>),
    Waiting,
    Blocked(String),
}

/// Asks for elevation now, while the user is at the Studio, unless a session
/// is already up or on its way.
pub fn begin(app: &mut StudioApp) {
    if matches!(app.elevation, State::Waiting { .. }) || matches!(&app.elevation, State::Ready { session, .. } if session.alive()) {
        return;
    }
    let (tx, rx) = channel();
    let owner = elevation::foreground_window();
    let waker = app.waker.clone();
    std::thread::spawn(move || {
        // If the run ended meanwhile nobody receives this, and dropping the
        // session stops the worker.
        let _ = tx.send(Session::launch(owner, Some(waker.clone())));
        waker();
    });
    app.elevation = State::Waiting { rx, since: Instant::now() };
}

pub fn waiting(app: &StudioApp) -> bool {
    matches!(app.elevation, State::Waiting { .. })
}

/// Resolves the UAC prompt, notices a dead worker, and ends the session once
/// nothing that could need it is running.
pub fn poll(app: &mut StudioApp) {
    let next = match &mut app.elevation {
        State::Waiting { rx, .. } => match rx.try_recv() {
            Ok(Ok(session)) => Some(State::Ready { session, since: Instant::now(), tests: 0 }),
            Ok(Err(e)) => Some(State::Unavailable(e.to_string())),
            Err(TryRecvError::Disconnected) => Some(State::Unavailable("The elevation request ended without an answer.".into())),
            Err(TryRecvError::Empty) => None,
        },
        State::Ready { session, .. } if !session.alive() => Some(State::Unavailable("The elevated worker stopped unexpectedly.".into())),
        _ => None,
    };
    if let Some(state) = next {
        app.elevation = state;
    }
    if !matches!(app.elevation, State::Off) && !in_use(app) {
        app.elevation = State::Off;
    }
}

fn in_use(app: &StudioApp) -> bool {
    crate::workflow::busy(app) || !app.batch.queue.is_empty()
}

/// Starts a system-context live install test in the elevated worker.
pub fn install_test(app: &mut StudioApp, recipe: &Path) -> Live {
    if matches!(app.elevation, State::Off) {
        // Only when a repair turned a user-scope recipe into a system one.
        begin(app);
    }
    let waker = app.waker.clone();
    let Some(ws) = &app.ws else { return Live::Blocked("No workspace is open.".into()) };
    match &mut app.elevation {
        State::Off | State::Waiting { .. } => Live::Waiting,
        State::Unavailable(reason) => Live::Blocked(reason.clone()),
        State::Ready { session, tests, .. } => match session.install_test(ws, recipe, &InstallTestOptions::live(), &app.settings, Some(waker)) {
            Ok(proc) => {
                *tests += 1;
                Live::Started(Box::new(proc))
            }
            Err(e) => Live::Blocked(format!("{e:#}")),
        },
    }
}

/// Status bar badge.
pub fn badge(app: &StudioApp, ui: &mut Ui) {
    let (text, color, hover) = match &app.elevation {
        State::Off => return,
        State::Waiting { .. } => (
            "Waiting for UAC approval".to_string(),
            theme::amber(),
            "Approve the UAC prompt for Yardstick Development Studio to run this run's system-context live tests.".to_string(),
        ),
        State::Ready { session, since, tests } => (
            format!("Elevated session · {tests} live test(s)"),
            theme::blue(),
            format!(
                "A hidden elevated worker (PID {}) started {} ago runs this run's system-context live tests without more UAC prompts. It closes when the run ends.",
                session.worker_pid,
                fmt_duration(since.elapsed())
            ),
        ),
        State::Unavailable(reason) => (
            "Elevation unavailable".to_string(),
            theme::red(),
            format!("{reason} System-context live tests in this run are blocked."),
        ),
    };
    ui.separator();
    theme::badge(ui, format!("{SHIELD}  {text}"), color).on_hover_text(hover);
}

/// The session's line in the run's progress area, with its controls.
pub fn strip(app: &mut StudioApp, ui: &mut Ui) {
    let mut end = false;
    let mut retry = false;
    match &app.elevation {
        State::Off => return,
        State::Waiting { since, .. } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(SHIELD).color(theme::amber()));
                ui.label(format!(
                    "Approve the UAC prompt for Yardstick Development Studio ({}). Safe checks keep running; system-context live tests wait for it.",
                    fmt_duration(since.elapsed())
                ));
            });
        }
        State::Ready { session, since, tests } => {
            let running = session.running();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(SHIELD).color(theme::blue()));
                let doing = if running > 0 { " · live test running" } else { "" };
                ui.label(format!("Elevated session active · {tests} system-context live test(s){doing} · started {} ago", fmt_duration(since.elapsed())));
                end = ui
                    .add_enabled(running == 0, eframe::egui::Button::new("End elevated session"))
                    .on_hover_text("Stops the hidden elevated worker now. Later system-context live tests in this run are blocked unless you retry.")
                    .on_disabled_hover_text("A live test is running elevated. Wait for it, or cancel the run.")
                    .clicked();
            });
        }
        State::Unavailable(reason) => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(SHIELD).color(theme::red()));
                ui.label(RichText::new(format!("{reason} System-context live tests are blocked.")).color(theme::red()));
                retry = ui
                    .button("Retry elevation")
                    .on_hover_text("Shows the UAC prompt again. Recipes already blocked need testing again afterwards.")
                    .clicked();
            });
        }
    }
    if end {
        app.elevation = State::Unavailable("You ended the elevated session.".into());
    }
    if retry {
        app.elevation = State::Off;
        begin(app);
    }
}
