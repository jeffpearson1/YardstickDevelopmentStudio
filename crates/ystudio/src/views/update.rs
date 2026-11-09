//! Self-update: background check and download, the Updates card in Settings,
//! the banner above every page and the status-bar badge. The work itself lives in `ystudio_core::update`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText, Ui};
use ystudio_core::update::{self, Release};

use crate::app::{StudioApp, View};
use crate::theme::{self, FluentText as _};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { release: Release, progress: Arc<AtomicU64> },
    /// Downloaded and verified, waiting for the Studio to be idle.
    Ready { release: Release, msi: PathBuf },
    Failed(String),
}

enum Msg {
    Checked(anyhow::Result<Option<Release>>),
    Downloaded(Release, anyhow::Result<PathBuf>),
}

pub struct UpdateState {
    pub status: Status,
    rx: Option<Receiver<Msg>>,
    /// A startup check: stay quiet about errors and skipped versions.
    quiet: bool,
    /// The version whose banner was dismissed this session.
    dismissed: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self { status: Status::Idle, rx: None, quiet: false, dismissed: None }
    }
}

impl UpdateState {
    /// A newer release worth surfacing outside the Updates card.
    pub fn badge_version<'a>(&'a self, skipped: &str) -> Option<&'a str> {
        match &self.status {
            Status::Available(r) if r.version != skipped => Some(&r.version),
            Status::Ready { release, .. } => Some(&release.version),
            _ => None,
        }
    }
}

pub fn start_check(app: &mut StudioApp, quiet: bool) {
    if matches!(app.update.status, Status::Checking | Status::Downloading { .. }) {
        return;
    }
    let (tx, rx) = channel();
    let waker = app.waker.clone();
    std::thread::spawn(move || {
        let _ = tx.send(Msg::Checked(update::check(CURRENT_VERSION)));
        waker();
    });
    app.update = UpdateState { status: Status::Checking, rx: Some(rx), quiet, dismissed: app.update.dismissed.take() };
}

fn start_download(app: &mut StudioApp, release: Release) {
    let (tx, rx) = channel();
    let progress = Arc::new(AtomicU64::new(0));
    let (p, r, waker) = (progress.clone(), release.clone(), app.waker.clone());
    std::thread::spawn(move || {
        let tick = waker.clone();
        let done = std::sync::atomic::AtomicBool::new(false);
        let result = std::thread::scope(|s| {
            s.spawn(|| {
                // Keep the progress bar moving while bytes arrive.
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    tick();
                }
            });
            let result = update::download(&r, &update::download_dir(), &p);
            done.store(true, Ordering::Relaxed);
            result
        });
        let _ = tx.send(Msg::Downloaded(r, result));
        waker();
    });
    app.update = UpdateState { status: Status::Downloading { release, progress }, rx: Some(rx), quiet: false, dismissed: app.update.dismissed.take() };
}

/// Drains finished checks and downloads. Called every frame.
pub fn poll(app: &mut StudioApp) {
    let Some(msg) = app.update.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { return };
    app.update.rx = None;
    let quiet = app.update.quiet;
    match msg {
        Msg::Checked(Ok(Some(r))) => {
            if !(quiet && r.version == app.settings.skipped_update) {
                app.notify(format!("Yardstick Development Studio {} is available - see Settings > Updates.", r.version));
            }
            app.update.status = Status::Available(r);
        }
        Msg::Checked(Ok(None)) => app.update.status = Status::UpToDate,
        Msg::Checked(Err(e)) => {
            let msg = format!("{e:#}");
            if !quiet {
                app.error(format!("Update check failed: {msg}"));
            }
            app.update.status = Status::Failed(msg);
        }
        Msg::Downloaded(release, Ok(msi)) => {
            app.update.status = Status::Ready { release, msi };
            install_if_idle(app);
        }
        Msg::Downloaded(_, Err(e)) => {
            let msg = format!("{e:#}");
            app.error(format!("Update download failed: {msg}"));
            app.update.status = Status::Failed(msg);
        }
    }
}

/// Why the Studio cannot close for the installer right now.
fn blocker(app: &StudioApp) -> Option<String> {
    let running = app.running_jobs();
    if running > 0 {
        return Some(format!("{running} job(s) are still running. Let them finish or cancel them in the console."));
    }
    if let Some(r) = app.recipe.as_ref().filter(|r| r.dirty()) {
        return Some(format!("'{}' has unsaved changes. Save or revert them first.", r.id));
    }
    None
}

fn install_if_idle(app: &mut StudioApp) {
    let Status::Ready { msi, .. } = &app.update.status else { return };
    if let Some(why) = blocker(app) {
        app.notify(format!("Update downloaded. {why} Then choose Install and restart."));
        return;
    }
    let msi = msi.clone();
    let exe = std::env::current_exe().unwrap_or_else(|_| update::installed_exe());
    match ystudio_core::process::spawn_hidden(&update::installer_helper(&msi, std::process::id(), &exe)) {
        Ok(()) => {
            app.shutdown();
            std::process::exit(0);
        }
        Err(e) => app.error(format!("Could not start the installer: {e:#}")),
    }
}

/// Handles `--updated <msiexec exit code> <log>`, passed by the installer helper on relaunch.
pub fn report_installed(app: &mut StudioApp, code: &str, log: Option<&str>) {
    let code: i32 = code.trim().parse().unwrap_or(-1);
    match update::describe_msi_exit(code) {
        None => {
            app.notify(format!("Updated to Yardstick Development Studio {CURRENT_VERSION}."));
            let _ = std::fs::remove_dir_all(update::download_dir());
        }
        Some(text) if code == 3010 => app.notify(text),
        Some(text) => {
            let log = log.map(|l| format!(" Log: {l}")).unwrap_or_default();
            app.error(format!("{text} (msiexec exit {code}).{log}"));
        }
    }
}

/// Status-bar badge when an update is available.
pub fn badge(app: &mut StudioApp, ui: &mut Ui) {
    let Some(v) = app.update.badge_version(&app.settings.skipped_update) else { return };
    ui.separator();
    if ui
        .add(egui::Button::new(RichText::new(format!("\u{E896}  Update {v}")).color(theme::accent())).frame(false))
        .on_hover_text("A newer Yardstick Development Studio is available. Open Updates in Settings.")
        .clicked()
    {
        app.view = View::Settings;
    }
}

/// Shown above every page while a newer release waits, until the user opens
/// Updates or dismisses it for this session.
pub fn banner(app: &mut StudioApp, ui: &mut Ui) {
    if app.view == View::Settings {
        return;
    }
    let Some(v) = app.update.badge_version(&app.settings.skipped_update).map(str::to_string) else { return };
    if app.update.dismissed.as_deref() == Some(v.as_str()) {
        return;
    }
    let what = if matches!(app.update.status, Status::Ready { .. }) { "is downloaded and ready to install" } else { "is available" };
    let (mut open, mut later) = (false, false);
    theme::banner(ui, theme::accent(), |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Yardstick Development Studio {v} {what}.")).semibold());
            open = theme::primary(ui, true, "View update").clicked();
            later = ui.button("Later").clicked();
        });
    });
    ui.add_space(8.0);
    if open {
        app.view = View::Settings;
    }
    if later {
        app.update.dismissed = Some(v);
    }
}

/// The Updates card body in Settings.
pub fn card(app: &mut StudioApp, ui: &mut Ui) {
    theme::section_heading(ui, "Updates");
    ui.horizontal(|ui| {
        ui.label(format!("Version {CURRENT_VERSION}"));
        ui.label(RichText::new("· updates come from GitHub Releases and are verified by SHA-256 before installing").small().weak());
    });
    ui.add_space(6.0);
    let mut action = None;
    match &app.update.status {
        Status::Idle => {
            ui.label(RichText::new("Not checked yet.").weak());
        }
        Status::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking for updates…");
            });
        }
        Status::UpToDate => {
            ui.colored_label(theme::green(), "\u{E73E}  You have the latest version.");
        }
        Status::Failed(e) => {
            ui.colored_label(theme::red(), format!("\u{E711}  {e}"));
        }
        Status::Available(r) => {
            ui.label(RichText::new(format!("Version {} is available.", r.version)).semibold().color(theme::accent()));
            notes(ui, r);
            ui.horizontal(|ui| {
                if theme::primary(ui, true, "\u{E896}  Download and install").on_hover_text("Downloads the MSI, closes the Studio, installs (Windows will ask for admin rights) and reopens it.").clicked() {
                    action = Some(Action::Download(r.clone()));
                }
                if r.version != app.settings.skipped_update && ui.button("Skip this version").clicked() {
                    action = Some(Action::Skip(r.version.clone()));
                }
            });
        }
        Status::Downloading { release, progress } => {
            let done = progress.load(Ordering::Relaxed);
            let frac = if release.size > 0 { done as f32 / release.size as f32 } else { 0.0 };
            ui.label(format!("Downloading {}…", release.asset_name));
            ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(360.0));
        }
        Status::Ready { release, .. } => {
            ui.label(RichText::new(format!("Version {} is downloaded and verified.", release.version)).semibold());
            if let Some(why) = blocker(app) {
                ui.colored_label(theme::amber(), why);
            }
            if theme::primary(ui, blocker(app).is_none(), "\u{E777}  Install and restart").clicked() {
                action = Some(Action::Install);
            }
        }
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let busy = matches!(app.update.status, Status::Checking | Status::Downloading { .. });
        if ui.add_enabled(!busy, egui::Button::new("\u{E72C}  Check for updates")).clicked() {
            action = Some(Action::Check);
        }
        if ui.checkbox(&mut app.settings.auto_check_updates, "Check at startup").changed() {
            action = Some(Action::SaveSettings);
        }
    });
    match action {
        Some(Action::Check) => start_check(app, false),
        Some(Action::Download(r)) => start_download(app, r),
        Some(Action::Install) => install_if_idle(app),
        Some(Action::Skip(v)) => {
            app.settings.skipped_update = v;
            app.save_settings();
        }
        Some(Action::SaveSettings) => app.save_settings(),
        None => {}
    }
}

enum Action {
    Check,
    Download(Release),
    Install,
    Skip(String),
    SaveSettings,
}

fn notes(ui: &mut Ui, r: &Release) {
    let first: String = r.notes.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).take(3).collect::<Vec<_>>().join(" ");
    if !first.is_empty() {
        ui.add(egui::Label::new(RichText::new(first).small().weak()).wrap());
    }
    if !r.html_url.is_empty() {
        ui.hyperlink_to("Release notes", &r.html_url);
    }
}
