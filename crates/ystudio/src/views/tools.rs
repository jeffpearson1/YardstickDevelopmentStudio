//! The tools the Studio needs and whether they are signed in (see
//! `ystudio_core::prereqs`): checked in the background at startup, shown in
//! the setup wizard and on Home, installed with winget and signed in through
//! a console window.

use std::process::Child;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::prereqs::{self, Context, Report, SignIn, Tool};

use crate::app::{JobKind, StudioApp, View};
use crate::theme::{self, FluentText as _};

pub struct ToolsState {
    pub(crate) report: Option<Report>,
    rx: Option<Receiver<Report>>,
    /// The console a sign-in runs in; the tools are checked again when it closes.
    console: Option<(Tool, Child)>,
    /// Off in tests, which set the report themselves.
    pub(crate) probe: bool,
    /// The Home banner was dismissed for this session.
    dismissed: bool,
    /// Open the wizard's Tools step if the next check finds a missing tool
    /// (set after an update).
    pub(crate) open_if_missing: bool,
}

impl Default for ToolsState {
    fn default() -> Self {
        Self { report: None, rx: None, console: None, probe: true, dismissed: false, open_if_missing: false }
    }
}

impl ToolsState {
    pub fn checking(&self) -> bool {
        self.rx.is_some()
    }

    /// Never checks: tests set the report themselves.
    #[cfg(test)]
    pub(crate) fn fixed() -> Self {
        Self { probe: false, ..Default::default() }
    }
}

/// What a sign-in says, in a few words.
fn describe(state: &SignIn) -> (String, egui::Color32) {
    match state {
        SignIn::SignedIn { account: Some(a) } => (format!("signed in as {a}"), theme::green()),
        SignIn::SignedIn { account: None } => ("signed in".into(), theme::green()),
        SignIn::SignedOut(why) => (format!("not signed in ({why})"), theme::amber()),
        SignIn::NoAccess(why) => (format!("signed in, but cannot push ({why})"), theme::amber()),
        SignIn::Unknown(why) => (format!("could not check the sign-in ({why})"), theme::grey()),
    }
}

/// Checks again in the background; the last report stays up meanwhile.
pub fn start(app: &mut StudioApp) {
    if !app.tools.probe || app.tools.rx.is_some() {
        return;
    }
    let cx = Context {
        configured_pwsh: app.settings.powershell_path.clone(),
        workspace: app.ws.as_ref().filter(|w| w.is_writable()).map(|w| w.root.clone()),
        repo_url: app.settings.repo_url.clone(),
    };
    let (tx, rx) = channel();
    let waker = app.waker.clone();
    std::thread::spawn(move || {
        let _ = tx.send(Report::probe(&cx));
        waker();
    });
    app.tools.rx = Some(rx);
}

/// Re-reads PATH (for tools installed since the Studio started) and checks again.
pub fn recheck(app: &mut StudioApp) {
    let _ = ystudio_core::process::refresh_path();
    app.agent.backends_found = None;
    app.settings_ui.tools = None;
    start(app);
}

/// Called every frame: picks up a finished check and notices a closed sign-in console.
pub fn poll(app: &mut StudioApp, ctx: &egui::Context) {
    if let Some(Ok(report)) = app.tools.rx.as_ref().map(Receiver::try_recv) {
        app.tools.rx = None;
        if app.settings.diagnostic_logging {
            ystudio_core::diagnostics::note(&format!("Tool check: {report:?}"));
        }
        app.tools.report = Some(report);
        if std::mem::take(&mut app.tools.open_if_missing) && app.view == View::Home && missing_essentials(app) {
            super::setup::open(app, super::setup::Step::Tools);
        }
    }
    if let Some((tool, child)) = &mut app.tools.console {
        match child.try_wait() {
            Ok(None) => ctx.request_repaint_after(Duration::from_secs(1)),
            _ => {
                let tool = *tool;
                app.tools.console = None;
                app.notify(format!("Checking {} again…", tool.label()));
                recheck(app);
            }
        }
    }
}

/// PowerShell 7, git, gh or the chosen agent CLI is not installed.
fn missing_essentials(app: &StudioApp) -> bool {
    app.tools.report.as_ref().is_some_and(|r| {
        [Tool::PowerShell, Tool::Git, Tool::Gh, Tool::Agent(app.settings.agent_backend)].iter().any(|t| r.path(*t).is_none())
    })
}

pub fn install(app: &mut StudioApp, tool: Tool) {
    match prereqs::install_spec(tool) {
        Ok(spec) => {
            if app.start_job(spec, JobKind::InstallTool).is_some() {
                app.console_open = true;
            }
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

pub fn install_finished(app: &mut StudioApp, ok: bool) {
    if ok {
        app.notify("Done. Checking the tools again…");
    } else {
        app.error("That did not finish; its output is in the console.");
    }
    recheck(app);
}

pub fn sign_in(app: &mut StudioApp, tool: Tool) {
    let Some(report) = app.tools.report.as_ref() else { return };
    let Some(exe) = report.path(tool).map(std::path::Path::to_path_buf) else { return };
    let gh = report.path(Tool::Gh).map(std::path::Path::to_path_buf);
    let gh_signed_in = report.sign_in(Tool::Gh).is_some_and(SignIn::is_signed_in);
    let repo_url = app.settings.repo_url.clone();
    // gh is already signed in: git only needs pointing at it.
    if tool == Tool::Git
        && let Some(gh) = gh.as_deref().filter(|_| gh_signed_in)
    {
        app.start_job(prereqs::gh_setup_git_spec(gh, &repo_url), JobKind::InstallTool);
        return;
    }
    let workspace = app.ws.as_ref().filter(|w| w.is_writable()).map(|w| w.root.clone());
    let spawned = prereqs::sign_in_spec(tool, &exe, gh.as_deref(), workspace.as_deref(), &repo_url).and_then(|s| ystudio_core::process::spawn_console(&s));
    match spawned {
        Ok(child) => {
            app.tools.console = Some((tool, child));
            app.notify(format!("Finish signing in to {} in the new window. The Studio checks again when it closes.", tool.label()));
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

fn purpose(tool: Tool) -> &'static str {
    match tool {
        Tool::PowerShell => "Runs Yardstick's setup and the recipe harness.",
        Tool::Git => "Clones and updates the workspace, and pushes recipes for pull requests.",
        Tool::Gh => "Opens pull requests for you. Without it, the Studio offers GitHub's page instead.",
        Tool::Agent(_) => "A coding agent that writes and repairs recipes. You need one of the two.",
    }
}

/// Every tool with its install and sign-in state, and buttons to fix them.
pub fn card(app: &mut StudioApp, ui: &mut Ui) {
    if app.tools.report.is_none() {
        start(app);
    }
    let installing = app.job_running(|k| matches!(k, JobKind::InstallTool));
    let signing_in = app.tools.console.as_ref().map(|(t, _)| *t);
    let checking = app.tools.checking();
    let Some(report) = app.tools.report.clone() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking the tools and their sign-ins… (a few seconds)");
        });
        return;
    };
    let mut install_tool = None;
    let mut sign_in_tool = None;
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Tools");
        for tool in Tool::ALL {
            ui.horizontal_wrapped(|ui| {
                match report.path(tool) {
                    Some(p) => {
                        ui.colored_label(theme::green(), "\u{E73E}");
                        ui.label(RichText::new(tool.label()).semibold());
                        ui.label(RichText::new(p.display().to_string()).small().monospace().weak());
                    }
                    None => {
                        ui.colored_label(if tool.required() { theme::red() } else { theme::amber() }, "\u{E711}");
                        ui.label(RichText::new(tool.label()).semibold());
                        ui.label(RichText::new(if tool.required() { "required, not installed" } else { "not installed" }).small().weak());
                        if report.winget.is_some() {
                            let r = ui.add_enabled(!installing, egui::Button::new("\u{E896}  Install").small());
                            if r.on_hover_text(format!("winget install {} (Windows may ask for administrator rights)", tool.winget_id())).clicked() {
                                install_tool = Some(tool);
                            }
                        }
                        ui.hyperlink_to(RichText::new("Download page").small(), tool.download_url());
                    }
                }
            });
            if let Some(state) = report.sign_in(tool) {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(22.0);
                    let (text, color) = describe(state);
                    ui.label(RichText::new(text).small().color(color));
                    if signing_in == Some(tool) {
                        ui.spinner();
                        ui.label(RichText::new("signing in in another window…").small().weak());
                    } else if !state.is_signed_in() {
                        let label = if tool == Tool::Git && report.sign_in(Tool::Gh).is_some_and(SignIn::is_signed_in) { "Use gh's sign-in" } else { "Sign in…" };
                        if ui.add_enabled(signing_in.is_none() && !installing, egui::Button::new(label).small()).clicked() {
                            sign_in_tool = Some(tool);
                        }
                    }
                });
                if tool == Tool::Git && !report.git_checked_push && state.is_signed_in() {
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        ui.label(RichText::new("Push access is checked once a development workspace is open.").small().weak());
                    });
                }
            }
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(purpose(tool)).small().weak());
            });
            ui.add_space(4.0);
        }
        if report.winget.is_none() && report.paths.iter().any(|(_, p)| p.is_none()) {
            ui.label(RichText::new("winget (App Installer) was not found, so missing tools are installed from their download pages.").small().weak());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!checking, egui::Button::new("\u{E72C}  Re-check")).on_hover_text("Looks again, including tools installed since the Studio started.").clicked() {
                recheck(app);
            }
            if checking {
                ui.spinner();
            }
        });
    });
    if let Some(tool) = install_tool {
        install(app, tool);
    }
    if let Some(tool) = sign_in_tool {
        sign_in(app, tool);
    }
}

/// On Home, once set up: what still needs doing, with a way to fix it.
pub fn banner(app: &mut StudioApp, ui: &mut Ui) {
    if app.tools.dismissed || !app.settings.setup_done {
        return;
    }
    let Some(items) = app.tools.report.as_ref().map(|r| r.attention(app.settings.agent_backend)).filter(|i| !i.is_empty()) else { return };
    let mut review = false;
    theme::banner(ui, theme::amber(), |ui| {
        ui.label(RichText::new("Some tools need attention").semibold().color(theme::amber()));
        ui.label(items.join(" · "));
        ui.horizontal(|ui| {
            review = ui.button("Fix in setup").clicked();
            if ui.button("Dismiss").clicked() {
                app.tools.dismissed = true;
            }
        });
    });
    ui.add_space(theme::CARD_GAP);
    if review {
        super::setup::open(app, super::setup::Step::Tools);
    }
}

/// On Publish: a warning and a sign-in button when gh or git is not signed in.
pub fn publish_sign_in(app: &mut StudioApp, ui: &mut Ui) {
    let Some(report) = app.tools.report.as_ref() else { return };
    let problems: Vec<(Tool, String)> = [Tool::Git, Tool::Gh]
        .into_iter()
        .filter_map(|t| report.sign_in(t).filter(|s| s.needs_sign_in()).map(|s| (t, describe(s).0)))
        .collect();
    let mut sign_in_tool = None;
    for (tool, text) in problems {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(theme::amber(), format!("{}: {text}", tool.label()));
            if ui.add_enabled(app.tools.console.is_none(), egui::Button::new("Sign in…").small()).clicked() {
                sign_in_tool = Some(tool);
            }
        });
    }
    if let Some(tool) = sign_in_tool {
        sign_in(app, tool);
    }
}
