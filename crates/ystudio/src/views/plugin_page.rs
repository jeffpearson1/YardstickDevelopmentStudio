//! A plugin's tab. `workitems` pages list work-item folders, each with a
//! pipeline of steps and agent tasks; `catalog` pages search an outside
//! system and act on the chosen row. Both show the plugin's panels, and the
//! prompts its gates need: a confirmation, or an approval bound to the
//! SHA-256 of the file describing the change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkViewer;
use serde_json::{Map, Value};
use ystudio_core::plugin::{
    self, ActionKind, Gate, Health, Installed, Lock, Origin, PageKind, PanelKind, Risk, Scope, Stage, StepDef, StepRequest, StepResult, StepState,
    StudioContext, WorkItem,
};
use ystudio_core::workspace::WorkspaceKind;

use crate::app::{JobKind, StudioApp, View};
use crate::theme::{self, FluentText as _};

/// Why a plugin step ran, so its result goes to the right place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A step of a work item's pipeline.
    Item,
    /// A catalog page's connection check.
    Probe,
    /// A catalog search.
    Query,
    /// Reading a list into new work items.
    Import,
    /// An action on a catalog row, by row key.
    Row(String),
}

#[derive(Default)]
pub struct PageState {
    /// Work items under the work root, newest first.
    pub items: Vec<WorkItem>,
    listed_root: Option<PathBuf>,
    pub selected: Option<PathBuf>,
    pub show_settings: bool,
    new_item: Option<NewItem>,
    import: Option<Vec<(bool, Value)>>,
    prompt: Option<Prompt>,
    panels: Option<PanelCache>,
    pub search: String,
    pub probe: Option<StepResult>,
    probed: bool,
    pub rows: Vec<Value>,
    pub selected_row: Option<usize>,
    /// Folders row actions made, by row key.
    row_folders: HashMap<String, PathBuf>,
}

#[derive(Default)]
struct NewItem {
    title: String,
    reference: String,
    text: String,
    files: Vec<PathBuf>,
    error: Option<String>,
}

enum Prompt {
    Confirm { dir: PathBuf, step: String, params: Map<String, Value>, purpose: Purpose },
    Approve { dir: PathBuf, step: String, params: Map<String, Value>, purpose: Purpose, file: String, sha256: String, summary: Option<String>, changed: bool, checked: Instant, ack: bool },
}

/// Panel contents for one folder, re-read every few seconds.
struct PanelCache {
    dir: PathBuf,
    built: Instant,
    request: Option<Arc<String>>,
    tables: HashMap<String, Result<Vec<Value>, String>>,
    markdown: HashMap<String, Option<Arc<String>>>,
    files: HashMap<String, Vec<String>>,
}

impl PageState {
    fn refresh_items(&mut self, root: &Path) {
        self.items = WorkItem::list(root);
        self.listed_root = Some(root.to_path_buf());
        self.panels = None;
    }
}

/// The plugin, its resolved settings and work root, for one frame.
struct Ctx {
    plugin: Installed,
    settings: Result<Map<String, Value>, Vec<String>>,
    root: Option<PathBuf>,
}

/// The open workspace, when it is a development one (`{workspace}` in plugin settings).
pub fn dev_workspace(app: &StudioApp) -> Option<PathBuf> {
    app.ws.as_ref().filter(|w| w.kind == WorkspaceKind::Development).map(|w| w.root.clone())
}

fn context(app: &StudioApp, id: &str) -> Option<Ctx> {
    let plugin = app.plugins.get(id)?.clone();
    let values = app.settings.plugins.get(id).map(|p| p.values.clone()).unwrap_or_default();
    let settings = plugin::resolve_settings(&plugin.manifest, &values, dev_workspace(app).as_deref());
    let root = settings.as_ref().ok().and_then(plugin::work_root);
    Some(Ctx { plugin, settings, root })
}

fn studio_context(app: &StudioApp) -> StudioContext {
    StudioContext {
        version: super::update::CURRENT_VERSION.to_string(),
        workspace: app.ws.as_ref().map(|w| w.root.clone()),
        workspace_kind: match app.ws.as_ref().map(|w| &w.kind) {
            Some(WorkspaceKind::Development) => "development",
            Some(WorkspaceKind::Production { .. }) => "production",
            Some(WorkspaceKind::Unconfigured) => "unconfigured",
            None => "",
        }
        .to_string(),
        production_paths: app.settings.production_paths.clone(),
    }
}

/// The running plugin job in `dir`, if any.
fn job_in(app: &StudioApp, dir: &Path) -> Option<(u64, String)> {
    app.jobs.iter().find_map(|j| match &j.kind {
        JobKind::PluginStep { dir: d, step, .. } if j.proc.is_running() && d == dir => Some((j.id, step.clone())),
        _ => None,
    })
}

pub fn plugin_busy(app: &StudioApp, id: &str) -> bool {
    app.jobs.iter().any(|j| j.proc.is_running() && matches!(&j.kind, JobKind::PluginStep { plugin, .. } if plugin == id))
}

/// Starts a step. Hash-gated steps need a granted approval; `confirmed`
/// answers a `confirm` gate. Returns whether it started.
pub fn run_step(app: &mut StudioApp, id: &str, dir: &Path, step_id: &str, params: Map<String, Value>, confirmed: bool, purpose: Purpose) -> bool {
    let Some(c) = context(app, id) else { return false };
    let settings = match &c.settings {
        Ok(s) => s.clone(),
        Err(problems) => {
            app.error(format!("{} needs its settings first: {}", c.plugin.manifest.name, problems.join(" ")));
            return false;
        }
    };
    let (Some(root), Some(step)) = (c.root.clone(), c.plugin.manifest.step(step_id).cloned()) else { return false };
    if step.risk == Risk::LocalInstall && crate::workflow::busy(app) {
        app.error("Finish the running agent or install test before installing software with a plugin.");
        return false;
    }
    if let Some((_, other)) = job_in(app, dir) {
        app.error(format!("{other} is still running there."));
        return false;
    }
    let lock = match step.scope {
        Scope::Workitem => match Lock::acquire(dir, &format!("{} · {}", c.plugin.manifest.name, step.label())) {
            Ok(l) => Some(l),
            Err(e) => {
                app.error(format!("{e:#}"));
                return false;
            }
        },
        Scope::Plugin => None,
    };
    let req = StepRequest { plugin: &c.plugin, step: &step, dir, work_root: &root, settings: &settings, params, confirmed };
    let prepared = match plugin::prepare_step(req, &studio_context(app), &app.pwsh(), SystemTime::now()) {
        Ok(p) => p,
        Err(e) => {
            app.error(format!("{e:#}"));
            return false;
        }
    };
    let kind = JobKind::PluginStep { plugin: id.to_string(), dir: dir.to_path_buf(), step: step.id.clone(), result: prepared.result.clone(), risk: step.risk, purpose: purpose.clone() };
    let Some(job) = app.start_job(prepared.spec, kind) else { return false };
    if let Some(lock) = lock {
        app.plugins.locks.insert(job, lock);
    }
    if purpose == Purpose::Item
        && let Ok(mut item) = WorkItem::open(dir)
    {
        let rec = item.meta.steps.entry(step.id.clone()).or_default();
        rec.state = StepState::Running;
        rec.started = ystudio_core::intake::timestamp();
        rec.finished.clear();
        rec.approval = prepared.approval;
        let _ = item.save();
        app.plugins.page(id).refresh_items(&root);
    }
    true
}

/// Records a finished step: in its work item, or in the catalog page.
pub fn step_finished(app: &mut StudioApp, job: u64, id: &str, dir: &Path, step_id: &str, result: &Path, purpose: Purpose) {
    app.plugins.locks.remove(&job);
    let (exit, cancelled) = app.job(job).map(|j| (j.proc.exit.flatten(), j.proc.cancelled)).unwrap_or((None, false));
    let r = plugin::read_result(result, if cancelled { None } else { exit });
    let state = r.status.state();
    let Some(c) = context(app, id) else { return };
    let label = c.plugin.manifest.step(step_id).map(|s| s.label().to_string()).unwrap_or_else(|| step_id.to_string());
    let page = app.plugins.page(id);
    match &purpose {
        Purpose::Item => {
            if let Ok(mut item) = WorkItem::open(dir) {
                let rec = item.meta.steps.entry(step_id.to_string()).or_default();
                rec.state = state;
                rec.finished = ystudio_core::intake::timestamp();
                rec.summary = r.summary.clone();
                rec.result = Some(result.to_path_buf());
                let _ = item.save();
            }
            if let Some(root) = &c.root {
                page.refresh_items(root);
            }
        }
        Purpose::Probe => page.probe = Some(r.clone()),
        Purpose::Query => {
            page.rows = r.rows.clone();
            page.selected_row = None;
        }
        Purpose::Import => {
            if !r.rows.is_empty() {
                page.import = Some(r.rows.iter().map(|v| (true, v.clone())).collect());
            }
        }
        Purpose::Row(key) => {
            if let (Some(folder), Some(root)) = (r.outputs.get("folder"), &c.root)
                && plugin::safe_relative(folder)
            {
                page.row_folders.insert(key.clone(), root.join(folder));
            }
            page.panels = None;
        }
    }
    let summary = if r.summary.is_empty() { String::new() } else { format!(": {}", r.summary) };
    let msg = format!("{} · {label}{summary}", c.plugin.manifest.name);
    match state {
        StepState::Ok | StepState::Warn => app.notify(msg),
        _ => app.error(msg),
    }
}

// ---------------------------------------------------------------- page

pub fn show(app: &mut StudioApp, ui: &mut Ui, tab: u16) {
    let Some(id) = app.plugins.tabs.get(tab as usize).cloned() else { return };
    let enabled = app.settings.plugins.get(&id).is_none_or(|p| p.enabled);
    let Some(c) = context(app, &id).filter(|_| enabled) else {
        theme::page_title(ui, "Plugin removed");
        ui.label(RichText::new("This plugin was uninstalled or turned off. Manage plugins in Settings.").weak());
        if theme::primary(ui, true, "Go to Settings").clicked() {
            app.view = View::Settings;
        }
        return;
    };
    let m = &c.plugin.manifest;
    ui.horizontal(|ui| {
        theme::page_title(ui, &m.name);
        ui.label(RichText::new(format!("{} · {}", m.version, m.publisher)).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let page = app.plugins.page(&id);
            if ui.selectable_label(page.show_settings, "\u{E713}  Settings").clicked() {
                page.show_settings = !page.show_settings;
            }
            if let Some(root) = &c.root
                && ui.button("\u{E838}  Work folder").clicked()
            {
                let _ = std::fs::create_dir_all(root);
                super::open_in_explorer(root);
            }
        });
    });
    if !m.description.is_empty() {
        ui.label(RichText::new(&m.description).weak());
    }
    ui.add_space(6.0);
    let health = match &c.plugin.health {
        Health::Ok => None,
        Health::Modified(files) => Some(format!("Its files changed since install ({}). Reinstall it from Settings to run its steps.", files.iter().take(3).cloned().collect::<Vec<_>>().join(", "))),
        Health::Unrecorded => Some("It wasn't installed by the Studio. Install it from its package in Settings to run its steps.".into()),
        Health::Incompatible(why) => Some(format!("It {why}.")),
    };
    if let Some(h) = health {
        theme::banner(ui, theme::red(), |ui| {
            ui.label(h);
        });
        ui.add_space(6.0);
    }
    let missing = app.plugins.registry.missing_requirements(&c.plugin);
    if !missing.is_empty() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label(format!("This plugin {}. Some actions are unavailable.", missing.join(", ")));
        });
        ui.add_space(6.0);
    }
    if app.plugins.page(&id).show_settings || c.settings.is_err() {
        theme::card(ui, |ui| {
            theme::section_heading(ui, "Settings");
            super::plugins::settings_form(app, ui, &id);
        });
        ui.add_space(theme::CARD_GAP);
    }
    let Some(root) = c.root.clone() else { return };
    if c.settings.is_err() {
        return;
    }
    if !root.is_dir() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("The work folder {} doesn't exist yet.", root.display()));
                if ui.button("Create it").clicked() {
                    match std::fs::create_dir_all(&root) {
                        Ok(()) => app.plugins.page(&id).listed_root = None,
                        Err(e) => app.error(format!("Could not create {}: {e}", root.display())),
                    }
                }
            });
        });
        return;
    }
    match m.page.kind {
        PageKind::Workitems => workitems(app, ui, &c, &root),
        PageKind::Catalog => catalog(app, ui, &c, &root),
    }
}

fn state_badge(ui: &mut Ui, state: StepState) -> egui::Response {
    let (text, color) = match state {
        StepState::NotRun => ("Not run", theme::grey()),
        StepState::Running => ("Running", theme::blue()),
        StepState::Ok => ("Done", theme::green()),
        StepState::Warn => ("Done · warnings", theme::amber()),
        StepState::Fail => ("Failed", theme::red()),
        StepState::Blocked => ("Blocked", theme::amber()),
    };
    theme::badge(ui, text, color)
}

pub fn risk_color(risk: Risk) -> egui::Color32 {
    match risk {
        Risk::LocalInstall | Risk::TenantWrite => theme::red(),
        Risk::TenantRead | Risk::SccmRead => theme::amber(),
        Risk::LocalRead | Risk::LocalWrite => theme::grey(),
    }
}

fn workitems(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, root: &Path) {
    let id = c.plugin.manifest.id.clone();
    let m = &c.plugin.manifest;
    let page = app.plugins.page(&id);
    if page.listed_root.as_deref() != Some(root) {
        page.refresh_items(root);
    }
    let mut import = false;
    egui::Panel::left(("plugin-items", &id)).resizable(true).default_size(300.0).show(ui, |ui| {
        let page = app.plugins.page(&id);
        ui.horizontal(|ui| {
            if theme::primary(ui, true, format!("\u{E710}  New {}", m.page.noun.to_lowercase())).clicked() {
                page.new_item = Some(NewItem::default());
            }
            if m.page.importer.is_some() && ui.button("\u{E8B5}  Import…").on_hover_text("Read a list into new work items").clicked() {
                import = true;
            }
            if ui.button("\u{E72C}").on_hover_text("Refresh").clicked() {
                page.refresh_items(root);
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt(("plugin-items-scroll", &id)).auto_shrink(false).show(ui, |ui| {
            if page.items.is_empty() {
                ui.label(RichText::new(format!("No {}s yet.", m.page.noun.to_lowercase())).weak());
            }
            let total = m.pipeline().len();
            let mut pick = None;
            for item in &page.items {
                let selected = page.selected.as_ref() == Some(&item.dir);
                let title = if item.meta.title.is_empty() { item.name() } else { item.meta.title.clone() };
                if ui.selectable_label(selected, RichText::new(title).semibold()).clicked() {
                    pick = Some(item.dir.clone());
                }
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(item.name()).small().weak());
                    let states: Vec<StepState> = item.meta.steps.values().map(|s| s.state).collect();
                    if states.contains(&StepState::Running) {
                        theme::badge(ui, "Running", theme::blue());
                    } else if states.contains(&StepState::Fail) {
                        theme::badge(ui, "Failed", theme::red());
                    } else if states.contains(&StepState::Blocked) {
                        theme::badge(ui, "Blocked", theme::amber());
                    } else if total > 0 {
                        let done = states.iter().filter(|s| matches!(s, StepState::Ok | StepState::Warn)).count();
                        theme::badge(ui, format!("{done}/{total} done"), if done == total { theme::green() } else { theme::grey() });
                    }
                });
                ui.add_space(4.0);
            }
            if pick.is_some() {
                page.selected = pick;
                page.panels = None;
            }
        });
    });
    if import
        && let Some(importer) = m.page.importer.clone()
        && let Some(file) = rfd::FileDialog::new().add_filter("Lists", &["json", "csv", "txt"]).add_filter("All files", &["*"]).pick_file()
    {
        let mut params = Map::new();
        params.insert("file".into(), Value::String(file.to_string_lossy().into_owned()));
        run_step(app, &id, root, &importer, params, false, Purpose::Import);
    }
    let selected = app.plugins.page(&id).selected.clone();
    egui::ScrollArea::vertical().id_salt(("plugin-detail", &id)).auto_shrink(false).show(ui, |ui| match selected.and_then(|d| WorkItem::open(&d).ok()) {
        Some(item) if item.dir.is_dir() => item_detail(app, ui, c, &item),
        _ => {
            ui.add_space(24.0);
            ui.label(RichText::new(format!("Pick a {} on the left, or start a new one.", m.page.noun.to_lowercase())).weak());
        }
    });
}

fn item_detail(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem) {
    let id = c.plugin.manifest.id.clone();
    let m = &c.plugin.manifest;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&item.meta.title).heading());
        if ui.small_button("\u{E838}  Open folder").clicked() {
            super::open_in_explorer(&item.dir);
        }
    });
    let mut facts = vec![item.name()];
    if !item.meta.reference.is_empty() {
        facts.push(item.meta.reference.clone());
    }
    if !item.meta.origin.from_plugin.is_empty() {
        facts.push(format!("from {}", item.meta.origin.from_plugin));
    }
    ui.label(RichText::new(facts.join(" · ")).weak());
    let running = job_in(app, &item.dir);
    if running.is_none()
        && let Some(holder) = Lock::holder(&item.dir)
    {
        ui.add_space(6.0);
        theme::banner(ui, theme::amber(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("In use: {} by {} on {} since {} UTC.", holder.purpose, holder.user, holder.host, holder.since));
                if ui.button("Clear").on_hover_text("Only if nothing is still running there, e.g. after a crash").clicked() {
                    let _ = Lock::force_release(&item.dir);
                }
            });
        });
    }
    ui.add_space(8.0);
    refresh_panels(app, &id, c, &item.dir, false);
    if let Some(text) = app.plugins.page(&id).panels.as_ref().and_then(|p| p.request.clone()) {
        egui::CollapsingHeader::new("Request").id_salt(("plugin-request", &id)).default_open(false).show(ui, |ui| {
            ui.label(text.as_str());
        });
    }
    theme::section_heading(ui, "Pipeline");
    let busy_elsewhere = crate::workflow::busy(app);
    for stage in m.pipeline() {
        let rec = item.step(stage.id());
        let state = if running.as_ref().is_some_and(|(_, s)| s == stage.id()) { StepState::Running } else { rec.state };
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                state_badge(ui, state);
                ui.label(RichText::new(stage.label()).semibold());
                match stage {
                    Stage::Task(_) => {
                        theme::badge(ui, "Agent", theme::blue());
                    }
                    Stage::Step(s) => {
                        theme::badge(ui, s.risk.label(), risk_color(s.risk));
                        match &s.gate {
                            Gate::Hash { file, .. } => {
                                ui.label("\u{E72E}").on_hover_text(format!("Runs only after you approve exactly this {file}"));
                            }
                            Gate::Confirm => {
                                ui.label("\u{E8FB}").on_hover_text("Asks you to confirm first");
                            }
                            Gate::None => {}
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match stage {
                    Stage::Task(_) => {
                        ui.add_enabled(false, egui::Button::new("\u{E99A}  Run agent")).on_disabled_hover_text("Plugin agent tasks aren't available in this build yet.");
                    }
                    Stage::Step(s) => step_button(app, ui, c, item, s, state, running.is_some(), busy_elsewhere),
                });
            });
            if !stage.description().is_empty() {
                ui.label(RichText::new(stage.description()).weak());
            }
            if !rec.summary.is_empty() {
                let color = match rec.state {
                    StepState::Fail => theme::red(),
                    StepState::Warn | StepState::Blocked => theme::amber(),
                    _ => ui.visuals().text_color(),
                };
                ui.label(RichText::new(&rec.summary).color(color));
            }
            if let Some(result) = rec.result.as_ref().filter(|_| state != StepState::Running) {
                let r = plugin::read_result(result, Some(0));
                ui.horizontal_wrapped(|ui| {
                    for (name, rel) in &r.outputs {
                        if plugin::safe_relative(rel) && ui.small_button(format!("\u{E8A5}  {name}")).on_hover_text(rel).clicked() {
                            super::open_external(&item.dir.join(rel));
                        }
                    }
                    if !rec.finished.is_empty() {
                        ui.label(RichText::new(format!("{} UTC", rec.finished)).small().weak());
                    }
                });
            }
        });
        ui.add_space(6.0);
    }
    if let Some((job, step)) = running {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("{step} is running."));
            if ui.button("\u{E756}  Show log").clicked() {
                app.console_job = Some(job);
                app.console_open = true;
            }
        });
    }
    ui.add_space(8.0);
    panels(app, ui, &id, c, &item.dir);
}

/// The Run button of a pipeline step, disabled with the reason when it can't run.
#[allow(clippy::too_many_arguments)]
fn step_button(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem, s: &StepDef, state: StepState, item_busy: bool, busy_elsewhere: bool) {
    if state == StepState::Running {
        ui.spinner();
        return;
    }
    let gate_file = match &s.gate {
        Gate::Hash { file, .. } => Some(file.clone()),
        _ => None,
    };
    let unmet: Vec<String> = s.after.iter().filter(|a| !item.satisfied(std::slice::from_ref(a))).map(|a| stage_label(c, a)).collect();
    let why = if let Err(e) = c.plugin.ensure_runnable() {
        Some(format!("{e:#}"))
    } else if item_busy {
        Some("Something else is running in this work item.".into())
    } else if !unmet.is_empty() {
        Some(format!("Run {} first.", unmet.join(" and ")))
    } else if s.risk == Risk::LocalInstall && busy_elsewhere {
        Some("Another agent or install test is running.".into())
    } else {
        gate_file.as_ref().filter(|f| !item.dir.join(f).is_file()).map(|f| format!("Needs {f}, which an earlier step writes."))
    };
    let approved = gate_file.as_ref().is_some_and(|f| valid_grant(&item.dir, &s.id, f));
    let text = match (&s.gate, approved, state) {
        (Gate::Hash { .. }, false, _) => "\u{E72E}  Review and approve…",
        (_, _, StepState::NotRun) => "\u{E768}  Run",
        _ => "\u{E72C}  Run again",
    };
    let resp = ui.add_enabled(why.is_none(), egui::Button::new(text));
    if let Some(why) = why {
        resp.on_disabled_hover_text(why);
    } else if resp.clicked() {
        start_with_gate(app, &c.plugin, s, &item.dir, Map::new(), Purpose::Item);
    }
}

fn stage_label(c: &Ctx, id: &str) -> String {
    let m = &c.plugin.manifest;
    m.step(id).map(|s| s.label().to_string()).or_else(|| m.task(id).map(|t| t.label.clone())).unwrap_or_else(|| id.to_string())
}

fn valid_grant(dir: &Path, step: &str, file: &str) -> bool {
    match (plugin::open_grant(dir, step), plugin::sha256_file(&dir.join(file))) {
        (Some((_, a)), Ok(sha)) => a.sha256.eq_ignore_ascii_case(&sha) && a.file == file,
        _ => false,
    }
}

/// Runs a step, asking for confirmation or approval first when its gate needs it.
fn start_with_gate(app: &mut StudioApp, plugin: &Installed, s: &StepDef, dir: &Path, params: Map<String, Value>, purpose: Purpose) {
    let id = plugin.manifest.id.clone();
    match &s.gate {
        Gate::None => {
            run_step(app, &id, dir, &s.id, params, false, purpose);
        }
        Gate::Confirm => app.plugins.page(&id).prompt = Some(Prompt::Confirm { dir: dir.to_path_buf(), step: s.id.clone(), params, purpose }),
        Gate::Hash { file, summary } => {
            if valid_grant(dir, &s.id, file) {
                run_step(app, &id, dir, &s.id, params, false, purpose);
                return;
            }
            match plugin::sha256_file(&dir.join(file)) {
                Ok(sha256) => {
                    let summary = summary.as_ref().and_then(|f| ystudio_core::read_text(&dir.join(f)).ok());
                    app.plugins.page(&id).prompt = Some(Prompt::Approve {
                        dir: dir.to_path_buf(),
                        step: s.id.clone(),
                        params,
                        purpose,
                        file: file.clone(),
                        sha256,
                        summary,
                        changed: false,
                        checked: Instant::now(),
                        ack: false,
                    });
                }
                Err(e) => app.error(format!("{e:#}")),
            }
        }
    }
}

// ---------------------------------------------------------------- catalog

fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        other => other.to_string(),
    }
}

fn row_key(row: &Value, i: usize) -> String {
    ["key", "id"].iter().find_map(|k| row.get(k).map(cell)).filter(|k| !k.is_empty()).unwrap_or_else(|| i.to_string())
}

fn columns(declared: &[String], rows: &[Value]) -> Vec<String> {
    if !declared.is_empty() {
        return declared.to_vec();
    }
    rows.first().and_then(Value::as_object).map(|o| o.keys().filter(|k| *k != "folder").cloned().collect()).unwrap_or_default()
}

fn catalog(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, root: &Path) {
    let id = c.plugin.manifest.id.clone();
    let m = &c.plugin.manifest;
    let probing = app.jobs.iter().any(|j| j.proc.is_running() && matches!(&j.kind, JobKind::PluginStep { plugin, purpose: Purpose::Probe, .. } if *plugin == id));
    let searching = app.jobs.iter().any(|j| j.proc.is_running() && matches!(&j.kind, JobKind::PluginStep { plugin, purpose: Purpose::Query, .. } if *plugin == id));
    if let Some(probe) = m.page.probe.clone() {
        if !app.plugins.page(&id).probed && c.plugin.ensure_runnable().is_ok() {
            app.plugins.page(&id).probed = true;
            run_step(app, &id, root, &probe, Map::new(), false, Purpose::Probe);
        }
        let result = app.plugins.page(&id).probe.clone();
        let (color, text) = match (&result, probing) {
            (_, true) => (theme::blue(), "Checking the connection…".to_string()),
            (Some(r), _) if matches!(r.status.state(), StepState::Ok | StepState::Warn) => (theme::green(), r.summary.clone()),
            (Some(r), _) => (theme::red(), r.summary.clone()),
            (None, _) => (theme::grey(), "Not checked yet.".into()),
        };
        theme::banner(ui, color, |ui| {
            ui.horizontal(|ui| {
                if probing {
                    ui.spinner();
                }
                ui.label(text);
                if !probing && ui.small_button("Check again").clicked() {
                    run_step(app, &id, root, &probe, Map::new(), false, Purpose::Probe);
                }
            });
        });
        ui.add_space(8.0);
    }
    let mut search = false;
    ui.horizontal(|ui| {
        let page = app.plugins.page(&id);
        let resp = ui.add(egui::TextEdit::singleline(&mut page.search).hint_text("Search").desired_width(360.0));
        search = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        search |= theme::primary(ui, !searching, "\u{E721}  Search").clicked();
        if searching {
            ui.spinner();
        }
    });
    if search && !searching && let Some(query) = m.page.query.clone() {
        let mut params = Map::new();
        params.insert("query".into(), Value::String(app.plugins.page(&id).search.trim().to_string()));
        run_step(app, &id, root, &query, params, false, Purpose::Query);
    }
    ui.add_space(8.0);

    let rows = app.plugins.page(&id).rows.clone();
    let cols = columns(&m.page.columns, &rows);
    egui::Panel::left(("plugin-rows", &id)).resizable(true).default_size(520.0).show(ui, |ui| {
        egui::ScrollArea::both().id_salt(("plugin-rows-scroll", &id)).auto_shrink(false).show(ui, |ui| {
            if rows.is_empty() {
                ui.label(RichText::new("Search to list items.").weak());
                return;
            }
            let page = app.plugins.page(&id);
            egui::Grid::new(("plugin-rows-grid", &id)).striped(true).num_columns(cols.len()).show(ui, |ui| {
                for col in &cols {
                    ui.label(RichText::new(col).semibold());
                }
                ui.end_row();
                for (i, row) in rows.iter().enumerate() {
                    for (n, col) in cols.iter().enumerate() {
                        let text = row.get(col).map(cell).unwrap_or_default();
                        if n == 0 {
                            if ui.selectable_label(page.selected_row == Some(i), text).clicked() {
                                page.selected_row = Some(i);
                                page.panels = None;
                            }
                        } else {
                            ui.label(text);
                        }
                    }
                    ui.end_row();
                }
            });
        });
    });
    let Some((i, row)) = app.plugins.page(&id).selected_row.and_then(|i| rows.get(i).map(|r| (i, r.clone()))) else {
        ui.label(RichText::new("Pick a row to see its details and actions.").weak());
        return;
    };
    let key = row_key(&row, i);
    let folder = row.get("folder").map(cell).filter(|f| plugin::safe_relative(f)).map(|f| root.join(f)).or_else(|| app.plugins.page(&id).row_folders.get(&key).cloned());
    egui::ScrollArea::vertical().id_salt(("plugin-row-detail", &id)).auto_shrink(false).show(ui, |ui| {
        let title = cols.first().and_then(|c| row.get(c)).map(cell).unwrap_or_else(|| key.clone());
        ui.label(RichText::new(title).heading());
        egui::Grid::new(("plugin-row-fields", &id)).num_columns(2).show(ui, |ui| {
            for (k, v) in row.as_object().into_iter().flatten() {
                ui.label(RichText::new(k).weak());
                ui.label(cell(v));
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            for action in &m.actions {
                row_action(app, ui, c, root, &row, &key, folder.as_deref(), action);
            }
        });
        if let Some(dir) = folder.as_ref().filter(|d| d.is_dir()) {
            ui.add_space(8.0);
            refresh_panels(app, &id, c, dir, false);
            panels(app, ui, &id, c, dir);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn row_action(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, root: &Path, row: &Value, key: &str, folder: Option<&Path>, action: &plugin::ActionDef) {
    let id = c.plugin.manifest.id.clone();
    let busy = job_in(app, root).is_some();
    match &action.kind {
        ActionKind::Step { step } => {
            let Some(s) = c.plugin.manifest.step(step).cloned() else { return };
            let why = c.plugin.ensure_runnable().err().map(|e| format!("{e:#}")).or_else(|| busy.then(|| "Another step is running.".to_string()));
            let resp = ui.add_enabled(why.is_none(), egui::Button::new(&action.label));
            let resp = if action.description.is_empty() { resp } else { resp.on_hover_text(&action.description) };
            if let Some(why) = why {
                resp.on_disabled_hover_text(why);
            } else if resp.clicked() {
                let mut params = Map::new();
                params.insert("selection".into(), row.clone());
                start_with_gate(app, &c.plugin, &s, root, params, Purpose::Row(key.to_string()));
            }
        }
        ActionKind::HandoffWorkItem { plugin: target, files, title } => {
            let target_ctx = context(app, target);
            let why = match (&target_ctx, folder) {
                (None, _) => Some(format!("Install the {target} plugin first.")),
                (Some(t), _) if t.root.is_none() => Some(format!("Set up {} in Settings first.", t.plugin.manifest.name)),
                (_, None) => Some("Run the step that prepares this item first.".into()),
                _ => None,
            };
            let resp = ui.add_enabled(why.is_none(), egui::Button::new(&action.label));
            let resp = if action.description.is_empty() { resp } else { resp.on_hover_text(&action.description) };
            if let Some(why) = why {
                resp.on_disabled_hover_text(why);
            } else if resp.clicked()
                && let (Some(t), Some(from)) = (target_ctx, folder)
            {
                let name = row.get("name").or_else(|| row.get("title")).map(cell).unwrap_or_else(|| key.to_string());
                let title = if title.is_empty() { name } else { title.replace("{name}", &name) };
                let origin = Origin { kind: "handoff".into(), from_plugin: id.clone(), key: key.to_string() };
                let target_root = t.root.clone().unwrap_or_default();
                match plugin::hand_off(&t.plugin.manifest, &target_root, from, files, &title, "", origin, SystemTime::now()) {
                    Ok(item) => {
                        let page = app.plugins.page(target);
                        page.refresh_items(&target_root);
                        page.selected = Some(item.dir.clone());
                        if let Some(v) = app.plugins.view_of(target) {
                            app.view = v;
                        }
                        app.notify(format!("Started {} in {}.", item.meta.title, t.plugin.manifest.name));
                    }
                    Err(e) => app.error(format!("{e:#}")),
                }
            }
        }
        ActionKind::HandoffRecipe { .. } => {
            ui.add_enabled(false, egui::Button::new(&action.label)).on_disabled_hover_text("Recipe handoffs aren't available in this build yet.");
        }
    }
}

// ---------------------------------------------------------------- panels

fn refresh_panels(app: &mut StudioApp, id: &str, c: &Ctx, dir: &Path, force: bool) {
    let page = app.plugins.page(id);
    let fresh = page.panels.as_ref().is_some_and(|p| p.dir == dir && p.built.elapsed().as_secs() < 3);
    if fresh && !force {
        return;
    }
    let mut cache = PanelCache {
        dir: dir.to_path_buf(),
        built: Instant::now(),
        request: ystudio_core::read_text(&dir.join("ticket.md")).ok().map(Arc::new),
        tables: HashMap::new(),
        markdown: HashMap::new(),
        files: HashMap::new(),
    };
    for p in &c.plugin.manifest.page.panels {
        match p.kind {
            PanelKind::Table => {
                let rows = p.file.as_ref().map(|f| dir.join(f)).ok_or_else(String::new).and_then(|path| {
                    let text = ystudio_core::read_text(&path).map_err(|_| String::new())?;
                    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{e}"))?;
                    let rows = match &p.rows {
                        Some(ptr) => doc.pointer(ptr).cloned().unwrap_or(Value::Null),
                        None => doc,
                    };
                    Ok(match rows {
                        Value::Array(a) => a,
                        Value::Null => Vec::new(),
                        other => vec![other],
                    })
                });
                cache.tables.insert(p.title.clone(), rows);
            }
            PanelKind::Markdown => {
                let text = p.file.as_ref().and_then(|f| ystudio_core::read_text(&dir.join(f)).ok()).map(Arc::new);
                cache.markdown.insert(p.title.clone(), text);
            }
            PanelKind::Files => {
                cache.files.insert(p.title.clone(), plugin::glob_files(dir, &p.globs));
            }
        }
    }
    page.panels = Some(cache);
}

fn panels(app: &mut StudioApp, ui: &mut Ui, id: &str, c: &Ctx, dir: &Path) {
    for p in &c.plugin.manifest.page.panels {
        let page = app.plugins.page(id);
        let Some(cache) = page.panels.as_ref() else { return };
        match p.kind {
            PanelKind::Table => {
                let Some(Ok(rows)) = cache.tables.get(&p.title).cloned() else { continue };
                if rows.is_empty() {
                    continue;
                }
                let cols = columns(&p.columns, &rows);
                egui::CollapsingHeader::new(&p.title).id_salt(("plugin-panel", id, &p.title)).default_open(true).show(ui, |ui| {
                    egui::ScrollArea::horizontal().id_salt(("plugin-panel-scroll", id, &p.title)).show(ui, |ui| {
                        egui::Grid::new(("plugin-panel-grid", id, &p.title)).striped(true).num_columns(cols.len()).show(ui, |ui| {
                            for col in &cols {
                                ui.label(RichText::new(col).semibold());
                            }
                            ui.end_row();
                            for row in &rows {
                                for col in &cols {
                                    let text = row.get(col).map(cell).unwrap_or_default();
                                    let short: String = text.chars().take(120).collect();
                                    let resp = ui.label(&short);
                                    if short.len() < text.len() {
                                        resp.on_hover_text(text);
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    });
                });
            }
            PanelKind::Markdown => {
                let Some(Some(text)) = cache.markdown.get(&p.title).cloned() else { continue };
                egui::CollapsingHeader::new(&p.title).id_salt(("plugin-panel", id, &p.title)).default_open(true).show(ui, |ui| {
                    CommonMarkViewer::new().show(ui, &mut app.md_cache, &text);
                });
            }
            PanelKind::Files => {
                let Some(files) = cache.files.get(&p.title).cloned() else { continue };
                if files.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(&p.title).id_salt(("plugin-panel", id, &p.title)).default_open(true).show(ui, |ui| {
                    for f in &files {
                        if ui.link(f).clicked() {
                            super::open_external(&dir.join(f));
                        }
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------- modals

/// The current plugin page's prompts. Called every frame.
pub fn modals(app: &mut StudioApp, ctx: &egui::Context) {
    let View::Plugin(tab) = app.view else { return };
    let Some(id) = app.plugins.tabs.get(tab as usize).cloned() else { return };
    let Some(c) = context(app, &id) else { return };
    new_item_modal(app, ctx, &c);
    import_modal(app, ctx, &c);
    prompt_modal(app, ctx, &c);
}

fn new_item_modal(app: &mut StudioApp, ctx: &egui::Context, c: &Ctx) {
    let id = c.plugin.manifest.id.clone();
    let m = &c.plugin.manifest;
    let Some(root) = c.root.clone() else { return };
    let Some(mut form) = app.plugins.page(&id).new_item.take() else { return };
    let text_intake = m.page.intake.is_empty() || m.page.intake.iter().any(|i| i == "text");
    let file_intake = m.page.intake.iter().any(|i| i == "files");
    let (mut create, mut cancel) = (false, false);
    egui::Modal::new(egui::Id::unique(("plugin-new", &id))).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.label(RichText::new(format!("New {}", m.page.noun.to_lowercase())).heading());
        ui.add_space(6.0);
        egui::Grid::new("plugin-new-grid").num_columns(2).show(ui, |ui| {
            ui.label("Title");
            ui.add(egui::TextEdit::singleline(&mut form.title).hint_text("e.g. Mplus 8.11").desired_width(400.0));
            ui.end_row();
            ui.label("Reference");
            ui.add(egui::TextEdit::singleline(&mut form.reference).hint_text("Ticket number (optional)").desired_width(400.0));
            ui.end_row();
        });
        if text_intake {
            ui.label("Request");
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut form.text).desired_rows(8).desired_width(f32::INFINITY).hint_text("Paste the ticket or request text"));
            });
        }
        if file_intake {
            ui.horizontal(|ui| {
                ui.label(format!("Files ({})", form.files.len()));
                if ui.button("\u{E710}  Add files…").clicked()
                    && let Some(files) = rfd::FileDialog::new().pick_files()
                {
                    form.files.extend(files);
                }
            });
            let mut remove = None;
            for (i, f) in form.files.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("\u{E711}").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.label(f.display().to_string());
                });
            }
            if let Some(i) = remove {
                form.files.remove(i);
            }
        }
        if let Some(e) = &form.error {
            ui.label(RichText::new(e).color(theme::red()));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            create = theme::primary(ui, !form.title.trim().is_empty(), "Create").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel {
        return;
    }
    if create {
        let kind = if form.files.is_empty() { "text" } else { "files" };
        match create_item(&root, &m.page.folder, &form.title, &form.reference, &form.text, &form.files, Origin { kind: kind.into(), ..Default::default() }) {
            Ok(item) => {
                let page = app.plugins.page(&id);
                page.refresh_items(&root);
                page.selected = Some(item.dir);
                return;
            }
            Err(e) => form.error = Some(format!("{e:#}")),
        }
    }
    app.plugins.page(&id).new_item = Some(form);
}

fn create_item(root: &Path, pattern: &str, title: &str, reference: &str, text: &str, files: &[PathBuf], origin: Origin) -> anyhow::Result<WorkItem> {
    let item = WorkItem::create(root, pattern, title, reference, origin, SystemTime::now())?;
    if !text.trim().is_empty() {
        std::fs::write(item.dir.join("ticket.md"), text)?;
    }
    if !files.is_empty() {
        let intake = item.dir.join("intake");
        std::fs::create_dir_all(&intake)?;
        for f in files {
            if let Some(name) = f.file_name() {
                std::fs::copy(f, intake.join(name))?;
            }
        }
    }
    Ok(item)
}

fn import_modal(app: &mut StudioApp, ctx: &egui::Context, c: &Ctx) {
    let id = c.plugin.manifest.id.clone();
    let Some(root) = c.root.clone() else { return };
    let Some(mut rows) = app.plugins.page(&id).import.take() else { return };
    let (mut create, mut cancel) = (false, false);
    egui::Modal::new(egui::Id::unique(("plugin-import", &id))).show(ctx, |ui| {
        ui.set_width(640.0);
        ui.label(RichText::new("Import").heading());
        ui.label(RichText::new(format!("Each ticked item becomes a {}.", c.plugin.manifest.page.noun.to_lowercase())).weak());
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for (keep, row) in rows.iter_mut() {
                let title = row.get("title").map(cell).unwrap_or_default();
                let reference = row.get("reference").map(cell).unwrap_or_default();
                ui.checkbox(keep, format!("{reference}  {title}").trim().to_string());
            }
        });
        ui.horizontal(|ui| {
            let n = rows.iter().filter(|(k, _)| *k).count();
            create = theme::primary(ui, n > 0, format!("Create {n}")).clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel {
        return;
    }
    if !create {
        app.plugins.page(&id).import = Some(rows);
        return;
    }
    let mut made = 0;
    let mut problems = Vec::new();
    for (_, row) in rows.iter().filter(|(k, _)| *k) {
        let title = row.get("title").map(cell).unwrap_or_default();
        let reference = row.get("reference").map(cell).unwrap_or_default();
        let text = row.get("text").map(cell).unwrap_or_default();
        let origin = Origin { kind: "import".into(), key: row.get("key").map(cell).unwrap_or_else(|| reference.clone()), ..Default::default() };
        match create_item(&root, &c.plugin.manifest.page.folder, &title, &reference, &text, &[], origin) {
            Ok(_) => made += 1,
            Err(e) => problems.push(format!("{title}: {e:#}")),
        }
    }
    app.plugins.page(&id).refresh_items(&root);
    if problems.is_empty() {
        app.notify(format!("Imported {made} item(s)."));
    } else {
        app.error(format!("Imported {made} item(s); skipped {}: {}", problems.len(), problems.join("; ")));
    }
}

fn prompt_modal(app: &mut StudioApp, ctx: &egui::Context, c: &Ctx) {
    let id = c.plugin.manifest.id.clone();
    let Some(mut prompt) = app.plugins.page(&id).prompt.take() else { return };
    let mut keep = true;
    let mut go: Option<(PathBuf, String, Map<String, Value>, Purpose)> = None;
    match &mut prompt {
        Prompt::Confirm { dir, step, params, purpose } => {
            let Some(s) = c.plugin.manifest.step(step) else { return };
            egui::Modal::new(egui::Id::unique(("plugin-confirm", &id))).show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(RichText::new(s.label()).heading());
                if !s.description.is_empty() {
                    ui.label(&s.description);
                }
                ui.horizontal(|ui| {
                    theme::badge(ui, s.risk.label(), risk_color(s.risk));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::primary(ui, true, "Run").clicked() {
                        go = Some((dir.clone(), step.clone(), params.clone(), purpose.clone()));
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            });
            if let Some((dir, step, params, purpose)) = go.take() {
                run_step(app, &id, &dir, &step, params, true, purpose);
            }
        }
        Prompt::Approve { dir, step, params, purpose, file, sha256, summary, changed, checked, ack } => {
            let Some(s) = c.plugin.manifest.step(step).cloned() else { return };
            if checked.elapsed().as_secs() >= 1 {
                *checked = Instant::now();
                *changed = plugin::sha256_file(&dir.join(&*file)).map_or(true, |now| !now.eq_ignore_ascii_case(sha256));
            }
            let mut approve = false;
            egui::Modal::new(egui::Id::unique(("plugin-approve", &id))).show(ctx, |ui| {
                ui.set_width(680.0);
                ui.label(RichText::new(format!("\u{E72E}  Approve: {}", s.label())).heading().color(risk_color(s.risk)));
                ui.horizontal(|ui| {
                    theme::badge(ui, s.risk.label(), risk_color(s.risk));
                    ui.label(RichText::new(&c.plugin.manifest.name).weak());
                });
                if !s.description.is_empty() {
                    ui.label(&s.description);
                }
                ui.add_space(6.0);
                if let Some(text) = summary.as_ref() {
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        CommonMarkViewer::new().show(ui, &mut app.md_cache, text);
                    });
                } else {
                    ui.label(RichText::new("No summary was written; open the file to review it.").weak());
                }
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label("File:");
                    if ui.link(&*file).clicked() {
                        super::open_external(&dir.join(&*file));
                    }
                });
                ui.label(RichText::new(format!("SHA-256 {sha256}")).monospace().small());
                if *changed {
                    ui.label(RichText::new("The file changed while this was open. Close and review it again.").color(theme::red()));
                }
                ui.checkbox(ack, format!("I approve exactly this {file} (SHA-256 {}…).", &sha256[..12.min(sha256.len())]));
                ui.horizontal(|ui| {
                    approve = theme::primary(ui, *ack && !*changed, "Approve and run").clicked();
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            });
            if approve {
                keep = false;
                let granted = plugin::grant(dir, &c.plugin, &s, sha256, SystemTime::now());
                match granted {
                    Ok(_) => {
                        run_step(app, &id, dir, step, params.clone(), false, purpose.clone());
                    }
                    Err(e) => app.error(format!("{e:#}")),
                }
            }
        }
    }
    if keep {
        app.plugins.page(&id).prompt = Some(prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::{fake_app, render};

    /// Installs the core crate's sample plugin, made to fit this Studio, and
    /// points its work folder into `dir`. `edit` adjusts the manifest.
    fn install_sample(app: &mut StudioApp, dir: &Path, id: &str, edit: impl Fn(String) -> String) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"..\ystudio-core\tests\fixtures\plugins\sample");
        let copy = dir.join("src").join(id);
        for rel in ["plugin.yaml", r"scripts\Echo.ps1", r"agent\INSTRUCTIONS.md", r"agent\tasks\triage.md"] {
            std::fs::create_dir_all(copy.join(rel).parent().unwrap()).unwrap();
            std::fs::copy(src.join(rel), copy.join(rel)).unwrap();
        }
        let manifest = std::fs::read_to_string(copy.join("plugin.yaml")).unwrap().replace("\r\n", "\n");
        let manifest = edit(manifest.replace("minVersion: 0.6.0", &format!("minVersion: {}", super::super::update::CURRENT_VERSION)).replace("id: sample-plugin", &format!("id: {id}")));
        std::fs::write(copy.join("plugin.yaml"), manifest).unwrap();
        let pkg = plugin::Package::from_bytes(&plugin::zip_dir(&copy).unwrap()).unwrap();
        let plugins = dir.join("plugins");
        plugin::install(&pkg, &plugins, super::super::update::CURRENT_VERSION, "test", false).unwrap();
        app.plugins = crate::plugins::Host::load(&plugins);
        let root = dir.join("work").join(id);
        std::fs::create_dir_all(&root).unwrap();
        app.settings.plugins.entry(id.into()).or_default().values.insert("workRoot".into(), root.to_string_lossy().into());
        root
    }

    #[test]
    fn plugin_tab_pages_and_approval_render() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        assert_eq!(app.plugins.view_of("sample-plugin"), Some(View::Plugin(0)));
        let item = WorkItem::create(&root, "{date}-{ref}-{slug}", "Mplus 8.11", "TASK1", Origin::default(), SystemTime::now()).unwrap();
        std::fs::write(item.dir.join("ticket.md"), "Please package Mplus 8.11 for the Psychology lab.").unwrap();
        std::fs::write(item.dir.join("notes.md"), "# Notes\n\nMplus needs a licence file.").unwrap();
        app.plugins.page("sample-plugin").selected = Some(item.dir.clone());
        app.view = View::Plugin(0);
        render(&mut app, "plugin-workitems", 1315, 697);
        render(&mut app, "plugin-workitems-small", 900, 600);

        std::fs::write(item.dir.join("change.json"), r#"{"apps":["mplus"]}"#).unwrap();
        std::fs::write(item.dir.join("change.md"), "## Change\n\nUpload **Mplus 8.11** and assign it to the Testing group.").unwrap();
        let installed = app.plugins.get("sample-plugin").unwrap().clone();
        let step = installed.manifest.step("approve").unwrap().clone();
        start_with_gate(&mut app, &installed, &step, &item.dir, Map::new(), Purpose::Item);
        assert!(matches!(app.plugins.page("sample-plugin").prompt, Some(Prompt::Approve { .. })), "a hash-gated step asks for approval");
        render(&mut app, "plugin-approve", 1315, 697);
        assert!(app.plugins.page("sample-plugin").prompt.is_some(), "the prompt stays until answered");
        assert!(app.jobs.is_empty(), "nothing ran without approval");

        // Turning the plugin off hides its page.
        app.settings.plugins.get_mut("sample-plugin").unwrap().enabled = false;
        render(&mut app, "plugin-off", 900, 600);

        // Settings lists it, and a package opens the trust prompt.
        app.plugins.page("sample-plugin").prompt = None;
        app.settings.plugins.get_mut("sample-plugin").unwrap().enabled = true;
        app.view = View::Settings;
        app.plugins_ui.open_settings = Some("sample-plugin".into());
        render(&mut app, "plugin-settings", 1315, 900);
        let zip = dir.path().join("sample-plugin-1.0.0.zip");
        std::fs::write(&zip, plugin::zip_dir(&app.plugins.get("sample-plugin").unwrap().root).unwrap()).unwrap();
        super::super::plugins::open_package(&mut app, zip);
        render(&mut app, "plugin-trust", 1315, 900);
    }

    #[test]
    fn catalog_page_renders_rows_and_actions() {
        let (dir, mut app) = fake_app();
        install_sample(&mut app, dir.path(), "sample-catalog", |m| {
            m.replace("  type: workitems\n", "  type: catalog\n  query: search\n  columns: [name, version]\n")
                .replace("steps:\n", "steps:\n  - id: search\n    label: Search\n    script: scripts/Echo.ps1\n    risk: local-read\n    scope: plugin\n")
                .replace("actions:\n", "actions:\n  - id: export\n    label: Export\n    kind: step\n    step: echo\n")
                .replace("  plugins:\n    - id: other-plugin\n      minVersion: 1.0.0\n", "  plugins:\n    - id: other-plugin\n")
        });
        let page = app.plugins.page("sample-catalog");
        page.rows = vec![serde_json::json!({"key": "app-1", "name": "Spectragryph", "version": "1.2.16"}), serde_json::json!({"key": "app-2", "name": "Mplus", "version": "8.11"})];
        page.selected_row = Some(0);
        app.view = View::Plugin(0);
        render(&mut app, "plugin-catalog", 1315, 697);
        assert_eq!(app.plugins.page("sample-catalog").selected_row, Some(0));
    }

    #[test]
    fn steps_run_as_jobs_and_record_results() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let item = WorkItem::create(&root, "{date}-{ref}-{slug}", "Echo", "", Origin::default(), SystemTime::now()).unwrap();
        assert!(run_step(&mut app, "sample-plugin", &item.dir, "echo", Map::new(), false, Purpose::Item));
        assert!(Lock::holder(&item.dir).is_some(), "the work item is locked while the step runs");
        assert_eq!(WorkItem::open(&item.dir).unwrap().step("echo").state, StepState::Running);
        assert!(!run_step(&mut app, "sample-plugin", &item.dir, "echo", Map::new(), false, Purpose::Item), "one step at a time per work item");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while app.jobs.iter().any(|j| j.proc.is_running()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        app.poll_for_tests();
        let rec = WorkItem::open(&item.dir).unwrap().step("echo");
        assert_eq!(rec.state, StepState::Ok, "{rec:?}");
        assert_eq!(rec.summary, "Echoed hello");
        assert!(Lock::holder(&item.dir).is_none() && app.plugins.locks.is_empty(), "the lock is released");

        // A confirm-gated step won't start unconfirmed.
        assert!(!run_step(&mut app, "sample-plugin", &item.dir, "confirmed", Map::new(), false, Purpose::Item));
        assert!(Lock::holder(&item.dir).is_none(), "a refused step leaves no lock");
    }
}
