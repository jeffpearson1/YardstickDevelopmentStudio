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
use ystudio_core::agent::{self, Change, Evidence, PluginAgentContext, Snapshot};
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
    /// What the latest agent run changed, by work item.
    agent_changes: HashMap<PathBuf, Vec<Change>>,
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

/// The running plugin step or agent task in `dir`, if any, by job and id.
fn job_in(app: &StudioApp, dir: &Path) -> Option<(u64, String)> {
    app.jobs.iter().find_map(|j| match &j.kind {
        JobKind::PluginStep { dir: d, step: id, .. } | JobKind::PluginAgent { dir: d, task: id, .. } if j.proc.is_running() && d == dir => Some((j.id, id.clone())),
        _ => None,
    })
}

pub fn plugin_busy(app: &StudioApp, id: &str) -> bool {
    app.jobs.iter().any(|j| {
        j.proc.is_running()
            && match &j.kind {
                JobKind::PluginStep { plugin, .. } | JobKind::PluginAgent { plugin, .. } | JobKind::PluginClone { plugin, .. } => plugin == id,
                _ => false,
            }
    })
}

/// A plugin with a separate private repository does not touch the active Yardstick
/// checkout, so its file-only agent work may run beside Recipe Builder.
pub fn has_private_workspace(app: &StudioApp, id: &str) -> bool {
    let Some(installed) = app.plugins.get(id) else { return false };
    let Some(def) = installed.manifest.settings.iter().find(|setting| setting.clone_repository) else { return false };
    let Some(path) = app.settings.plugins.get(id).and_then(|prefs| prefs.values.get(&def.key)).map(PathBuf::from) else { return false };
    path.is_absolute() && app.ws.as_ref().is_none_or(|workspace| !ystudio_core::same_path(&workspace.root, &path))
}

fn isolated_agent_conflict(app: &StudioApp, id: &str) -> bool {
    app.jobs.iter().any(|job| {
        job.proc.is_running()
            && match &job.kind {
                JobKind::PluginAgent { plugin, .. } => plugin == id,
                JobKind::PluginStep { risk: Risk::LocalInstall, .. } | JobKind::BatchExtract { .. } => true,
                _ => false,
            }
    })
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
    egui::Panel::left(("plugin-items", &id)).resizable(true).default_size(300.0).frame(theme::pane_frame()).show(ui, |ui| {
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
    theme::pane(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(("plugin-detail", &id)).auto_shrink(false).show(ui, |ui| match selected.and_then(|d| WorkItem::open(&d).ok()) {
            Some(item) if item.dir.is_dir() => item_detail(app, ui, c, &item),
            _ => {
                ui.add_space(24.0);
                ui.label(RichText::new(format!("Pick a {} on the left, or start a new one.", m.page.noun.to_lowercase())).weak());
            }
        });
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
        if ui
            .small_button("\u{E70F}  Continue in Recipe Builder")
            .on_hover_text("Use this request to create a reusable Yardstick recipe instead; the plugin work item is kept.")
            .clicked()
        {
            let request = ystudio_core::read_text(&item.dir.join("ticket.md")).unwrap_or_default();
            let notes = format!(
                "Continued from {} work item '{}'.\n\n{}",
                c.plugin.manifest.name,
                item.name(),
                request
            );
            crate::views::batch::add_plugin_work_item(app, &item.meta.title, &item.meta.reference, &notes);
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
    if m.agent.as_ref().is_some_and(|a| !a.tasks.is_empty()) {
        egui::CollapsingHeader::new("Agent options").id_salt(("plugin-agent-options", &id)).default_open(false).show(ui, |ui| {
            let agent_available = if has_private_workspace(app, &id) { !isolated_agent_conflict(app, &id) } else { !crate::workflow::busy(app) };
            ui.add_enabled_ui(agent_available, |ui| {
                super::settings::agent_picker(app, ui, &format!("plugin-{id}"));
                super::settings::downloads_checkbox(app, ui);
            });
        });
    }
    let busy_elsewhere = if has_private_workspace(app, &id) { isolated_agent_conflict(app, &id) } else { crate::workflow::busy(app) };
    for stage in m.pipeline() {
        let rec = item.step(stage.id());
        let state = if running.as_ref().is_some_and(|(_, s)| s == stage.id()) { StepState::Running } else { rec.state };
        theme::card(ui, |ui| {
            // Wide enough, the button sits at the right of the heading; otherwise under it.
            if ui.available_width() > 520.0 {
                ui.horizontal(|ui| {
                    stage_heading(ui, stage, state);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| stage_button(app, ui, c, item, stage, state, running.is_some(), busy_elsewhere));
                });
            } else {
                ui.horizontal_wrapped(|ui| stage_heading(ui, stage, state));
                ui.horizontal(|ui| stage_button(app, ui, c, item, stage, state, running.is_some(), busy_elsewhere));
            }
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
    if let Some(changes) = app.plugins.page(&id).agent_changes.get(&item.dir).filter(|c| !c.is_empty()).cloned() {
        egui::CollapsingHeader::new(format!("The agent changed {} file(s)", changes.len())).id_salt(("plugin-agent-changes", &id)).default_open(true).show(ui, |ui| {
            for change in &changes {
                let (mark, color) = match change {
                    Change::Added(_) => ("+", theme::green()),
                    Change::Modified(_) => ("~", theme::amber()),
                    Change::Removed(_) => ("-", theme::red()),
                };
                let rel = change.path().strip_prefix(&item.dir).unwrap_or(change.path()).display().to_string();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(mark).monospace().color(color));
                    if matches!(change, Change::Removed(_)) {
                        ui.label(rel);
                    } else if ui.link(rel).clicked() {
                        super::open_external(change.path());
                    }
                });
            }
        });
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

/// A pipeline row's state, name, risk and gate.
fn stage_heading(ui: &mut Ui, stage: Stage<'_>, state: StepState) {
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
}

#[allow(clippy::too_many_arguments)]
fn stage_button(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem, stage: Stage<'_>, state: StepState, item_busy: bool, busy_elsewhere: bool) {
    match stage {
        Stage::Task(t) => {
            let unmet: Vec<String> = t.after.iter().filter(|a| !item.satisfied(std::slice::from_ref(a))).map(|a| stage_label(c, a)).collect();
            let why = if let Err(e) = c.plugin.ensure_runnable() {
                Some(format!("{e:#}"))
            } else if item_busy {
                Some("Something else is running in this work item.".into())
            } else if busy_elsewhere || !app.batch.queue.is_empty() {
                Some("Another agent or test is running.".into())
            } else if !unmet.is_empty() {
                Some(format!("Run {} first.", unmet.join(" and ")))
            } else {
                None
            };
            let text = if state == StepState::NotRun { "\u{E99A}  Run agent" } else { "\u{E72C}  Run agent again" };
            let resp = ui.add_enabled(why.is_none(), egui::Button::new(text));
            if let Some(why) = why {
                resp.on_disabled_hover_text(why);
            } else if resp.on_hover_text(format!("Runs {} with this task. It may only change files in this work item.", app.settings.agent_backend.label())).clicked() {
                run_agent(app, &c.plugin.manifest.id, &item.dir, &t.id);
            }
        }
        Stage::Step(s) => step_button(app, ui, c, item, s, state, item_busy, busy_elsewhere),
    }
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
    egui::Panel::left(("plugin-rows", &id)).resizable(true).default_size(520.0).frame(theme::pane_frame()).show(ui, |ui| {
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
        theme::pane(ui, |ui| ui.label(RichText::new("Pick a row to see its details and actions.").weak()));
        return;
    };
    let key = row_key(&row, i);
    let folder = row.get("folder").map(cell).filter(|f| plugin::safe_relative(f)).map(|f| root.join(f)).or_else(|| app.plugins.page(&id).row_folders.get(&key).cloned());
    theme::pane(ui, |ui| egui::ScrollArea::vertical().id_salt(("plugin-row-detail", &id)).auto_shrink(false).show(ui, |ui| {
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
    }));
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
            let missing = app.plugins.registry.missing_requirements(&c.plugin);
            let why = match (&target_ctx, folder) {
                (None, _) => Some(format!("Install the {target} plugin first.")),
                (Some(t), _) if t.root.is_none() => Some(format!("Set up {} in Settings first.", t.plugin.manifest.name)),
                (Some(t), _) if t.plugin.ensure_runnable().is_err() => t.plugin.ensure_runnable().err().map(|e| format!("{e:#}")),
                _ if !missing.is_empty() => Some(format!("This plugin {}.", missing.join(", "))),
                (_, None) => Some("Run the step that prepares this item first.".into()),
                _ => None,
            };
            let resp = ui.add_enabled(why.is_none(), egui::Button::new(&action.label));
            let resp = if action.description.is_empty() { resp } else { resp.on_hover_text(&action.description) };
            if let Some(why) = why {
                resp.on_disabled_hover_text(why);
            } else if resp.clicked()
                && let Some(from) = folder
            {
                start_handoff(app, &id, target, files, title, row, key, from);
            }
        }
        ActionKind::HandoffRecipe { notes_file } => {
            let why = if !app.writable() {
                Some("Open a writable development workspace first.".to_string())
            } else if !app.ws_cache.has_skill {
                Some("This workspace has no yardstick-recipes skill; pull the latest Yardstick.".to_string())
            } else if folder.is_none() {
                Some("Run the step that prepares this item first.".to_string())
            } else {
                None
            };
            let resp = ui.add_enabled(why.is_none(), egui::Button::new(&action.label));
            let resp = if action.description.is_empty() { resp } else { resp.on_hover_text(&action.description) };
            if let Some(why) = why {
                resp.on_disabled_hover_text(why);
            } else if resp.clicked()
                && let Some(from) = folder
            {
                recipe_handoff(app, row, key, &from.join(notes_file));
            }
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

/// Creates a plugin work item directly from a Recipe Builder request and opens it.
pub fn create_from_recipe_builder(
    app: &mut StudioApp,
    plugin_id: &str,
    title: &str,
    reference: &str,
    text: &str,
    key: &str,
) -> anyhow::Result<PathBuf> {
    let c = context(app, plugin_id).ok_or_else(|| anyhow::anyhow!("The selected plugin is no longer installed."))?;
    c.plugin.ensure_runnable()?;
    let root = c.root.ok_or_else(|| anyhow::anyhow!("Set up {} in Settings first.", c.plugin.manifest.name))?;
    let item = create_item(
        &root,
        &c.plugin.manifest.page.folder,
        title,
        reference,
        text,
        &[],
        Origin { kind: "recipe-builder".into(), from_plugin: "recipe-builder".into(), key: key.to_string() },
    )?;
    let dir = item.dir.clone();
    let page = app.plugins.page(plugin_id);
    page.refresh_items(&root);
    page.selected = Some(dir.clone());
    if let Some(view) = app.plugins.view_of(plugin_id) {
        app.view = view;
    }
    Ok(dir)
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
pub(crate) mod tests {
    use super::*;
    use crate::workflow::tests::{fake_app, render};

    /// Installs the core crate's sample plugin, made to fit this Studio, and
    /// points its work folder into `dir`. `edit` adjusts the manifest.
    pub(crate) fn install_sample(app: &mut StudioApp, dir: &Path, id: &str, edit: impl Fn(String) -> String) -> PathBuf {
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
        render(&mut app, "plugin-workitems-min", crate::app::MIN_LAYOUT.x as u32, crate::app::MIN_LAYOUT.y as u32);

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

    /// The repository's One-Off Packager, installed and showing a ticket part-way through.
    #[test]
    fn the_oneoff_packager_page_renders_a_ticket_in_progress() {
        let (dir, mut app) = fake_app();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"..\..\plugins\oneoff-packager");
        if !src.join("plugin.yaml").is_file() {
            return;
        }
        let pkg = plugin::Package::from_bytes(&plugin::zip_dir(&src).unwrap()).unwrap();
        let plugins = dir.path().join("plugins");
        plugin::install(&pkg, &plugins, super::super::update::CURRENT_VERSION, "test", false).unwrap();
        app.plugins = crate::plugins::Host::load(&plugins);
        let root = dir.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let prefs = app.settings.plugins.entry("oneoff-packager".into()).or_default();
        prefs.values.insert("workRoot".into(), root.to_string_lossy().into());
        prefs.values.insert("tenantId".into(), "11111111-1111-1111-1111-111111111111".into());

        let mut item = WorkItem::create(&root, "{date}-{ref}-{slug}", "FastStone Image Viewer and inPhoto Capture", "TASK1430821", Origin { kind: "text".into(), ..Default::default() }, SystemTime::now()).unwrap();
        std::fs::write(item.dir.join("ticket.md"), "Please add FastStone Image Viewer 8.5 and inPhoto Capture PS to Company Portal for the photography lab.").unwrap();
        std::fs::write(item.dir.join("products.json"), r#"{"products":[
            {"id":"faststoneimageviewer","name":"FastStone Image Viewer","version":"8.5","status":"locally-validated","notes":"Vendor installer, unsigned"},
            {"id":"inphotocaptureps","name":"inPhoto Capture PS","version":"4.18.42","status":"needs-research","notes":"Vendor link in the ticket"}]}"#).unwrap();
        std::fs::write(item.dir.join("research-and-status.md"), "# Research\n\nBoth installers come from the vendors' own sites.\n\n- FastStone: `/S` silent switch confirmed in the vendor FAQ.").unwrap();
        for (id, state, summary) in [
            ("parse-ticket", StepState::Ok, "2 link(s), 0 Dropbox, 1 asset tag(s)."),
            ("triage", StepState::Ok, "Finished: 1 file(s) changed."),
            ("runtime", StepState::Ok, "Created the runtime and proved its DELETE guard. YardstickDev commit 2a28d70a."),
            ("inventory", StepState::Ok, "Scanned 412 apps in Contoso: 0 possible existing app(s)."),
            ("research", StepState::Ok, "Finished: 4 file(s) changed."),
            ("prepare-test", StepState::Warn, "Prepare test run 20261113-101500-001: faststoneimageviewer: WhatIf."),
        ] {
            let rec = item.meta.steps.entry(id.into()).or_default();
            rec.state = state;
            rec.summary = summary.into();
            rec.finished = "2026-11-13 10:15:00".into();
        }
        item.save().unwrap();
        app.plugins.page("oneoff-packager").selected = Some(item.dir.clone());
        app.view = View::Plugin(0);
        render(&mut app, "oneoff-packager-ticket", 1500, 1500);

        // The pipeline is in the order the page lists it, and a step is ready only after what it needs.
        let m = &app.plugins.get("oneoff-packager").unwrap().manifest;
        let order: Vec<&str> = m.pipeline().iter().map(|s| s.id()).collect();
        assert_eq!(order, ["parse-ticket", "triage", "runtime", "inventory", "research", "prepare-test", "repair", "full-test-request", "full-test", "build", "prepare-live", "live-change"]);
        let item = WorkItem::open(&item.dir).unwrap();
        assert!(item.satisfied(&m.step("full-test-request").unwrap().after), "Prepare finished with warnings, so the request can be written");
        assert!(!item.satisfied(&m.step("build").unwrap().after), "nothing is built before the install test");
    }

    /// Installs the repository's plugins and gives them settings under `dir`.
    fn install_repo_plugins(app: &mut StudioApp, dir: &Path) -> (PathBuf, PathBuf) {
        let plugins = dir.join("plugins");
        for name in ["oneoff-packager", "sccm-migration"] {
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"..\..\plugins").join(name);
            let pkg = plugin::Package::from_bytes(&plugin::zip_dir(&src).unwrap()).unwrap();
            plugin::install(&pkg, &plugins, super::super::update::CURRENT_VERSION, "test", false).unwrap();
        }
        app.plugins = crate::plugins::Host::load(&plugins);
        let (oneoff, sccm) = (dir.join("oneoff"), dir.join("sccm"));
        let prefs = app.settings.plugins.entry("oneoff-packager".into()).or_default();
        prefs.values.insert("workRoot".into(), oneoff.to_string_lossy().into());
        let prefs = app.settings.plugins.entry("sccm-migration".into()).or_default();
        for (k, v) in [("workRoot", sccm.to_string_lossy().into_owned()), ("smsProvider", "cm1.example.edu".into()), ("siteCode", "ABC".into())] {
            prefs.values.insert(k.into(), v);
        }
        (oneoff, sccm)
    }

    #[test]
    fn recipe_builder_and_oneoff_handoffs_preserve_context_and_private_clone() {
        let (dir, mut app) = fake_app();
        if !PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"..\..\plugins\oneoff-packager\plugin.yaml").is_file() {
            return;
        }
        let (oneoff, _) = install_repo_plugins(&mut app, dir.path());
        let private = dir.path().join("private-yardstick");
        std::fs::create_dir_all(private.join(".git")).unwrap();
        app.settings
            .plugins
            .entry("oneoff-packager".into())
            .or_default()
            .values
            .insert("yardstickDevPath".into(), private.to_string_lossy().into());
        assert!(has_private_workspace(&app, "oneoff-packager"));
        app.plugins_ui.open_settings = Some("oneoff-packager".into());
        app.view = View::Settings;
        render(&mut app, "oneoff-private-clone-setting", 1400, 1900);

        let made = create_from_recipe_builder(
            &mut app,
            "oneoff-packager",
            "Licensed Lab Tool",
            "TASK-42",
            "Requester supplied licensed media.",
            "batch:0",
        )
        .unwrap();
        assert!(made.starts_with(&oneoff));
        assert_eq!(ystudio_core::read_text(&made.join("ticket.md")).unwrap(), "Requester supplied licensed media.");
        let item = WorkItem::open(&made).unwrap();
        assert_eq!((item.meta.origin.kind.as_str(), item.meta.origin.from_plugin.as_str(), item.meta.origin.key.as_str()), ("recipe-builder", "recipe-builder", "batch:0"));

        assert!(crate::views::batch::add_plugin_work_item(
            &mut app,
            &item.meta.title,
            &item.meta.reference,
            "Continued from One-Off Packager.",
        ));
        let returned = app.batch.batch.as_ref().unwrap().items.last().unwrap();
        assert!(returned.is_recipe_work());
        assert_eq!((returned.app.as_str(), returned.source.as_str()), ("Licensed Lab Tool", "TASK-42"));
        render(&mut app, "recipe-builder-plugin-return", 1400, 900);
        render(&mut app, "recipe-builder-plugin-return-narrow", 900, 600);
        let returned = app.batch.batch.as_mut().unwrap().items.last_mut().unwrap();
        returned.handler = "oneoff-packager".into();
        returned.handler_item = Some(made);
        returned.include = false;
        app.batch.selected = Some(app.batch.batch.as_ref().unwrap().items.len() - 1);
        render(&mut app, "recipe-builder-plugin-destination", 1400, 900);
    }

    #[test]
    fn an_sccm_application_is_handed_to_the_oneoff_packager() {
        let (dir, mut app) = fake_app();
        if !PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"..\..\plugins\sccm-migration\plugin.yaml").is_file() {
            return;
        }
        let (oneoff, sccm) = install_repo_plugins(&mut app, dir.path());
        let sccm_tab = app.plugins.view_of("sccm-migration").unwrap();
        let oneoff_tab = app.plugins.view_of("oneoff-packager").unwrap();
        assert_ne!(sccm_tab, oneoff_tab);

        // An application already exported, with its content copied.
        let folder = sccm.join("spectragryph-1-0-7");
        std::fs::create_dir_all(folder.join("source")).unwrap();
        std::fs::write(folder.join("export.md"), "# Migrate Spectragryph 1.0.7 from Configuration Manager to Intune\n\n- Install: `\"setup.exe\" /VERYSILENT`\n- Detection in Intune: **mapped** (registry)\n").unwrap();
        std::fs::write(folder.join("sccm-export.json"), r#"{"application":{"name":"Spectragryph 1.0.7"},"deploymentTypes":[{"title":"Silent Install","technology":"Script","installCommand":"setup.exe /S"}]}"#).unwrap();
        std::fs::write(folder.join("intune-mapping.json"), r#"{"deploymentTypes":[{"title":"Silent Install for Spectragryph","technology":"Script","context":"System","detection":"registry","status":"mapped"}]}"#).unwrap();
        std::fs::write(folder.join("source-manifest.json"), r#"{"fileCount":1}"#).unwrap();
        std::fs::write(folder.join(r"source\setup.exe"), "installer").unwrap();

        let page = app.plugins.page("sccm-migration");
        page.rows = vec![
            serde_json::json!({"key":"ScopeId_X/Application_1/8","name":"Spectragryph 1.0.7","version":"1.0.7","manufacturer":"Dr. Friedrich Menges","state":"active","deployed":true,"folder":"spectragryph-1-0-7"}),
            serde_json::json!({"key":"ScopeId_X/Application_2/3","name":"Zoom Workplace","version":"6.1","manufacturer":"Zoom","state":"active","deployed":true}),
            serde_json::json!({"key":"ScopeId_X/Application_3/1","name":"Adobe Reader","version":"24.1","manufacturer":"Adobe","state":"retired","deployed":false}),
        ];
        page.selected_row = Some(0);
        page.probed = true;
        page.probe = Some(StepResult { status: plugin::ResultStatus::Ok, summary: "Connected to cm1.example.edu over Dcom: site ABC \"Main site\", version 5.00.9132.".into(), ..Default::default() });
        app.view = sccm_tab;
        render(&mut app, "sccm-catalog", 1400, 900);

        // The handoff starts a ticket in the One-Off Packager with the export and the content.
        let m = app.plugins.get("sccm-migration").unwrap().manifest.clone();
        let Some(plugin::ActionDef { kind: ActionKind::HandoffWorkItem { plugin: target, files, title }, .. }) = m.actions.iter().find(|a| a.id == "to-oneoff").cloned() else { panic!("no to-oneoff action") };
        let row = serde_json::json!({"key":"ScopeId_X/Application_1/8","name":"Spectragryph 1.0.7"});
        assert!(start_handoff(&mut app, "sccm-migration", &target, &files, &title, &row, "ScopeId_X/Application_1/8", &folder));
        assert_eq!(app.view, oneoff_tab, "the One-Off Packager's tab opens");
        let items = WorkItem::list(&oneoff);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.meta.title, "SCCM migration: Spectragryph 1.0.7");
        assert_eq!((item.meta.origin.kind.as_str(), item.meta.origin.from_plugin.as_str()), ("handoff", "sccm-migration"));
        for f in ["export.md", "sccm-export.json", "intune-mapping.json", "source-manifest.json", r"source\setup.exe"] {
            assert!(item.dir.join("intake").join(f).is_file(), "{f} was handed over");
        }
        assert!(!item.dir.join(r"intake\icon.png").exists(), "no icon was exported, and that is fine");
        assert_eq!(app.plugins.page("oneoff-packager").selected.as_deref(), Some(item.dir.as_path()));
        render(&mut app, "sccm-handoff-oneoff", 1500, 900);
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

    /// Runs `script` (cmd syntax) as a stand-in for a plugin agent in `dir`,
    /// the way `run_agent` registers one, and waits for the result to be recorded.
    fn fake_agent(app: &mut StudioApp, dir: &Path, script: &str) -> plugin::StepRecord {
        let lock = Lock::acquire(dir, "test agent").unwrap();
        let spec = ystudio_core::process::ProcessSpec::new("agent", "cmd", dir).args(["/d", "/c", script]);
        let kind = JobKind::PluginAgent { plugin: "sample-plugin".into(), dir: dir.to_path_buf(), task: "triage".into(), before: Snapshot::of_dir(dir) };
        let job = app.start_job(spec, kind).unwrap();
        app.plugins.locks.insert(job, lock);
        assert!(crate::workflow::busy(app), "a plugin agent keeps other agents and tests waiting");
        assert!(plugin_busy(app, "sample-plugin") && job_in(app, dir).is_some_and(|(_, id)| id == "triage"));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while app.jobs.iter().any(|j| j.proc.is_running()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        app.poll_for_tests();
        assert!(Lock::holder(dir).is_none() && app.plugins.locks.is_empty(), "the lock is released");
        WorkItem::open(dir).unwrap().step("triage")
    }

    #[test]
    fn agent_results_are_recorded_with_what_stopped_them() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let item = WorkItem::create(&root, "{date}-{ref}-{slug}", "Agent", "", Origin::default(), SystemTime::now()).unwrap();

        // It wrote what the task expects (notes.md).
        let rec = fake_agent(&mut app, &item.dir, "echo # Notes> notes.md");
        assert_eq!((rec.state, rec.summary.as_str()), (StepState::Ok, "Finished: 1 file(s) changed."));
        let changes = &app.plugins.page("sample-plugin").agent_changes[&item.dir];
        assert_eq!(changes, &[Change::Added(item.dir.join("notes.md"))]);
        let _ = std::fs::remove_file(item.dir.join("notes.md"));

        // Done, but the expected file is missing: a warning, not a pass.
        let rec = fake_agent(&mut app, &item.dir, "echo nothing useful");
        assert_eq!(rec.state, StepState::Warn);
        assert!(rec.summary.contains("did not write notes.md"), "{}", rec.summary);

        // The agent says it is blocked: shown as is, as the Studio does for recipes.
        let rec = fake_agent(&mut app, &item.dir, "echo STUDIO-BLOCKED: no installer was supplied");
        assert_eq!(rec.state, StepState::Blocked);
        assert!(rec.summary.contains("Blocked: no installer was supplied"), "{}", rec.summary);

        // A denied download asks the user to approve downloads.
        let rec = fake_agent(&mut app, &item.dir, "echo STUDIO-NEEDS-APPROVAL: downloads");
        assert_eq!(rec.state, StepState::Blocked);
        assert!(rec.summary.contains("Allow the agent to download"), "{}", rec.summary);

        // An error exit is a failure.
        let rec = fake_agent(&mut app, &item.dir, "echo boom & exit /b 3");
        assert_eq!(rec.state, StepState::Fail);
    }

    #[test]
    fn agent_runs_wait_for_other_work() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let item = WorkItem::create(&root, "{date}-{ref}-{slug}", "Busy", "", Origin::default(), SystemTime::now()).unwrap();
        // A running recipe-agent or test holds everything else back.
        let spec = ystudio_core::process::ProcessSpec::new("long", "cmd", dir.path()).args(["/d", "/c", "ping -n 20 127.0.0.1 > nul"]);
        let job = app.start_job(spec, JobKind::Agent { before: Snapshot::default(), batch_item: None, verification: None }).unwrap();
        assert!(!run_agent(&mut app, "sample-plugin", &item.dir, "triage"));
        assert!(app.status.error && app.status.text.contains("Finish the running agent"), "{}", app.status.text);
        assert!(Lock::holder(&item.dir).is_none(), "a refused run leaves no lock");
        app.job_mut(job).unwrap().proc.cancel();
    }

    #[test]
    fn recipe_handoff_fills_recipe_builder() {
        let (dir, mut app) = fake_app();
        let notes = dir.path().join("export.md");
        std::fs::write(&notes, "## Spectragryph\n\nSilent switch: /S\n").unwrap();
        let row = serde_json::json!({"key": "ScopeId_1/Application_2", "name": "Spectragryph optical spectroscopy", "manufacturer": "Dr. F. Menges"});
        recipe_handoff(&mut app, &row, "ScopeId_1/Application_2", &notes);
        assert_eq!(app.view, View::Batch);
        assert_eq!((app.batch.new_recipe.display.as_str(), app.batch.new_recipe.publisher.as_str()), ("Spectragryph optical spectroscopy", "Dr. F. Menges"));
        assert!(app.batch.new_recipe.notes.contains("Silent switch: /S"));
        assert!(ystudio_core::recipe::is_valid_id(&app.batch.new_recipe.id), "{}", app.batch.new_recipe.id);
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

#[cfg(test)]
mod small_window_tests {
    use super::*;
    use crate::app::MIN_LAYOUT;
    use crate::workflow::tests::{fake_app, render};

    /// Below the minimum layout size the whole window scrolls over a layout of
    /// that size; the pages never get squeezed into the smaller window.
    #[test]
    fn small_windows_scroll_instead_of_overlapping() {
        let (_dir, mut app) = fake_app();
        for (name, view) in [("home", View::Home), ("recipes", View::Recipes), ("batch", View::Batch), ("history", View::Runs), ("publish", View::Promote), ("settings", View::Settings)] {
            app.view = view;
            for (w, h) in [(700u32, 450u32), (900, 600), (1100, 500), (MIN_LAYOUT.x as u32, MIN_LAYOUT.y as u32)] {
                render(&mut app, &format!("small-{name}-{w}x{h}"), w, h);
            }
        }
    }
}

/// Starts a work item in another plugin from an exported folder, and goes to its tab.
#[allow(clippy::too_many_arguments)]
fn start_handoff(app: &mut StudioApp, from_id: &str, target: &str, files: &[String], title: &str, row: &Value, key: &str, from: &Path) -> bool {
    let Some(t) = context(app, target) else { return false };
    let Some(target_root) = t.root.clone() else { return false };
    let name = row.get("name").or_else(|| row.get("title")).map(cell).unwrap_or_else(|| key.to_string());
    let title = if title.is_empty() { name } else { title.replace("{name}", &name) };
    let origin = Origin { kind: "handoff".into(), from_plugin: from_id.to_string(), key: key.to_string() };
    if let Err(e) = std::fs::create_dir_all(&target_root) {
        app.error(format!("Could not create {}: {e}", target_root.display()));
        return false;
    }
    match plugin::hand_off(&t.plugin.manifest, &target_root, from, files, &title, "", origin, SystemTime::now()) {
        Ok(item) => {
            let page = app.plugins.page(target);
            page.refresh_items(&target_root);
            page.selected = Some(item.dir.clone());
            if let Some(v) = app.plugins.view_of(target) {
                app.view = v;
            }
            app.notify(format!("Started {} in {}.", item.meta.title, t.plugin.manifest.name));
            true
        }
        Err(e) => {
            app.error(format!("{e:#}"));
            false
        }
    }
}

// ---------------------------------------------------------------- agent

/// Runs an agent task on a work item: the agent works in its folder only,
/// with the plugin's instructions, and the result is recorded when it ends.
pub fn run_agent(app: &mut StudioApp, id: &str, dir: &Path, task_id: &str) -> bool {
    let Some(c) = context(app, id) else { return false };
    let (Some(root), Ok(_)) = (c.root.clone(), &c.settings) else {
        app.error(format!("{} needs its settings first.", c.plugin.manifest.name));
        return false;
    };
    let Some(task) = c.plugin.manifest.task(task_id).cloned() else { return false };
    let busy = if has_private_workspace(app, id) { isolated_agent_conflict(app, id) } else { crate::workflow::busy(app) || !app.batch.queue.is_empty() };
    if busy {
        app.error("Finish the running agent or test first.");
        return false;
    }
    if !plugin::is_within(dir, &root) {
        app.error(format!("{} is outside the plugin's work folder.", dir.display()));
        return false;
    }
    let Ok(item) = WorkItem::open(dir) else { return false };
    let evidence: Vec<Evidence> = task
        .evidence_from
        .iter()
        .filter_map(|sid| {
            let rec = item.meta.steps.get(sid).filter(|r| r.state != StepState::NotRun)?;
            let state = match rec.state {
                StepState::Ok => "passed",
                StepState::Warn => "passed with warnings",
                StepState::Fail => "failed",
                StepState::Blocked => "blocked",
                StepState::Running | StepState::NotRun => "not finished",
            };
            Some(Evidence { step: stage_label(&c, sid), state: state.into(), summary: rec.summary.clone(), result: rec.result.clone() })
        })
        .collect();
    let lock = match Lock::acquire(dir, &format!("{} · {}", c.plugin.manifest.name, task.label)) {
        Ok(l) => l,
        Err(e) => {
            app.error(format!("{e:#}"));
            return false;
        }
    };
    let ctx = PluginAgentContext { plugin: &c.plugin, task: &task, dir, title: &item.meta.title, evidence: &evidence };
    let spec = match agent::build_plugin_command(app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &ctx, super::settings::agent_options(app)) {
        Ok(s) => s,
        Err(e) => {
            app.error(format!("{e:#}"));
            return false;
        }
    };
    let kind = JobKind::PluginAgent { plugin: id.to_string(), dir: dir.to_path_buf(), task: task.id.clone(), before: Snapshot::of_dir(dir) };
    let Some(job) = app.start_job(spec, kind) else { return false };
    app.plugins.locks.insert(job, lock);
    app.plugins.page(id).agent_changes.remove(dir);
    if let Ok(mut item) = WorkItem::open(dir) {
        let rec = item.meta.steps.entry(task.id.clone()).or_default();
        rec.state = StepState::Running;
        rec.started = ystudio_core::intake::timestamp();
        rec.finished.clear();
        let _ = item.save();
        app.plugins.page(id).refresh_items(&root);
    }
    true
}

/// Records a finished agent task: what it changed, what stopped it, and
/// whether it wrote what the task expects.
pub fn agent_finished(app: &mut StudioApp, job: u64, id: &str, dir: &Path, task_id: &str, before: Snapshot) {
    app.plugins.locks.remove(&job);
    let (ok, cancelled, text) = app.job(job).map(|j| (j.proc.succeeded(), j.proc.cancelled, j.proc.text())).unwrap_or((false, false, String::new()));
    let Some(c) = context(app, id) else { return };
    let mut changes = before.diff(&Snapshot::of_dir(dir));
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    let n = changes.iter().filter(|c| !matches!(c, Change::Removed(_))).count();
    let missing: Vec<String> = c
        .plugin
        .manifest
        .task(task_id)
        .map(|t| t.outputs.iter().filter(|o| plugin::safe_relative(o) && !dir.join(o).exists()).cloned().collect())
        .unwrap_or_default();
    let blocker = if agent::needs_approval(&text) {
        Some(crate::workflow::NEEDS_APPROVAL_NOTE.to_string())
    } else {
        agent::blocked_reason(&text).map(|r| format!("Blocked: {r}"))
    };
    let (state, summary) = if cancelled {
        (StepState::Fail, "Cancelled.".to_string())
    } else if let Some(b) = blocker {
        (StepState::Blocked, format!("{b} ({n} file(s) changed)"))
    } else if !ok {
        (StepState::Fail, format!("The agent exited with an error ({n} file(s) changed). See its log."))
    } else if !missing.is_empty() {
        (StepState::Warn, format!("Finished ({n} file(s) changed) but did not write {}.", missing.join(", ")))
    } else {
        (StepState::Ok, format!("Finished: {n} file(s) changed."))
    };
    if let Ok(mut item) = WorkItem::open(dir) {
        let rec = item.meta.steps.entry(task_id.to_string()).or_default();
        rec.state = state;
        rec.finished = ystudio_core::intake::timestamp();
        rec.summary = summary.clone();
        let _ = item.save();
    }
    let page = app.plugins.page(id);
    page.agent_changes.insert(dir.to_path_buf(), changes);
    page.panels = None;
    if let Some(root) = &c.root {
        page.refresh_items(root);
    }
    let label = stage_label(&c, task_id);
    let msg = format!("{} · {label}: {summary}", c.plugin.manifest.name);
    if matches!(state, StepState::Ok | StepState::Warn) {
        app.notify(msg);
    } else {
        app.error(msg);
    }
}

/// Fills Recipe Builder's new-recipe form from a catalog row and its export notes, so
/// the user reviews it before the recipe agent runs.
fn recipe_handoff(app: &mut StudioApp, row: &Value, key: &str, notes_file: &Path) {
    let field = |names: &[&str]| names.iter().find_map(|n| row.get(*n).map(cell)).unwrap_or_default();
    let name = Some(field(&["name", "title"])).filter(|n| !n.is_empty()).unwrap_or_else(|| key.to_string());
    let notes: String = ystudio_core::read_text(notes_file).unwrap_or_default().chars().take(6000).collect();
    let draft = &mut app.batch.new_recipe;
    draft.display = name.clone();
    draft.publisher = field(&["publisher", "manufacturer"]);
    draft.vendor.clear();
    draft.id = ystudio_core::recipe::suggest_id(&name);
    draft.id_touched = false;
    draft.notes = notes.trim().to_string();
    super::batch::show_new_recipe(app);
    app.notify(format!("Review {name} and press 'Write with agent' to start the recipe."));
}
