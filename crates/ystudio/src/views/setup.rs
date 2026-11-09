//! First-run setup: a development workspace, the tools the Studio needs, and
//! Yardstick's own development-only setup, answered in the GUI and applied by
//! `Install-Yardstick.ps1 -NonInteractive` (see `ystudio_core::setup`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::AgentBackend;
use ystudio_core::process::find_program;
use ystudio_core::setup::{self, Answers, SetupResult, Status};
use ystudio_core::workspace::WorkspaceKind;

use crate::app::{JobKind, StudioApp, View, job_log};
use crate::theme::{self, FluentText as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
    #[default]
    Workspace,
    Tools,
    Yardstick,
    Done,
}

impl Step {
    const ALL: [(Step, &'static str); 4] = [
        (Step::Workspace, "Workspace"),
        (Step::Tools, "Tools"),
        (Step::Yardstick, "Set up Yardstick"),
        (Step::Done, "Done"),
    ];
}

#[derive(Default)]
enum Check {
    #[default]
    NotStarted,
    Running,
    Ready(Box<Status>),
    Failed(String),
}

#[derive(Default)]
enum Run {
    #[default]
    Idle,
    Running(u64),
    Finished(Result<SetupResult, String>),
}

#[derive(Default)]
pub struct SetupState {
    pub step: Step,
    /// The workspace the status was read for.
    pub(crate) checked_root: Option<PathBuf>,
    check: Check,
    run: Run,
    modules: BTreeSet<String>,
    set_policy: bool,
    shared_icons: String,
    shared_overrides: String,
    /// The tool check, run once per visit to the Tools step.
    tools: Option<Tools>,
}

struct Tools {
    pwsh: Option<PathBuf>,
    git: Option<PathBuf>,
    gh: Option<PathBuf>,
    agents: [Option<PathBuf>; AgentBackend::ALL.len()],
}

impl Tools {
    fn probe(configured: Option<&std::path::Path>) -> Self {
        Self {
            pwsh: setup::pwsh7(configured),
            git: find_program("git", &[]),
            gh: find_program("gh", &[]),
            agents: AgentBackend::ALL.map(|b| b.locate()),
        }
    }
}

fn result_path(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ystudio-setup-{}-{kind}.json", std::process::id()))
}

/// Opens the wizard on first run, unless a development workspace shows the
/// Studio is already in use.
pub fn maybe_start(app: &mut StudioApp) {
    if app.settings.setup_done || app.view != View::Home {
        return;
    }
    if app.ws.as_ref().is_some_and(|w| w.kind == WorkspaceKind::Development) {
        app.settings.setup_done = true;
        app.save_settings();
        return;
    }
    open(app, Step::Workspace);
}

/// Shows the wizard at `step` (Settings and Home reopen it).
pub fn open(app: &mut StudioApp, step: Step) {
    app.setup.step = step;
    app.setup.tools = None;
    app.view = View::Setup;
}

fn finish(app: &mut StudioApp, to: View) {
    app.settings.setup_done = true;
    app.save_settings();
    app.setup.step = Step::Workspace;
    app.view = to;
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            theme::page_title(ui, "Set up the Studio");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Skip setup").on_hover_text("You can run it again from Settings.").clicked() {
                    finish(app, View::Home);
                }
            });
        });
        ui.label(RichText::new("A few steps to a development workspace where recipes can be written and tested. Nothing here touches production.").weak());
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for (i, (step, name)) in Step::ALL.iter().enumerate() {
                let current = app.setup.step == *step;
                let done = Step::ALL.iter().position(|(s, _)| *s == app.setup.step).is_some_and(|at| i < at);
                let color = if current { theme::accent() } else if done { theme::green() } else { theme::grey() };
                theme::badge(ui, format!("{}", i + 1), color);
                ui.label(if current { RichText::new(*name).semibold() } else { RichText::new(*name).weak() });
                if i + 1 < Step::ALL.len() {
                    ui.label(RichText::new("›").weak());
                }
            }
        });
        ui.add_space(12.0);
        match app.setup.step {
            Step::Workspace => workspace_step(app, ui),
            Step::Tools => tools_step(app, ui),
            Step::Yardstick => yardstick_step(app, ui),
            Step::Done => done_step(app, ui),
        }
    });
}

fn nav_buttons(app: &mut StudioApp, ui: &mut Ui, back: Option<Step>, next: Option<(Step, bool, &str)>) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if let Some(step) = back
            && ui.button("Back").clicked()
        {
            app.setup.step = step;
            app.setup.tools = None;
        }
        if let Some((step, enabled, why)) = next {
            let r = theme::primary(ui, enabled, "Next");
            if r.clicked() {
                app.setup.step = step;
                app.setup.tools = None;
            } else if !enabled {
                r.on_disabled_hover_text(why);
            }
        }
    });
}

fn workspace_step(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Where do you develop recipes?");
        ui.label("The Studio works in a development clone of Yardstick. Your production folder stays read-only.");
        ui.add_space(6.0);
        match app.ws.as_ref().map(|w| (w.root.clone(), w.kind.clone())) {
            Some((root, WorkspaceKind::Production { .. })) => {
                ui.colored_label(theme::amber(), format!("{} is a production folder, so the Studio only reads it. Clone a development workspace below, or open one.", root.display()));
            }
            Some((root, kind)) => {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::green(), "\u{E73E}");
                    ui.label("Using");
                    ui.label(RichText::new(root.display().to_string()).monospace());
                    theme::badge(ui, kind.label(), if kind == WorkspaceKind::Development { theme::green() } else { theme::amber() });
                });
            }
            None => {
                ui.label(RichText::new("No workspace is open yet.").weak());
            }
        }
        ui.add_space(6.0);
        if ui.button("\u{E838}  Open an existing clone…").clicked()
            && let Some(p) = rfd::FileDialog::new().set_title("Open a Yardstick development clone").pick_folder()
        {
            app.open_workspace(&p);
        }
    });
    ui.add_space(theme::CARD_GAP);
    theme::card(ui, |ui| {
        super::home::create_body(app, ui);
        ui.add_space(6.0);
        super::home::create_footer(app, ui);
    });
    let ready = app.writable();
    nav_buttons(app, ui, None, Some((Step::Tools, ready, "Open or clone a development workspace first.")));
}

fn tool_row(ui: &mut Ui, name: &str, found: &Option<PathBuf>, required: bool, what: &str, help: Option<(&str, &str)>) {
    ui.horizontal_wrapped(|ui| {
        match found {
            Some(p) => {
                ui.colored_label(theme::green(), "\u{E73E}");
                ui.label(RichText::new(name).semibold());
                ui.label(RichText::new(p.display().to_string()).small().monospace().weak());
            }
            None => {
                ui.colored_label(if required { theme::red() } else { theme::amber() }, "\u{E711}");
                ui.label(RichText::new(name).semibold());
                ui.label(RichText::new(if required { "required" } else { "optional" }).small().weak());
                if let Some((label, url)) = help {
                    ui.hyperlink_to(label, url);
                }
            }
        }
    });
    ui.label(RichText::new(what).small().weak());
    ui.add_space(4.0);
}

fn tools_step(app: &mut StudioApp, ui: &mut Ui) {
    let configured = app.settings.powershell_path.clone();
    let tools = app.setup.tools.get_or_insert_with(|| Tools::probe(configured.as_deref()));
    let (pwsh, git) = (tools.pwsh.is_some(), tools.git.is_some());
    let any_agent = tools.agents.iter().any(Option::is_some);
    theme::card(ui, |ui| {
        theme::section_heading(ui, "Tools");
        tool_row(ui, "PowerShell 7", &tools.pwsh, true, "Runs Yardstick's setup and the recipe harness.", Some(("Install: winget install --id Microsoft.PowerShell", "https://aka.ms/powershell")));
        tool_row(ui, "Git", &tools.git, true, "Clones and updates the workspace, and publishes recipes as pull requests.", Some(("Get Git (or install it from Company Portal)", "https://git-scm.com/download/win")));
        tool_row(ui, "GitHub CLI (gh)", &tools.gh, false, "Opens pull requests for you. Without it, the Studio offers GitHub's page instead.", Some(("Get gh", "https://cli.github.com")));
        for (b, found) in AgentBackend::ALL.iter().zip(&tools.agents) {
            let help = match b {
                AgentBackend::CopilotCli => ("Get GitHub Copilot CLI", "https://github.com/github/copilot-cli"),
                AgentBackend::ClaudeCode => ("Get Claude Code", "https://github.com/anthropics/claude-code"),
            };
            tool_row(ui, b.label(), found, false, "A coding agent that writes and repairs recipes. You need one of the two.", Some(help));
        }
    });
    if !any_agent {
        ui.add_space(4.0);
        ui.colored_label(theme::amber(), "Without an agent CLI you can still design and test recipes by hand.");
    }
    ui.add_space(6.0);
    if ui.button("\u{E72C}  Re-check").on_hover_text("Looks again, including tools installed since the Studio started.").clicked() {
        let _ = ystudio_core::process::refresh_path();
        app.setup.tools = None;
        app.agent.backends_found = None;
        app.settings_ui.tools = None;
    }
    let why = if !pwsh { "PowerShell 7 is required." } else { "Git is required." };
    nav_buttons(app, ui, Some(Step::Workspace), Some((Step::Yardstick, pwsh && git, why)));
}

/// Pre-fills the answers from what is already in place.
fn prefill(st: &mut SetupState, status: &Status) {
    st.modules = status.optional_modules.iter().filter(|m| !m.is_current && m.used_by_recipes).map(|m| m.name.clone()).collect();
    st.set_policy = !status.execution_policy.allows_scripts && !status.execution_policy.group_policy;
    st.shared_icons = status.preferences.shared_icons.clone();
    // The installer reports the overrides file; the folder is what people know.
    let overrides = &status.preferences.shared_recipe_overrides;
    st.shared_overrides = overrides.strip_suffix("\\recipeOverrides.yaml").unwrap_or(overrides).to_string();
}

fn yardstick_step(app: &mut StudioApp, ui: &mut Ui) {
    let Some(ws) = app.ws.clone() else {
        app.setup.step = Step::Workspace;
        return;
    };
    if !setup::supported(&ws) {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(RichText::new("This clone's Install-Yardstick.ps1 is too old for the wizard").semibold().color(theme::amber()));
            ui.label("Pull the latest Yardstick (it needs pull request #36), or run the installer in a console and answer its questions there.");
        });
        super::git_bar(app, ui);
        if ui.button("\u{E768}  Run Install-Yardstick -DevelopmentOnly in a console").clicked() {
            let result = ystudio_core::harness::install_development_only(&ws, &app.pwsh()).and_then(|s| ystudio_core::process::launch_detached(&s));
            match result {
                Ok(()) => app.notify("The installer opened in a new console. Reload the workspace when it finishes."),
                Err(e) => app.error(format!("{e:#}")),
            }
        }
        nav_buttons(app, ui, Some(Step::Tools), Some((Step::Done, true, "")));
        return;
    }
    let Some(pwsh) = setup::pwsh7(app.settings.powershell_path.as_deref()) else {
        app.setup.step = Step::Tools;
        return;
    };
    // Read what is in place, once per workspace.
    if app.setup.checked_root.as_ref() != Some(&ws.root) {
        app.setup.checked_root = Some(ws.root.clone());
        app.setup.run = Run::Idle;
        let path = result_path("status");
        let _ = std::fs::remove_file(&path);
        app.setup.check = match setup::status_spec(&ws, &pwsh, &path) {
            Ok(spec) => app.start_background_job(spec, JobKind::SetupStatus { result: path }).map_or_else(|| Check::Failed("could not start PowerShell".into()), |_| Check::Running),
            Err(e) => Check::Failed(format!("{e:#}")),
        };
    }
    match &app.setup.check {
        Check::NotStarted | Check::Running => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking what is already set up… (about 10 seconds)");
            });
            return;
        }
        Check::Failed(e) => {
            ui.colored_label(theme::red(), format!("Could not read the current setup: {e}"));
            if ui.button("Try again").clicked() {
                app.setup.checked_root = None;
            }
            nav_buttons(app, ui, Some(Step::Tools), None);
            return;
        }
        Check::Ready(_) => {}
    }
    let Check::Ready(status) = &app.setup.check else { return };
    let status = status.clone();
    let running = matches!(app.setup.run, Run::Running(_));
    let st = &mut app.setup;

    theme::card(ui, |ui| {
        theme::section_heading(ui, "Yardstick on this computer");
        if status.preferences.exists && status.preferences.development_only {
            ui.colored_label(theme::green(), "\u{E73E}  This clone is already set up for development. Run setup again to change the settings below.");
        } else {
            ui.label("This marks the clone development-only (it never connects to Intune), creates its working folders and checks its tools.");
        }
        ui.add_space(8.0);
        ui.label(RichText::new("PowerShell modules").semibold());
        ui.add_enabled_ui(!running, |ui| {
            for m in &status.optional_modules {
                let installed = m.installed_version.as_deref().map(|v| format!("{v} installed")).unwrap_or_else(|| "not installed".into());
                if m.is_current {
                    ui.horizontal(|ui| {
                        ui.colored_label(theme::green(), "\u{E73E}");
                        ui.label(format!("{} - {} ({installed})", m.name, m.purpose));
                    });
                } else {
                    let mut on = st.modules.contains(&m.name);
                    let need = m.minimum_version.as_deref().map(|v| format!(", needs {v}+")).unwrap_or_default();
                    if ui.checkbox(&mut on, format!("Install {} - {} ({installed}{need})", m.name, m.purpose)).changed() {
                        if on {
                            st.modules.insert(m.name.clone());
                        } else {
                            st.modules.remove(&m.name);
                        }
                    }
                }
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Script execution").semibold());
        let p = &status.execution_policy;
        if p.allows_scripts {
            ui.horizontal(|ui| {
                ui.colored_label(theme::green(), "\u{E73E}");
                ui.label(format!("The execution policy ({}) lets Yardstick's scripts run.", p.policy));
            });
        } else if p.group_policy {
            ui.colored_label(theme::amber(), format!("Group Policy sets the execution policy to {}, which blocks Yardstick's scripts outside the Studio. Ask IT to allow RemoteSigned.", p.policy));
        } else {
            ui.add_enabled(!running, egui::Checkbox::new(&mut st.set_policy, format!("Allow Yardstick's scripts for my account (RemoteSigned; it is {} now)", p.policy)));
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Shared with your team").semibold());
        ui.label(RichText::new("Icons and site-specific recipe overrides kept in a shared folder. Leave blank to keep them on this computer.").small().weak());
        for (label, text) in [("Icon folder", &mut st.shared_icons), ("Recipe overrides folder", &mut st.shared_overrides)] {
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(egui::vec2(170.0, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_width(170.0);
                    ui.label(label);
                });
                ui.add_enabled(!running, egui::TextEdit::singleline(text).hint_text(r"e.g. \\server\share\Intune").desired_width(380.0));
                if ui.add_enabled(!running, egui::Button::new("Browse…")).clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    *text = p.display().to_string();
                }
            });
        }
        let mut notes = Vec::new();
        if status.unwritable_paths.count > 0 {
            notes.push(format!(
                "Windows will ask once for administrator rights, to give you back {} item(s) an elevated session created (e.g. {}).",
                status.unwritable_paths.count,
                status.unwritable_paths.examples.first().cloned().unwrap_or_default()
            ));
        }
        if !status.intune_win_app_util.present {
            notes.push("IntuneWinAppUtil.exe is downloaded from Microsoft's GitHub release.".into());
        }
        if !notes.is_empty() {
            ui.add_space(6.0);
            for n in notes {
                ui.label(RichText::new(n).small());
            }
        }
    });
    ui.add_space(8.0);

    let mut start = false;
    ui.horizontal(|ui| {
        let label = if status.preferences.exists { "\u{E768}  Run setup again" } else { "\u{E768}  Set up" };
        start = theme::primary(ui, !running, label).clicked();
        if running {
            ui.spinner();
            ui.label("Setting up… usually under a minute, longer when modules are installed.");
        }
    });
    if start {
        run_setup(app);
    }

    let mut next = false;
    match &app.setup.run {
        Run::Idle => {}
        Run::Running(job) => {
            if let Some(job) = app.job_mut(*job) {
                job_log(ui, &mut job.proc, "setup");
            }
        }
        Run::Finished(Ok(r)) => {
            theme::card(ui, |ui| {
                theme::section_heading(ui, if r.succeeded { "Setup finished" } else { "Setup needs attention" });
                for s in &r.steps {
                    level_row(ui, &s.level, &format!("{}: {}", s.step, s.result), &s.detail);
                }
                let notable: Vec<_> = r.messages.iter().filter(|m| m.level == "Warn" || m.level == "Fail").collect();
                if !notable.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Worth a look").semibold());
                    for m in notable {
                        level_row(ui, &m.level, &m.message, "");
                    }
                }
            });
            next = r.succeeded;
        }
        Run::Finished(Err(e)) => {
            ui.colored_label(theme::red(), e);
            if let Some(job) = app.jobs.iter_mut().rev().find(|j| matches!(j.kind, JobKind::Setup { .. })) {
                job_log(ui, &mut job.proc, "setup");
            }
        }
    }
    nav_buttons(app, ui, Some(Step::Tools), Some((Step::Done, next && !running, "Run setup first.")));
}

/// Runs the development-only setup with the wizard's answers.
pub(crate) fn run_setup(app: &mut StudioApp) {
    let Some(ws) = app.ws.clone() else { return };
    let Some(pwsh) = setup::pwsh7(app.settings.powershell_path.as_deref()) else {
        app.error("PowerShell 7 is required to set up Yardstick.");
        return;
    };
    let st = &app.setup;
    let answers = Answers {
        modules: st.modules.iter().cloned().collect(),
        set_execution_policy: st.set_policy,
        shared_icons: Some(st.shared_icons.trim().to_string()),
        shared_recipe_overrides: Some(st.shared_overrides.trim().to_string()),
    };
    let path = result_path("result");
    let _ = std::fs::remove_file(&path);
    match setup::setup_spec(&ws, &pwsh, &answers, &path) {
        Ok(spec) => {
            if let Some(job) = app.start_job(spec, JobKind::Setup { result: path }) {
                app.setup.run = Run::Running(job);
            }
        }
        Err(e) => app.error(format!("{e:#}")),
    }
}

fn level_row(ui: &mut Ui, level: &str, text: &str, detail: &str) {
    let (glyph, color) = match level {
        "OK" => ("\u{E73E}", theme::green()),
        "Fail" => ("\u{E711}", theme::red()),
        "Skip" => ("\u{E738}", theme::grey()),
        "Info" => ("\u{E946}", theme::grey()),
        _ => ("\u{E7BA}", theme::amber()),
    };
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(color, glyph);
        ui.label(text);
        if !detail.is_empty() {
            ui.label(RichText::new(detail).small().weak());
        }
    });
}

fn done_step(app: &mut StudioApp, ui: &mut Ui) {
    theme::card(ui, |ui| {
        theme::section_heading(ui, "You're set up");
        ui.label("Start with a recipe, or bring in a list of requests.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if theme::primary(ui, app.writable(), "\u{E710}  New recipe…").clicked() {
                finish(app, View::Home);
                app.library.new_open = true;
            }
            if ui.button("\u{E8C3}  Import requests").clicked() {
                finish(app, View::Batch);
            }
            if ui.button("Go to Home").clicked() {
                finish(app, View::Home);
            }
        });
    });
}

/// `Install-Yardstick -Status` finished: read what it found.
pub fn status_finished(app: &mut StudioApp, ok: bool, result: &std::path::Path) {
    app.setup.check = if !ok {
        Check::Failed("the installer stopped without reporting; see its output in the console".into())
    } else {
        match setup::read_status(result) {
            Ok(status) => {
                prefill(&mut app.setup, &status);
                Check::Ready(Box::new(status))
            }
            Err(e) => Check::Failed(format!("{e:#}")),
        }
    };
    let _ = std::fs::remove_file(result);
}

/// The setup run finished: show its result and pick up the new preferences.
pub fn setup_finished(app: &mut StudioApp, ok: bool, result: &std::path::Path) {
    let read = setup::read_result(result);
    let _ = std::fs::remove_file(result);
    app.setup.run = Run::Finished(match read {
        Ok(r) => Ok(r),
        Err(_) if !ok => Err("The installer stopped before it could report. Its output is below.".into()),
        Err(e) => Err(format!("{e:#}")),
    });
    // Preferences now exist: the workspace is development-only.
    app.reload_workspace();
    app.refresh_recipes();
    app.setup.checked_root = app.ws.as_ref().map(|w| w.root.clone());
    if matches!(&app.setup.run, Run::Finished(Ok(r)) if r.succeeded) {
        app.notify("Yardstick is set up for development on this computer.");
    }
}

#[cfg(test)]
impl SetupState {
    pub(crate) fn status_for_tests(&self) -> Option<&Status> {
        match &self.check {
            Check::Ready(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn result_for_tests(&self) -> Option<&Result<SetupResult, String>> {
        match &self.run {
            Run::Finished(r) => Some(r),
            _ => None,
        }
    }

    pub(crate) fn answers_for_tests(&self) -> (Vec<String>, bool) {
        (self.modules.iter().cloned().collect(), self.set_policy)
    }
}
