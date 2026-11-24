//! A plugin's tab. `workitems` pages list work-item folders, each with a
//! pipeline of steps and agent tasks; `catalog` pages search an outside
//! system and act on the chosen row. Both show the plugin's panels, and the
//! prompts its gates need: a confirmation, or an approval bound to the
//! SHA-256 of the file describing the change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkViewer;
use serde_json::{Map, Value};
use ystudio_core::plugin::{
    self, ActionKind, AutomationNext, AutomationState, Gate, Health, HostAction, Installed, Lock, Origin, PageKind, PanelKind, RecipeWorkRecord, RecipeWorkState,
    Risk, Scope, Stage, StepDef, StepRequest, StepResult, StepState, StudioContext, WorkItem,
};
use ystudio_core::agent::{self, Change, Evidence, PluginAgentContext, Snapshot};
use ystudio_core::intake::{Batch, BatchItem, ItemStatus, PluginRecipeOrigin, RequestKind};
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

/// One work item in the cross-plugin activity center.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginActivityState {
    Running,
    Waiting,
    Attention,
    Ready,
    Complete,
}

impl PluginActivityState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Waiting => "Waiting for you",
            Self::Attention => "Needs attention",
            Self::Ready => "Ready to continue",
            Self::Complete => "Complete",
        }
    }

    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Waiting | Self::Attention)
    }
}

#[derive(Debug, Clone)]
pub struct PluginActivity {
    pub plugin_id: String,
    pub plugin_name: String,
    pub dir: PathBuf,
    pub title: String,
    pub reference: String,
    pub created: String,
    pub state: PluginActivityState,
    pub note: String,
    pub done: usize,
    pub total: usize,
}

#[derive(Default)]
pub struct PageState {
    /// Work items under the work root, newest first.
    pub items: Vec<WorkItem>,
    listed_root: Option<PathBuf>,
    pub selected: Option<PathBuf>,
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
    routed: HashMap<String, Result<Vec<Value>, String>>,
    completed: HashMap<String, Result<Vec<Value>, String>>,
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

/// Refreshes the disk-backed cross-plugin activity cache. This also performs
/// the same interrupted-run reconciliation as opening the plugin itself, so
/// Home and History never leave an orphaned run looking live.
pub fn refresh_activity(app: &mut StudioApp, force: bool) {
    if !force
        && app
            .plugins
            .activity_refreshed
            .is_some_and(|refreshed| refreshed.elapsed() < Duration::from_secs(3))
    {
        return;
    }
    let installed = app.plugins.registry.plugins.clone();
    let mut activity = Vec::new();
    for plugin in installed {
        let id = plugin.manifest.id.clone();
        if plugin.manifest.page.kind != PageKind::Workitems
            || !app.settings.plugins.get(&id).is_none_or(|preferences| preferences.enabled)
        {
            continue;
        }
        let values = app.settings.plugins.get(&id).map(|preferences| preferences.values.clone()).unwrap_or_default();
        let Ok(settings) = plugin::resolve_settings(&plugin.manifest, &values, dev_workspace(app).as_deref()) else { continue };
        let Some(root) = plugin::work_root(&settings) else { continue };
        reconcile_work_items(app, &id, &root);
        let stages = plugin.manifest.pipeline();
        for item in WorkItem::list(&root) {
            let total = stages.len();
            let done = stages
                .iter()
                .filter(|stage| matches!(item.step(stage.id()).state, StepState::Ok | StepState::Warn))
                .count();
            let failed = stages.iter().find_map(|stage| {
                let record = item.step(stage.id());
                matches!(record.state, StepState::Interrupted | StepState::Fail | StepState::Blocked)
                    .then_some((stage.label(), record.summary))
            });
            let recipe_failed = item.meta.recipe_work.values().find(|record| record.state == RecipeWorkState::Failed);
            let (state, note) = if let Some((stage, summary)) = failed {
                let detail = if summary.is_empty() { item.meta.automation.note.as_str() } else { summary.as_str() };
                (PluginActivityState::Attention, format!("{stage}: {detail}"))
            } else if let Some(record) = recipe_failed {
                (PluginActivityState::Attention, record.note.clone())
            } else {
                let state = match item.meta.automation.state {
                    AutomationState::Running => PluginActivityState::Running,
                    AutomationState::AwaitingAction => PluginActivityState::Waiting,
                    AutomationState::Paused => PluginActivityState::Attention,
                    AutomationState::Idle => PluginActivityState::Ready,
                    AutomationState::Complete => PluginActivityState::Complete,
                };
                (state, item.meta.automation.note.clone())
            };
            activity.push(PluginActivity {
                plugin_id: id.clone(),
                plugin_name: plugin.manifest.name.clone(),
                dir: item.dir,
                title: item.meta.title,
                reference: item.meta.reference,
                created: item.meta.created,
                state,
                note,
                done,
                total,
            });
        }
    }
    activity.sort_by(|left, right| right.created.cmp(&left.created).then_with(|| left.title.cmp(&right.title)));
    app.plugins.activity = activity;
    app.plugins.activity_refreshed = Some(Instant::now());
}

pub fn open_activity(app: &mut StudioApp, activity: &PluginActivity) {
    let Some(view) = app.plugins.view_of(&activity.plugin_id) else {
        app.error(format!("{} is no longer available.", activity.plugin_name));
        return;
    };
    app.plugins.page(&activity.plugin_id).selected = Some(activity.dir.clone());
    app.view = view;
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

/// Reconciles work-item state after a Studio/process interruption. This runs
/// when a plugin work root is first opened in a session, before the list is
/// shown, so a persisted `Running` record can never strand its retry action.
fn reconcile_work_items(app: &mut StudioApp, id: &str, root: &Path) {
    let active: HashMap<PathBuf, String> = app
        .jobs
        .iter()
        .filter_map(|job| match &job.kind {
            JobKind::PluginStep { plugin, dir, step, .. } if plugin == id && job.proc.is_running() => Some((dir.clone(), step.clone())),
            JobKind::PluginAgent { plugin, dir, task, .. } if plugin == id && job.proc.is_running() => Some((dir.clone(), task.clone())),
            _ => None,
        })
        .collect();
    let mut failures = Vec::new();
    let mut changed_items = Vec::new();
    for mut item in WorkItem::list(root) {
        let current = active.get(&item.dir).map(String::as_str);
        if item.reconcile_running(current, SystemTime::now()) {
            match item.save() {
                Ok(()) => changed_items.push(item.dir.clone()),
                Err(e) => failures.push(format!("{}: {e:#}", item.name())),
            }
        }
    }
    for dir in changed_items {
        sync_recipe_builder_origin(app, id, &dir);
    }
    if !failures.is_empty() {
        app.error(format!("Could not recover interrupted {} work: {}", id, failures.join("; ")));
    }
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
    if purpose == Purpose::Item {
        let mut item = match WorkItem::open(dir) {
            Ok(item) => item,
            Err(e) => {
                app.error(format!("Could not record {} before it starts: {e:#}", step.label()));
                return false;
            }
        };
        let rec = item.meta.steps.entry(step.id.clone()).or_default();
        rec.state = StepState::Running;
        rec.started = ystudio_core::intake::timestamp();
        rec.finished.clear();
        rec.summary.clear();
        rec.result = None;
        rec.approval = prepared.approval.clone();
        if step.automatic && matches!(item.meta.automation.state, AutomationState::AwaitingAction | AutomationState::Paused) {
            item.meta.automation.state = AutomationState::Running;
            item.meta.automation.updated = ystudio_core::intake::timestamp();
            item.meta.automation.note = format!("Running {}.", step.label());
        }
        if let Err(e) = item.save() {
            app.error(format!("Could not record {} before it starts: {e:#}. Nothing was run.", step.label()));
            return false;
        }
    }
    let kind = JobKind::PluginStep { plugin: id.to_string(), dir: dir.to_path_buf(), step: step.id.clone(), result: prepared.result.clone(), risk: step.risk, purpose: purpose.clone() };
    let Some(job) = app.start_job(prepared.spec, kind) else {
        if purpose == Purpose::Item {
            mark_interrupted(app, id, dir, &step.id, "The process could not be started. Review the error, then retry this stage.");
        }
        return false;
    };
    if let Some(lock) = lock {
        app.plugins.locks.insert(job, lock);
    }
    if purpose == Purpose::Item {
        app.plugins.page(id).refresh_items(&root);
    }
    true
}

fn mark_interrupted(app: &mut StudioApp, id: &str, dir: &Path, stage: &str, summary: &str) {
    match WorkItem::open(dir) {
        Ok(mut item) => {
            let rec = item.meta.steps.entry(stage.to_string()).or_default();
            rec.state = StepState::Interrupted;
            rec.finished = ystudio_core::intake::timestamp();
            rec.summary = summary.to_string();
            if let Err(e) = item.save() {
                app.error(format!("Could not save the interrupted state for {stage}: {e:#}"));
            }
        }
        Err(e) => app.error(format!("Could not open the interrupted work item for {stage}: {e:#}")),
    }
    if let Some(c) = context(app, id)
        && let Some(root) = c.root
    {
        app.plugins.page(id).refresh_items(&root);
    }
}

fn set_automation(app: &mut StudioApp, id: &str, dir: &Path, state: AutomationState, note: impl Into<String>, new_run: bool) -> bool {
    let mut item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not update the automatic pipeline: {e:#}"));
            return false;
        }
    };
    let now = ystudio_core::intake::timestamp();
    if new_run {
        item.meta.automation.started = now.clone();
    }
    item.meta.automation.state = state;
    item.meta.automation.updated = now;
    item.meta.automation.note = note.into();
    if let Err(e) = item.save() {
        app.error(format!("Could not save the automatic pipeline: {e:#}"));
        return false;
    }
    if let Some(c) = context(app, id)
        && let Some(root) = c.root
    {
        app.plugins.page(id).refresh_items(&root);
    }
    sync_recipe_builder_origin(app, id, dir);
    true
}

fn plugin_origin_rollup(name: &str, manifest: &plugin::Manifest, item: &WorkItem) -> (ItemStatus, String) {
    let stages = manifest.pipeline();
    let total = stages.len();
    let done = stages
        .iter()
        .filter(|stage| matches!(item.step(stage.id()).state, StepState::Ok | StepState::Warn))
        .count();
    let progress = format!("{done}/{total} stages complete");
    let failed = stages.iter().find_map(|stage| {
        let record = item.step(stage.id());
        matches!(record.state, StepState::Interrupted | StepState::Fail | StepState::Blocked)
            .then_some((stage.label().to_string(), record.summary))
    });
    if item.meta.automation.state == AutomationState::Complete {
        return (ItemStatus::Done, format!("Complete in {name} · {progress}. {}", item.meta.automation.note));
    }
    if let Some((stage, summary)) = failed {
        let detail = if summary.is_empty() { item.meta.automation.note.as_str() } else { summary.as_str() };
        return (ItemStatus::Delegated, format!("Needs attention in {name} · {progress}. {stage}: {detail}"));
    }
    match item.meta.automation.state {
        AutomationState::Running => (ItemStatus::Delegated, format!("Running in {name} · {progress}. {}", item.meta.automation.note)),
        AutomationState::AwaitingAction => (ItemStatus::Delegated, format!("Waiting for you in {name} · {progress}. {}", item.meta.automation.note)),
        AutomationState::Paused => (ItemStatus::Delegated, format!("Needs attention in {name} · {progress}. {}", item.meta.automation.note)),
        AutomationState::Idle => (ItemStatus::Delegated, format!("In {name} · {progress}. Open the linked work item to continue.")),
        AutomationState::Complete => unreachable!(),
    }
}

/// Rolls a plugin work item's lifecycle back into the Recipe Builder request
/// that created it. Missing or deleted source lists are intentionally quiet.
fn sync_recipe_builder_origin(app: &mut StudioApp, id: &str, dir: &Path) {
    let Some(installed) = app.plugins.get(id).cloned() else { return };
    let Ok(item) = WorkItem::open(dir) else { return };
    if item.meta.origin.kind != "recipe-builder" || item.meta.origin.from_plugin != "recipe-builder" {
        return;
    }
    let Some((batch_id, raw_index)) = item.meta.origin.key.rsplit_once(':') else { return };
    let hinted = raw_index.parse::<usize>().ok();
    let (status, note) = plugin_origin_rollup(&installed.manifest.name, &installed.manifest, &item);
    let update = |batch: &mut Batch| {
        let index = hinted
            .filter(|index| batch.items.get(*index).is_some_and(|source| source.handler_item.as_deref() == Some(dir)))
            .or_else(|| batch.items.iter().position(|source| source.handler_item.as_deref() == Some(dir)));
        let Some(index) = index else { return false };
        let source = &mut batch.items[index];
        let changed = source.status != status || source.status_note != note;
        source.status = status;
        source.status_note.clone_from(&note);
        changed
    };
    if app.batch.batch.as_ref().is_some_and(|batch| batch.id == batch_id) {
        if let Some(batch) = app.batch.batch.as_mut()
            && update(batch)
        {
            let _ = batch.save();
        }
    } else if let Ok(mut batch) = Batch::load(&Batch::root().join(batch_id))
        && update(&mut batch)
    {
        let _ = batch.save();
    }
}

/// Starts the next explicitly automatic stage, or records the boundary that
/// needs the user's attention. One call starts at most one external process;
/// successful completion calls this again.
fn advance_automation(app: &mut StudioApp, id: &str, dir: &Path) {
    let Some(c) = context(app, id) else { return };
    let item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not continue the automatic pipeline: {e:#}"));
            return;
        }
    };
    match c.plugin.manifest.next_automatic(&item) {
        AutomationNext::Run(Stage::Task(task)) => {
            let (task_id, label) = (task.id.clone(), task.label.clone());
            if set_automation(app, id, dir, AutomationState::Running, format!("Running {label}."), false)
                && !run_agent(app, id, dir, &task_id)
            {
                set_automation(app, id, dir, AutomationState::Paused, format!("{label} could not start. Resolve the error, then continue safe stages."), false);
            }
        }
        AutomationNext::Run(Stage::Step(step)) => {
            let (step_id, label) = (step.id.clone(), step.label().to_string());
            if set_automation(app, id, dir, AutomationState::Running, format!("Running {label}."), false)
                && !run_step(app, id, dir, &step_id, Map::new(), false, Purpose::Item)
            {
                set_automation(app, id, dir, AutomationState::Paused, format!("{label} could not start. Resolve the error, then continue safe stages."), false);
            }
        }
        AutomationNext::UserAction(stage) => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::AwaitingAction,
                format!("Next: {} needs your review or approval. Run it below, then continue safe stages.", stage.label()),
                false,
            );
        }
        AutomationNext::RoutingReview(panel, count) => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::AwaitingAction,
                if count == 0 {
                    format!("Every product in {} is already available. Confirm, or choose one to package anyway.", panel.title)
                } else {
                    format!("Review the products in {} and choose which belong in Recipe Builder.", panel.title)
                },
                false,
            );
        }
        AutomationNext::RoutedComplete(panel) => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::Complete,
                format!("No products in {} are left to package here: each was routed to Recipe Builder or is already available.", panel.title),
                false,
            );
        }
        AutomationNext::RecipeWorkNeeded(panel, products) => {
            if !ensure_recipe_work(app, id, dir, panel, &products) {
                set_automation(
                    app,
                    id,
                    dir,
                    AutomationState::Paused,
                    "Recipe Builder work could not be created. Resolve the error, then continue safe stages.",
                    false,
                );
            }
        }
        AutomationNext::RecipeWorkWaiting(panel, products) => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::AwaitingAction,
                format!("{} product(s) in {} are waiting for Recipe Builder's safe checks.", products.len(), panel.title),
                false,
            );
        }
        AutomationNext::RecipeWorkStopped(panel, products) => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::AwaitingAction,
                format!("Recipe Builder needs attention for {} in {}. Open the linked work below, then retry it.", products.join(", "), panel.title),
                false,
            );
        }
        AutomationNext::Waiting(stage, unmet) => {
            let labels = unmet.iter().map(|dependency| stage_label(&c, dependency)).collect::<Vec<_>>().join(" and ");
            set_automation(
                app,
                id,
                dir,
                AutomationState::AwaitingAction,
                format!("{} is waiting for {labels}. Complete that manual stage, then continue safe stages.", stage.label()),
                false,
            );
        }
        AutomationNext::Stopped(stage, state) => {
            let reason = match state {
                StepState::Interrupted => "was interrupted",
                StepState::Blocked => "is blocked",
                _ => "failed",
            };
            set_automation(
                app,
                id,
                dir,
                AutomationState::Paused,
                format!("{} {reason}. Review or retry it below, then continue safe stages.", stage.label()),
                false,
            );
        }
        AutomationNext::Running(stage) => {
            set_automation(app, id, dir, AutomationState::Running, format!("Running {}.", stage.label()), false);
        }
        AutomationNext::Complete => {
            set_automation(
                app,
                id,
                dir,
                AutomationState::Complete,
                "All currently available safe stages are complete. After any manual or approved stage, continue to check for more safe work.",
                false,
            );
        }
    }
}

/// Records a finished step: in its work item, or in the catalog page.
pub fn step_finished(app: &mut StudioApp, job: u64, id: &str, dir: &Path, step_id: &str, result: &Path, purpose: Purpose) {
    app.plugins.locks.remove(&job);
    let (exit, cancelled) = app.job(job).map(|j| (j.proc.exit.flatten(), j.proc.cancelled)).unwrap_or((None, false));
    let r = plugin::read_result(result, if cancelled { None } else { exit });
    let state = r.status.state();
    let Some(c) = context(app, id) else { return };
    let label = c.plugin.manifest.step(step_id).map(|s| s.label().to_string()).unwrap_or_else(|| step_id.to_string());
    match &purpose {
        Purpose::Item => {
            let mut item = match WorkItem::open(dir) {
                Ok(item) => item,
                Err(e) => {
                    app.error(format!("{label} finished, but its work item could not be opened: {e:#}"));
                    return;
                }
            };
            let rec = item.meta.steps.entry(step_id.to_string()).or_default();
            rec.state = state;
            rec.finished = ystudio_core::intake::timestamp();
            rec.summary = r.summary.clone();
            rec.result = Some(result.to_path_buf());
            if let Err(e) = item.save() {
                app.error(format!("{label} finished, but its result could not be saved: {e:#}"));
                return;
            }
            if let Some(root) = &c.root {
                app.plugins.page(id).refresh_items(root);
            }
        }
        Purpose::Probe => app.plugins.page(id).probe = Some(r.clone()),
        Purpose::Query => {
            let page = app.plugins.page(id);
            page.rows = r.rows.clone();
            page.selected_row = None;
        }
        Purpose::Import => {
            if !r.rows.is_empty() {
                app.plugins.page(id).import = Some(r.rows.iter().map(|v| (true, v.clone())).collect());
            }
        }
        Purpose::Row(key) => {
            if let (Some(folder), Some(root)) = (r.outputs.get("folder"), &c.root)
                && plugin::safe_relative(folder)
            {
                app.plugins.page(id).row_folders.insert(key.clone(), root.join(folder));
            }
            app.plugins.page(id).panels = None;
        }
    }
    let summary = if r.summary.is_empty() { String::new() } else { format!(": {}", r.summary) };
    let msg = format!("{} · {label}{summary}", c.plugin.manifest.name);
    match state {
        StepState::Ok | StepState::Warn => app.notify(msg),
        _ => app.error(msg),
    }
    if purpose == Purpose::Item
        && let Ok(item) = WorkItem::open(dir)
        && item.meta.automation.state == AutomationState::Running
    {
        if matches!(state, StepState::Ok | StepState::Warn) {
            advance_automation(app, id, dir);
        } else {
            set_automation(
                app,
                id,
                dir,
                AutomationState::Paused,
                format!("{label} did not finish successfully. Review or retry it, then continue safe stages."),
                false,
            );
        }
    }
    if purpose == Purpose::Item {
        sync_recipe_builder_origin(app, id, dir);
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
    let host_actions = m.page.effective_host_actions();
    ui.horizontal(|ui| {
        // What the plugin is for is on the title; the page itself is for doing it.
        let title = theme::page_title(ui, &m.name);
        if !m.description.is_empty() {
            title.on_hover_text(&m.description);
        }
        ui.label(RichText::new(format!("{} · {}", m.version, m.publisher)).small().weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if host_actions.contains(&HostAction::Settings)
                && ui.button("\u{E713}").on_hover_text("Plugin settings").clicked()
            {
                open_plugin_settings(app, &id);
            }
            if host_actions.contains(&HostAction::WorkFolder)
                && let Some(root) = &c.root
                && ui.button("\u{E838}").on_hover_text("Open the work folder").clicked()
            {
                let _ = std::fs::create_dir_all(root);
                super::open_in_explorer(root);
            }
        });
    });
    ui.add_space(6.0);
    let health = match &c.plugin.health {
        Health::Ok => None,
        Health::Modified(files) => Some(format!("Its files changed since install ({}). Reinstall it from Settings to run its steps.", files.iter().take(3).cloned().collect::<Vec<_>>().join(", "))),
        Health::Unrecorded => Some("It wasn't installed by the Studio. Install it from its package in Settings to run its steps.".into()),
        Health::Incompatible(why) => Some(format!("It {why}.")),
    };
    if let Some(h) = health {
        theme::banner(ui, theme::red(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(h);
                if ui.button("Manage plugin").clicked() {
                    open_plugin_settings(app, &id);
                }
            });
        });
        ui.add_space(6.0);
    }
    let missing = app.plugins.registry.missing_requirements(&c.plugin);
    if !missing.is_empty() {
        theme::banner(ui, theme::amber(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("This plugin {}. Some actions are unavailable.", missing.join(", ")));
                if ui.button("Manage plugins").clicked() {
                    open_plugin_settings(app, &id);
                }
            });
        });
        ui.add_space(6.0);
    }
    if let Err(problems) = &c.settings {
        theme::banner(ui, theme::amber(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Finish setting up this plugin: {}", problems.join(" ")));
                if theme::primary(ui, true, "Open plugin settings").clicked() {
                    open_plugin_settings(app, &id);
                }
            });
        });
        return;
    }
    let Some(root) = c.root.clone() else { return };
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

fn open_plugin_settings(app: &mut StudioApp, id: &str) {
    app.plugins_ui.open_settings = Some(id.to_string());
    app.view = View::Settings;
}

fn state_badge(ui: &mut Ui, state: StepState) -> egui::Response {
    let (text, color) = match state {
        StepState::NotRun => ("Not run", theme::grey()),
        StepState::Running => ("Running", theme::blue()),
        StepState::Interrupted => ("Interrupted", theme::amber()),
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
    let host_actions = m.page.effective_host_actions();
    if app.plugins.page(&id).listed_root.as_deref() != Some(root) {
        reconcile_work_items(app, &id, root);
        app.plugins.page(&id).refresh_items(root);
    }
    let mut import = false;
    egui::Panel::left(("plugin-items", &id)).resizable(true).default_size(300.0).frame(theme::pane_frame()).show(ui, |ui| {
        let page = app.plugins.page(&id);
        ui.horizontal(|ui| {
            if host_actions.contains(&HostAction::NewWork)
                && theme::primary(ui, true, format!("\u{E710}  New {}", m.page.noun.to_lowercase())).clicked()
            {
                page.new_item = Some(NewItem::default());
            }
            if host_actions.contains(&HostAction::Import)
                && ui.button("\u{E8B5}  Import…").on_hover_text("Read a list into new work items").clicked()
            {
                import = true;
            }
            if host_actions.contains(&HostAction::Refresh)
                && ui.button("\u{E72C}").on_hover_text("Refresh").clicked()
            {
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
                    } else if states.contains(&StepState::Interrupted) {
                        theme::badge(ui, "Interrupted", theme::amber());
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
    let from_recipe_builder = item.meta.origin.kind == "recipe-builder" && item.meta.origin.from_plugin == "recipe-builder";
    let source_plugin = (!from_recipe_builder && !item.meta.origin.from_plugin.is_empty())
        .then(|| {
            let source_id = item.meta.origin.from_plugin.clone();
            let name = app
                .plugins
                .get(&source_id)
                .map(|plugin| plugin.manifest.name.clone())
                .unwrap_or_else(|| source_id.clone());
            (source_id.clone(), name, app.plugins.view_of(&source_id))
        });
    let mut back_to_source = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&item.meta.title).heading());
        if ui.small_button("\u{E838}  Open folder").clicked() {
            super::open_in_explorer(&item.dir);
        }
        if from_recipe_builder
            && ui
                .small_button("\u{E72B}  Back to Recipe Builder")
                .on_hover_text("Return to the exact request that created this plugin work item.")
                .clicked()
        {
            back_to_source = true;
        } else if let Some((_, name, Some(_))) = &source_plugin
            && ui
                .small_button(format!("\u{E72B}  Back to {name}"))
                .on_hover_text("Return to the workflow that created this work item.")
                .clicked()
        {
            back_to_source = true;
        }
    });
    if back_to_source {
        if from_recipe_builder {
            crate::views::batch::open_plugin_handoff_source(app, &item.meta.origin.key, &item.dir);
        } else if let Some((source_id, _, Some(view))) = source_plugin {
            app.view = view;
            app.notify(format!("Returned to {}.", app.plugins.get(&source_id).map(|plugin| plugin.manifest.name.as_str()).unwrap_or(&source_id)));
        }
    }
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
                if ui.button("Recover").on_hover_text("Clear the stale lock and make any orphaned running stage retryable").clicked() {
                    match Lock::force_release(&item.dir) {
                        Ok(()) => {
                            match WorkItem::open(&item.dir) {
                                Ok(mut recovered) => {
                                    let changed = recovered.reconcile_running(None, SystemTime::now());
                                    if changed
                                        && let Err(e) = recovered.save()
                                    {
                                        app.error(format!("The lock was cleared, but interrupted state could not be saved: {e:#}"));
                                    } else {
                                        if changed {
                                            sync_recipe_builder_origin(app, &id, &item.dir);
                                        }
                                        if let Some(root) = &c.root {
                                            app.plugins.page(&id).refresh_items(root);
                                        }
                                        app.notify("Cleared the stale lock. Review any outputs, then retry the interrupted stage.");
                                    }
                                }
                                Err(e) => app.error(format!("The lock was cleared, but the work item could not be opened: {e:#}")),
                            }
                        }
                        Err(e) => app.error(format!("Could not clear the stale lock: {e:#}")),
                    }
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
    if item.meta.steps.values().any(|record| record.state == StepState::Interrupted) {
        theme::banner(ui, theme::amber(), |ui| {
            ui.label("This work was interrupted. Review any outputs from the interrupted stage, then retry it; completed stages remain valid.");
        });
        ui.add_space(8.0);
    }
    routing_panels(app, ui, c, item, running.is_some());
    recipe_work_panels(app, ui, c, item);
    theme::section_heading(ui, "Pipeline");
    if m.agent.as_ref().is_some_and(|a| !a.tasks.is_empty()) {
        let agent_available = if has_private_workspace(app, &id) { !isolated_agent_conflict(app, &id) } else { !crate::workflow::busy(app) };
        ui.add_enabled_ui(agent_available, |ui| super::settings::run_options(app, ui, &format!("plugin-{id}"), false));
        ui.add_space(4.0);
    }
    let busy_elsewhere = if has_private_workspace(app, &id) { isolated_agent_conflict(app, &id) } else { crate::workflow::busy(app) };
    if m.has_automatic_stages() {
        automation_controls(app, ui, c, item, running.is_some(), busy_elsewhere);
        ui.add_space(8.0);
    }
    // A row per stage rather than a card: what a stage does is on its name;
    // what it did (the summary and outputs) stays visible under it.
    for stage in m.pipeline() {
        let rec = item.step(stage.id());
        let state = if running.as_ref().is_some_and(|(_, s)| s == stage.id()) { StepState::Running } else { rec.state };
        ui.separator();
        ui.scope(|ui| {
            // Wide enough, the button sits at the right of the heading; otherwise under it.
            if ui.available_width() > 520.0 {
                ui.horizontal(|ui| {
                    ui.set_min_height(30.0);
                    stage_heading(ui, stage, state);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| stage_button(app, ui, c, item, stage, state, running.is_some(), busy_elsewhere));
                });
            } else {
                ui.horizontal_wrapped(|ui| stage_heading(ui, stage, state));
                ui.horizontal(|ui| stage_button(app, ui, c, item, stage, state, running.is_some(), busy_elsewhere));
            }
            if !rec.summary.is_empty() {
                let color = match rec.state {
                    StepState::Fail => theme::red(),
                    StepState::Warn | StepState::Blocked | StepState::Interrupted => theme::amber(),
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

#[derive(Debug, Clone)]
enum RoutingAction {
    Route(usize),
    Return(usize),
    Restore(usize),
    Confirm,
    OpenRouted(String, String),
}

fn row_field(row: &Value, names: &[&str]) -> String {
    names.iter().find_map(|name| row.get(*name).map(cell)).unwrap_or_default()
}

fn row_list(row: &Value, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .find_map(|name| row.get(*name))
        .map(|value| match value {
            Value::Array(values) => values.iter().map(cell).filter(|value| !value.is_empty()).collect(),
            value => {
                let value = cell(value);
                if value.is_empty() { Vec::new() } else { vec![value] }
            }
        })
        .unwrap_or_default()
}

fn routed_product_id(row: &Value) -> String {
    plugin::panel_recipe_id(row)
}

fn product_batch_item(c: &Ctx, item: &WorkItem, row: &Value) -> BatchItem {
    let name = Some(row_field(row, &["name", "title", "app"])).filter(|value| !value.is_empty()).unwrap_or_else(|| "Untitled product".into());
    let suggested_id = routed_product_id(row);
    let reference = if item.meta.reference.is_empty() { item.name() } else { item.meta.reference.clone() };
    let request = ystudio_core::read_text(&item.dir.join("ticket.md")).unwrap_or_default();
    let details = serde_json::to_string_pretty(row).unwrap_or_else(|_| row.to_string());
    let notes = format!(
        "Routed from {} work item '{}'.\n\nProduct selected from the ticket:\n```json\n{}\n```\n\nOriginal request:\n{}",
        c.plugin.manifest.name,
        item.name(),
        details,
        request.trim()
    );
    BatchItem {
        source: format!("{reference} · {name}"),
        app: name,
        publisher: row_field(row, &["publisher", "manufacturer"]),
        version: row_field(row, &["version"]),
        urls: row_list(row, &["links", "urls"]),
        suggested_id,
        notes,
        request: RequestKind::New,
        include: true,
        confidence: 1.0,
        handler_item: Some(item.dir.clone()),
        ..Default::default()
    }
}

fn ensure_recipe_work(app: &mut StudioApp, id: &str, dir: &Path, panel: &plugin::PanelDef, products: &[String]) -> bool {
    let Some(c) = context(app, id) else { return false };
    let Some(routing) = panel.recipe_builder.as_ref() else { return false };
    let rows = match plugin::active_panel_rows(dir, panel) {
        Ok(rows) => rows,
        Err(e) => {
            app.error(format!("Could not read products for Recipe Builder: {e:#}"));
            return false;
        }
    };
    let item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not open the plugin work item: {e:#}"));
            return false;
        }
    };
    let mut requests = Vec::new();
    for row in rows.iter().filter(|row| products.iter().any(|id| id == &routed_product_id(row))) {
        let product_id = routed_product_id(row);
        let mut request = product_batch_item(&c, &item, row);
        request.handler_item = None;
        request.notes = request.notes.replacen("Routed from", "Linked from", 1);
        request.plugin_origins = vec![PluginRecipeOrigin {
            plugin: id.to_string(),
            work_item: dir.to_path_buf(),
            product_id,
            recipe_folder: routing.recipe_folder.clone(),
            icon_folder: routing.icon_folder.clone(),
        }];
        requests.push(request);
    }
    if requests.is_empty() {
        app.error("No matching plugin products were available for Recipe Builder.");
        return false;
    }
    let batch_id = crate::views::batch::add_plugin_recipe_work(app, requests);
    if batch_id.is_empty() {
        app.error("Recipe Builder could not create a persistent working list.");
        return false;
    }
    let mut item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not record the Recipe Builder links: {e:#}"));
            return false;
        }
    };
    for product_id in products {
        item.meta.recipe_work.entry(product_id.clone()).or_insert_with(|| RecipeWorkRecord {
            state: RecipeWorkState::Pending,
            batch_id: batch_id.clone(),
            recipe_id: String::new(),
            note: "Ready to complete in Recipe Builder.".into(),
            updated: ystudio_core::intake::timestamp(),
        });
    }
    if let Err(e) = item.save() {
        app.error(format!("Could not save the Recipe Builder links: {e:#}"));
        return false;
    }
    set_automation(
        app,
        id,
        dir,
        AutomationState::AwaitingAction,
        format!("Complete {} linked recipe(s) in Recipe Builder. One-Off resumes after their safe checks pass.", products.len()),
        false,
    )
}

pub fn recipe_work_changed(app: &mut StudioApp, id: &str, dir: &Path) {
    if let Some(c) = context(app, id)
        && let Some(root) = c.root
    {
        app.plugins.page(id).refresh_items(&root);
    }
    if WorkItem::open(dir).is_ok_and(|item| item.meta.automation.state == AutomationState::AwaitingAction) {
        advance_automation(app, id, dir);
    }
}

fn set_routing_review(app: &mut StudioApp, id: &str, dir: &Path, panel: &str, reviewed: bool) -> bool {
    let mut item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not update the routing decision: {e:#}"));
            return false;
        }
    };
    if reviewed {
        item.meta.routing_reviews.insert(panel.to_string());
    } else {
        item.meta.routing_reviews.remove(panel);
    }
    if let Err(e) = item.save() {
        app.error(format!("Could not save the routing decision: {e:#}"));
        return false;
    }
    if let Some(c) = context(app, id)
        && let Some(root) = c.root
    {
        app.plugins.page(id).refresh_items(&root);
    }
    true
}

fn continue_after_routing(app: &mut StudioApp, id: &str, dir: &Path) {
    // Complete too: moving a product back after "nothing left to package" resumes the work.
    if WorkItem::open(dir).is_ok_and(|item| matches!(item.meta.automation.state, AutomationState::AwaitingAction | AutomationState::Complete))
        && set_automation(app, id, dir, AutomationState::Running, "Routing reviewed. Looking for the next safe stage.", false)
    {
        advance_automation(app, id, dir);
    }
}

/// Coloured markers a plugin puts on a row: `flags: [{ label, tone, detail }]`,
/// where tone is green, amber, red, blue or grey.
fn row_flags(row: &Value) -> Vec<(String, egui::Color32, String)> {
    row.get("flags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|flag| {
            let text = |name: &str| flag.get(name).and_then(Value::as_str).unwrap_or_default().trim().to_string();
            let label = text("label");
            let tone = match text("tone").as_str() {
                "green" => theme::green(),
                "amber" => theme::amber(),
                "red" => theme::red(),
                "blue" => theme::blue(),
                _ => theme::grey(),
            };
            (!label.is_empty()).then(|| (label, tone, text("detail")))
        })
        .collect()
}

fn routing_row(ui: &mut Ui, row: &Value, button: &str, enabled: bool, secondary: Option<(&str, bool)>) -> (bool, bool) {
    let name = Some(row_field(row, &["name", "title", "app"])).filter(|value| !value.is_empty()).unwrap_or_else(|| "Untitled product".into());
    let version = row_field(row, &["version"]);
    let status = row_field(row, &["status"]);
    let notes = row_field(row, &["notes"]);
    let flags = row_flags(row);
    let mut clicked = false;
    let mut secondary_clicked = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(name).semibold());
        if !version.is_empty() {
            theme::badge(ui, version, theme::grey());
        }
        if !status.is_empty() {
            theme::badge(ui, status.replace('-', " "), theme::grey());
        }
        for (label, color, detail) in &flags {
            let badge = theme::badge(ui, label, *color);
            if !detail.is_empty() {
                badge.on_hover_text(detail);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            clicked = ui.add_enabled(enabled, egui::Button::new(button)).clicked();
            if let Some((label, secondary_enabled)) = secondary {
                secondary_clicked = ui.add_enabled(secondary_enabled, egui::Button::new(label).frame(false)).clicked();
            }
        });
    });
    for (_, _, detail) in flags.iter().filter(|(_, _, detail)| !detail.is_empty()) {
        ui.label(RichText::new(detail).small());
    }
    if !notes.is_empty() {
        ui.label(RichText::new(notes).small().weak());
    }
    (clicked, secondary_clicked)
}

fn routing_panels(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem, item_running: bool) {
    let id = c.plugin.manifest.id.clone();
    for panel in c.plugin.manifest.page.panels.iter().filter(|panel| panel.recipe_builder.is_some()) {
        let (active, routed, completed) = {
            let Some(cache) = app.plugins.page(&id).panels.as_ref() else { continue };
            (
                cache.tables.get(&panel.title).cloned(),
                cache.routed.get(&panel.title).cloned(),
                cache.completed.get(&panel.title).cloned(),
            )
        };
        let Some(Ok(active)) = active else { continue };
        let routed = routed.and_then(Result::ok).unwrap_or_default();
        let completed = completed.and_then(Result::ok).unwrap_or_default();
        if active.is_empty() && routed.is_empty() && completed.is_empty() {
            continue;
        }
        let routing = panel.recipe_builder.as_ref().unwrap();
        let stage_started = item.step(&routing.review_before).state != StepState::NotRun;
        let busy = item_running || crate::workflow::busy(app) || Lock::holder(&item.dir).is_some();
        let editable = !stage_started && !busy;
        let needs_review = !item.meta.routing_reviews.contains(&panel.title)
            && (!completed.is_empty() || (!active.is_empty() && (active.len() > 1 || !routed.is_empty())));
        let mut action = None;

        theme::section_heading_about(
            ui,
            "Product routing",
            &format!(
                "Keep products that belong in {} here. Send reusable applications to Recipe Builder before this workflow continues.",
                c.plugin.manifest.name
            ),
        );
        ui.add_space(2.0);
        if needs_review {
            theme::banner(ui, theme::amber(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let total = active.len() + routed.len() + completed.len();
                    ui.label(if active.is_empty() {
                        format!(
                            "{} already available. Confirm, or package one anyway.",
                            if total == 1 { "This product is".to_string() } else { format!("All {total} products are") }
                        )
                    } else if !completed.is_empty() {
                        format!(
                            "{} of {total} product(s) already available. Route each reusable app, then confirm what remains in {}.",
                            completed.len(),
                            c.plugin.manifest.name
                        )
                    } else {
                        format!("This ticket contains {total} products. Route each reusable app, then confirm what remains in {}.", c.plugin.manifest.name)
                    });
                    let keep = match active.len() {
                        0 => "Confirm".to_string(),
                        1 => "Keep it here".to_string(),
                        2 => "Keep both here".to_string(),
                        n => format!("Keep {n} here"),
                    };
                    if theme::primary(ui, editable, keep)
                        .on_hover_text("Continue these products through the One-Off Packager pipeline.")
                        .on_disabled_hover_text("Wait for current work to finish before changing routing.")
                        .clicked()
                    {
                        action = Some(RoutingAction::Confirm);
                    }
                });
            });
            ui.add_space(6.0);
        } else if stage_started {
            ui.label(RichText::new(format!("Routing is locked because downstream {} work has started.", c.plugin.manifest.name)).small().weak());
        } else if active.is_empty() {
            theme::banner(ui, theme::green(), |ui| {
                ui.label(if completed.is_empty() {
                    "Every product in this ticket is now handled by Recipe Builder."
                } else {
                    "Nothing in this ticket needs packaging: each product is already available or handled by Recipe Builder."
                });
            });
        }

        for (index, row) in active.iter().enumerate() {
            if routing_row(ui, row, "Send to Recipe Builder", editable, None).0 {
                action = Some(RoutingAction::Route(index));
            }
            ui.separator();
        }
        if !completed.is_empty() {
            egui::CollapsingHeader::new(format!("Already available ({})", completed.len()))
                .id_salt(("plugin-completed-products", &id, &panel.title))
                .default_open(true)
                .show(ui, |ui| {
                    for (index, row) in completed.iter().enumerate() {
                        // Triage's "needs research" no longer describes a product that exists.
                        let mut row = row.clone();
                        if let Some(fields) = row.as_object_mut() {
                            fields.remove("status");
                        }
                        if routing_row(ui, &row, "Package anyway", editable, None).0 {
                            action = Some(RoutingAction::Restore(index));
                        }
                        ui.separator();
                    }
                });
        }
        if !routed.is_empty() {
            egui::CollapsingHeader::new(format!("In Recipe Builder ({})", routed.len()))
                .id_salt(("plugin-routed-products", &id, &panel.title))
                .default_open(true)
                .show(ui, |ui| {
                    for (index, row) in routed.iter().enumerate() {
                        let product_id = routed_product_id(row);
                        let batch_id = item.meta.routed_recipe_work.get(&product_id).cloned().unwrap_or_default();
                        let return_label = format!("Return to {}", c.plugin.manifest.name);
                        let (open, return_to_plugin) = routing_row(
                            ui,
                            row,
                            "Open in Recipe Builder",
                            true,
                            Some((&return_label, editable)),
                        );
                        if open {
                            action = Some(RoutingAction::OpenRouted(product_id, batch_id));
                        } else if return_to_plugin {
                            action = Some(RoutingAction::Return(index));
                        }
                        ui.separator();
                    }
                });
        }
        ui.add_space(10.0);

        match action {
            Some(RoutingAction::Route(index)) if set_routing_review(app, &id, &item.dir, &panel.title, false) => {
                match plugin::move_panel_row(&item.dir, panel, index, true) {
                    Ok(row) => {
                        let product_id = routed_product_id(&row);
                        let batch_id = crate::views::batch::add_plugin_product(app, product_batch_item(c, item, &row));
                        if !batch_id.is_empty() {
                            match WorkItem::open(&item.dir) {
                                Ok(mut updated) => {
                                    updated.meta.routed_recipe_work.insert(product_id, batch_id);
                                    if let Err(e) = updated.save() {
                                        app.error(format!("The product was routed, but its Recipe Builder return link could not be saved: {e:#}"));
                                    }
                                }
                                Err(e) => app.error(format!("The product was routed, but its plugin work item could not be updated: {e:#}")),
                            }
                        }
                        app.plugins.page(&id).panels = None;
                        continue_after_routing(app, &id, &item.dir);
                    }
                    Err(e) => app.error(format!("Could not route the product: {e:#}")),
                }
            }
            Some(RoutingAction::Return(index)) => {
                let row = routed.get(index).cloned().unwrap_or(Value::Null);
                let product_id = routed_product_id(&row);
                match crate::views::batch::remove_plugin_product(app, &item.dir, &product_id) {
                    Ok(()) if set_routing_review(app, &id, &item.dir, &panel.title, false) => {
                        match plugin::move_panel_row(&item.dir, panel, index, false) {
                            Ok(_) => {
                                if let Ok(mut updated) = WorkItem::open(&item.dir) {
                                    updated.meta.routed_recipe_work.remove(&product_id);
                                    let _ = updated.save();
                                }
                                app.plugins.page(&id).panels = None;
                                app.notify(format!("Returned the product to {}.", c.plugin.manifest.name));
                                continue_after_routing(app, &id, &item.dir);
                            }
                            Err(e) => app.error(format!("Could not return the product: {e:#}")),
                        }
                    }
                    Ok(()) => {}
                    Err(e) => app.error(e),
                }
            }
            Some(RoutingAction::Restore(index)) if set_routing_review(app, &id, &item.dir, &panel.title, false) => {
                match plugin::move_panel_row_between(&item.dir, panel, index, plugin::RowList::Completed, plugin::RowList::Active) {
                    Ok(_) => {
                        app.plugins.page(&id).panels = None;
                        app.notify(format!("Moved the product back to {}. It will be packaged.", c.plugin.manifest.name));
                        continue_after_routing(app, &id, &item.dir);
                    }
                    Err(e) => app.error(format!("Could not move the product back: {e:#}")),
                }
            }
            Some(RoutingAction::Confirm) if set_routing_review(app, &id, &item.dir, &panel.title, true) => {
                continue_after_routing(app, &id, &item.dir);
            }
            Some(RoutingAction::OpenRouted(product_id, batch_id)) => {
                crate::views::batch::open_routed_plugin_product(app, &batch_id, &item.dir, &product_id);
            }
            _ => {}
        }
    }
}

fn recipe_work_panels(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem) {
    let mut open = None;
    for panel in c.plugin.manifest.page.panels.iter().filter(|panel| {
        panel.recipe_builder.as_ref().is_some_and(|routing| !routing.author_before.is_empty())
    }) {
        let rows = match plugin::active_panel_rows(&item.dir, panel) {
            Ok(rows) if !rows.is_empty() => rows,
            _ => continue,
        };
        theme::section_heading_about(
            ui,
            "Recipe Builder work",
            &format!(
                "Recipe Builder owns research, recipe authoring and safe checks. {} receives the checked recipe and then continues its specialist workflow.",
                c.plugin.manifest.name
            ),
        );
        ui.add_space(2.0);
        for row in rows {
            let product_id = routed_product_id(&row);
            let name = Some(row_field(&row, &["name", "title", "app"])).filter(|value| !value.is_empty()).unwrap_or_else(|| product_id.clone());
            let record = item.meta.recipe_work.get(&product_id);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(name).semibold());
                match record.map(|record| record.state) {
                    Some(RecipeWorkState::Ready) => {
                        theme::badge(ui, "Recipe ready", theme::green());
                    }
                    Some(RecipeWorkState::Running) => {
                        theme::badge(ui, "Working", theme::blue());
                    }
                    Some(RecipeWorkState::Failed) => {
                        theme::badge(ui, "Needs attention", theme::red());
                    }
                    Some(RecipeWorkState::Pending) => {
                        theme::badge(ui, "Ready in Recipe Builder", theme::amber());
                    }
                    None => {
                        theme::badge(ui, "Not started", theme::grey());
                    }
                }
                if let Some(record) = record.filter(|record| !record.batch_id.is_empty())
                    && ui.button("Open in Recipe Builder").clicked()
                {
                    open = Some((record.batch_id.clone(), product_id.clone()));
                }
            });
            let note = record.map(|record| record.note.as_str()).filter(|note| !note.is_empty()).unwrap_or("The safe pipeline will create this linked recipe work after the plugin's read-only inventory.");
            ui.label(RichText::new(note).small().weak());
            ui.separator();
        }
        ui.add_space(8.0);
    }
    if let Some((batch_id, product_id)) = open {
        crate::views::batch::open_plugin_recipe_work(app, &batch_id, &item.dir, Some(&product_id));
    }
}

fn automation_controls(app: &mut StudioApp, ui: &mut Ui, c: &Ctx, item: &WorkItem, item_running: bool, busy_elsewhere: bool) {
    let id = c.plugin.manifest.id.clone();
    let record = item.meta.automation.clone();
    let color = match record.state {
        AutomationState::Running => theme::blue(),
        AutomationState::Complete => theme::green(),
        AutomationState::AwaitingAction | AutomationState::Paused => theme::amber(),
        AutomationState::Idle => theme::grey(),
    };
    let default_note = "Runs stages the plugin explicitly marks safe, one at a time. Stops before approvals, installs, tenant changes and manual work.";
    let mut start = false;
    let mut pause = false;
    // Idle, it is just the action; a coloured bar only once there is a state to report.
    let quiet = record.state == AutomationState::Idle && record.note.is_empty();
    let body = |ui: &mut Ui, start: &mut bool, pause: &mut bool| {
        ui.horizontal_wrapped(|ui| {
            if record.state == AutomationState::Running && item_running {
                ui.spinner();
                ui.label(if record.note.is_empty() { "Running safe stages…" } else { &record.note });
                *pause = ui.button("Pause after this stage").clicked();
            } else {
                let label = if record.state == AutomationState::Idle { "Run safe stages" } else { "Continue safe stages" };
                let can_start = !item_running && !busy_elsewhere;
                *start = theme::primary(ui, can_start, format!("\u{E768}  {label}"))
                    .on_hover_text(default_note)
                    .on_disabled_hover_text(if item_running { "Wait for the current stage to finish." } else { "Another agent or test is running." })
                    .clicked();
                if !record.note.is_empty() {
                    ui.label(&record.note);
                }
            }
        });
    };
    if quiet {
        body(ui, &mut start, &mut pause);
    } else {
        theme::banner(ui, color, |ui| body(ui, &mut start, &mut pause));
    }
    if pause {
        set_automation(
            app,
            &id,
            &item.dir,
            AutomationState::Paused,
            "Pausing after the current stage. Completed stages remain valid.",
            false,
        );
    } else if start
        && set_automation(app, &id, &item.dir, AutomationState::Running, "Looking for the next safe stage.", record.state == AutomationState::Idle)
    {
        advance_automation(app, &id, &item.dir);
    }
}

/// A pipeline row's state, name, risk and gate.
fn stage_heading(ui: &mut Ui, stage: Stage<'_>, state: StepState) {
    state_badge(ui, state);
    let name = ui.label(RichText::new(stage.label()).semibold());
    if !stage.description().is_empty() {
        name.on_hover_text(stage.description());
    }
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
            let text = match state {
                StepState::NotRun => "\u{E99A}  Run agent",
                StepState::Interrupted => "\u{E72C}  Retry interrupted agent task",
                _ => "\u{E72C}  Run agent again",
            };
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
        (_, _, StepState::Interrupted) => "\u{E72C}  Retry interrupted stage",
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
        ActionKind::HostPluginWorkItem { plugin: target, files, title } => {
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
        ActionKind::HostRecipeBuilder { notes_file } => {
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
        routed: HashMap::new(),
        completed: HashMap::new(),
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
                if p.recipe_builder.is_some() {
                    cache.routed.insert(p.title.clone(), plugin::routed_panel_rows(dir, p).map_err(|e| format!("{e:#}")));
                    cache.completed.insert(p.title.clone(), plugin::completed_panel_rows(dir, p).map_err(|e| format!("{e:#}")));
                }
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
        if p.recipe_builder.is_some() {
            continue;
        }
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
        let manifest = edit(manifest.replace("minVersion: 0.7.0", &format!("minVersion: {}", super::super::update::CURRENT_VERSION)).replace("id: sample-plugin", &format!("id: {id}")));
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

    #[test]
    fn host_settings_action_uses_the_global_plugin_settings_surface() {
        let (dir, mut app) = fake_app();
        install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        app.settings.plugins.get_mut("sample-plugin").unwrap().values.remove("workRoot");
        app.view = View::Plugin(0);

        render(&mut app, "plugin-needs-global-settings", 1100, 680);
        assert_eq!(app.view, View::Plugin(0), "rendering the recovery state does not silently navigate");

        open_plugin_settings(&mut app, "sample-plugin");
        assert_eq!(app.view, View::Settings);
        assert_eq!(app.plugins_ui.open_settings.as_deref(), Some("sample-plugin"));
        render(&mut app, "plugin-global-settings", 1100, 680);
    }

    #[test]
    fn plugin_activity_center_reconciles_filters_and_opens_exact_work() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);

        let mut interrupted = WorkItem::create(
            &root,
            "{date}-{slug}",
            "Interrupted package",
            "TASK-101",
            Origin::default(),
            SystemTime::now(),
        )
        .unwrap();
        interrupted.meta.steps.insert(
            "echo".into(),
            plugin::StepRecord { state: StepState::Running, started: "earlier".into(), ..Default::default() },
        );
        interrupted.meta.automation = plugin::AutomationRecord {
            state: AutomationState::Running,
            note: "Running Echo.".into(),
            ..Default::default()
        };
        interrupted.save().unwrap();

        let mut waiting = WorkItem::create(
            &root,
            "{date}-{slug}",
            "Approval needed",
            "TASK-102",
            Origin::default(),
            SystemTime::now(),
        )
        .unwrap();
        waiting.meta.automation = plugin::AutomationRecord {
            state: AutomationState::AwaitingAction,
            note: "Review the requested change before continuing.".into(),
            ..Default::default()
        };
        waiting.save().unwrap();

        let mut complete = WorkItem::create(
            &root,
            "{date}-{slug}",
            "Finished package",
            "TASK-103",
            Origin::default(),
            SystemTime::now(),
        )
        .unwrap();
        complete.meta.automation.state = AutomationState::Complete;
        complete.meta.automation.note = "All safe stages completed.".into();
        complete.save().unwrap();

        refresh_activity(&mut app, true);
        assert_eq!(app.plugins.activity.len(), 3);
        let recovered = app.plugins.activity.iter().find(|activity| activity.title == "Interrupted package").unwrap();
        assert_eq!(recovered.state, PluginActivityState::Attention);
        assert_eq!(WorkItem::open(&interrupted.dir).unwrap().step("echo").state, StepState::Interrupted);
        assert_eq!(
            app.plugins.activity.iter().find(|activity| activity.title == "Approval needed").unwrap().state,
            PluginActivityState::Waiting
        );
        assert_eq!(
            app.plugins.activity.iter().find(|activity| activity.title == "Finished package").unwrap().state,
            PluginActivityState::Complete
        );

        app.view = View::Home;
        render(&mut app, "home-plugin-attention", 1200, 820);
        app.runs_ui.show_plugin_work();
        app.view = View::Runs;
        render(&mut app, "history-plugin-work", 1200, 820);
        render(
            &mut app,
            "history-plugin-work-min",
            crate::app::MIN_LAYOUT.x as u32,
            crate::app::MIN_LAYOUT.y as u32,
        );

        let target = app.plugins.activity.iter().find(|activity| activity.title == "Approval needed").unwrap().clone();
        open_activity(&mut app, &target);
        assert_eq!(app.view, app.plugins.view_of("sample-plugin").unwrap());
        assert_eq!(app.plugins.page("sample-plugin").selected.as_deref(), Some(waiting.dir.as_path()));
    }

    #[test]
    fn opening_a_plugin_recovers_orphaned_running_stages() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let mut item = WorkItem::create(&root, "{date}-{slug}", "Interrupted ticket", "", Origin::default(), SystemTime::now()).unwrap();
        item.meta.steps.insert("triage".into(), plugin::StepRecord { state: StepState::Ok, summary: "Ready".into(), ..Default::default() });
        item.meta.steps.insert("echo".into(), plugin::StepRecord { state: StepState::Running, started: "2026-11-17 12:00:00".into(), ..Default::default() });
        item.save().unwrap();
        app.plugins.page("sample-plugin").selected = Some(item.dir.clone());
        app.view = View::Plugin(0);

        render(&mut app, "plugin-interrupted", 1100, 680);

        let recovered = WorkItem::open(&item.dir).unwrap().step("echo");
        assert_eq!(recovered.state, StepState::Interrupted);
        assert!(recovered.summary.contains("retry"));
        assert!(!app.jobs.iter().any(|job| job.proc.is_running()));
    }

    #[test]
    fn a_plugin_step_does_not_start_when_running_state_cannot_be_saved() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| m);
        let item = WorkItem::create(&root, "{date}-{slug}", "Unwritable state", "", Origin::default(), SystemTime::now()).unwrap();
        std::fs::create_dir(item.dir.join("work-item.json.tmp")).unwrap();

        assert!(!run_step(&mut app, "sample-plugin", &item.dir, "echo", Map::new(), false, Purpose::Item));
        assert!(app.jobs.is_empty(), "no external process starts before durable state is saved");
        assert_eq!(WorkItem::open(&item.dir).unwrap().step("echo").state, StepState::NotRun);
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
        for (id, state, summary) in [
            ("parse-ticket", StepState::Ok, "2 link(s), 0 file-sharing, 1 asset tag(s)."),
            ("fetch-links", StepState::Ok, "1 file(s) from 2 link(s)."),
            ("triage", StepState::Ok, "Finished: 1 file(s) changed."),
            ("stage-dropbox", StepState::Ok, "Staged 1 file(s) in the software dropbox."),
            ("runtime", StepState::Ok, "Created the runtime and proved its DELETE guard. YardstickDev commit 2a28d70a."),
            ("check-existing", StepState::Ok, "Checked 212 Yardstick recipe(s) and 412 Intune app(s). No product already exists."),
            ("inventory", StepState::Ok, "Scanned 412 apps in Contoso: 0 possible existing app(s)."),
            ("prepare-test", StepState::Warn, "Prepare test run 20261113-101500-001: faststoneimageviewer: WhatIf."),
        ] {
            let rec = item.meta.steps.entry(id.into()).or_default();
            rec.state = state;
            rec.summary = summary.into();
            rec.finished = "2026-11-13 10:15:00".into();
        }
        for product in ["faststoneimageviewer", "inphotocaptureps"] {
            item.meta.recipe_work.insert(product.into(), RecipeWorkRecord {
                state: RecipeWorkState::Ready,
                batch_id: "recipe-builder-list".into(),
                recipe_id: product.into(),
                note: format!("{product} passed Recipe Builder's safe checks and was synced for One-Off packaging."),
                updated: "2026-11-13 10:14:00".into(),
            });
        }
        item.save().unwrap();
        app.plugins.page("oneoff-packager").selected = Some(item.dir.clone());
        app.view = View::Plugin(0);
        render(&mut app, "oneoff-packager-ticket", 1500, 1500);

        // The pipeline is in the order the page lists it, and a step is ready only after what it needs.
        let m = app.plugins.get("oneoff-packager").unwrap().manifest.clone();
        let order: Vec<&str> = m.pipeline().iter().map(|s| s.id()).collect();
        assert_eq!(
            order,
            [
                "parse-ticket", "fetch-links", "triage", "stage-dropbox", "check-existing", "runtime", "inventory", "prepare-test", "full-test-request",
                "full-test", "build", "prepare-live", "live-change", "cleanup"
            ]
        );
        let item = WorkItem::open(&item.dir).unwrap();
        assert!(item.satisfied(&m.step("full-test-request").unwrap().after), "Prepare finished with warnings, so the request can be written");
        assert!(!item.satisfied(&m.step("build").unwrap().after), "nothing is built before the install test");
        assert!(matches!(m.next_automatic(&item), AutomationNext::Run(Stage::Step(step)) if step.id == "full-test-request"));
        let fresh = WorkItem::create(&root, "{date}-{slug}", "Fresh request", "", Origin::default(), SystemTime::now()).unwrap();
        assert!(matches!(m.next_automatic(&fresh), AutomationNext::Run(Stage::Step(step)) if step.id == "parse-ticket"));

        let mut mixed = WorkItem::create(&root, "{date}-{slug}", "Mixed route request", "TASK-MIXED", Origin::default(), SystemTime::now()).unwrap();
        std::fs::write(mixed.dir.join("ticket.md"), "Please package FastStone Image Viewer and QGIS.").unwrap();
        std::fs::write(mixed.dir.join("products.json"), r#"{"products":[
            {"id":"faststoneimageviewer","name":"FastStone Image Viewer","version":"8.5","status":"needs-research","notes":"Licensed lab request","links":[]},
            {"id":"qgis","name":"QGIS","publisher":"QGIS.org","version":"3.40","status":"needs-research","notes":"Reusable open-source application","links":["https://qgis.org"]}
        ]}"#).unwrap();
        for stage in ["parse-ticket", "fetch-links", "triage", "stage-dropbox", "check-existing"] {
            mixed.meta.steps.insert(stage.into(), plugin::StepRecord { state: StepState::Ok, ..Default::default() });
        }
        mixed.save().unwrap();
        assert!(matches!(m.next_automatic(&mixed), AutomationNext::RoutingReview(panel, 2) if panel.title == "Products"));
        app.plugins.page("oneoff-packager").selected = Some(mixed.dir.clone());
        render(&mut app, "oneoff-product-routing", 1400, 1000);
        render(&mut app, "oneoff-product-routing-min", 1100, 680);
        render(&mut app, "oneoff-product-routing-small", 900, 680);

        let c = context(&app, "oneoff-packager").unwrap();
        let panel = c.plugin.manifest.page.panels.iter().find(|panel| panel.title == "Products").unwrap();
        let row = plugin::move_panel_row(&mixed.dir, panel, 1, true).unwrap();
        let routed_batch_id = crate::views::batch::add_plugin_product(&mut app, product_batch_item(&c, &mixed, &row));
        let mut routed_work = WorkItem::open(&mixed.dir).unwrap();
        routed_work.meta.routed_recipe_work.insert("qgis".into(), routed_batch_id.clone());
        routed_work.save().unwrap();
        app.plugins.page("oneoff-packager").panels = None;
        render(&mut app, "oneoff-product-routed", 1400, 1000);
        let routed = app.batch.batch.as_ref().unwrap().items.iter().find(|item| item.suggested_id == "qgis").unwrap();
        assert_eq!((routed.app.as_str(), routed.publisher.as_str(), routed.version.as_str()), ("QGIS", "QGIS.org", "3.40"));
        assert_eq!(routed.handler_item.as_deref(), Some(mixed.dir.as_path()));
        assert_eq!(WorkItem::open(&mixed.dir).unwrap().meta.routed_recipe_work["qgis"], routed_batch_id);
        assert!(crate::views::batch::open_routed_plugin_product(&mut app, &routed_batch_id, &mixed.dir, "qgis"));
        assert_eq!(plugin::routed_panel_rows(&mixed.dir, panel).unwrap().len(), 1);
        assert!(matches!(m.next_automatic(&mixed), AutomationNext::RoutingReview(_, 1)), "the remaining product still needs explicit confirmation");

        let mut linked = WorkItem::open(&mixed.dir).unwrap();
        linked.meta.routing_reviews.insert("Products".into());
        for stage in ["runtime", "inventory"] {
            linked.meta.steps.insert(stage.into(), plugin::StepRecord { state: StepState::Ok, ..Default::default() });
        }
        linked.save().unwrap();
        assert!(matches!(m.next_automatic(&linked), AutomationNext::RecipeWorkNeeded(_, ids) if ids == ["faststoneimageviewer"]));
        app.batch.batch.as_mut().unwrap().id = dir.path().join("linked-recipe-work").to_string_lossy().into_owned();
        assert!(ensure_recipe_work(&mut app, "oneoff-packager", &linked.dir, panel, &["faststoneimageviewer".into()]));
        let child = app.batch.batch.as_ref().unwrap().items.iter().find(|item| item.suggested_id == "faststoneimageviewer").unwrap();
        assert!(child.plugin_origins.iter().any(|origin| origin.work_item == linked.dir && origin.product_id == "faststoneimageviewer"));
        app.plugins.page("oneoff-packager").selected = Some(linked.dir.clone());
        render(&mut app, "oneoff-recipe-builder-waiting", 1400, 1000);
        let batch_id = app.batch.batch.as_ref().unwrap().id.clone();
        assert!(crate::views::batch::open_plugin_recipe_work(
            &mut app,
            &batch_id,
            &linked.dir,
            Some("faststoneimageviewer"),
        ));
        render(&mut app, "recipe-builder-linked-plugin-work", 1400, 1000);

        let mut existing = WorkItem::create(&root, "{date}-{slug}", "Existing apps", "TASK-EXISTING", Origin::default(), SystemTime::now()).unwrap();
        std::fs::write(existing.dir.join("ticket.md"), "Please install 7-Zip, Zoom and FastStone Image Viewer.").unwrap();
        std::fs::write(existing.dir.join("products.json"), r#"{"products":[
            {"id":"faststoneimageviewer","name":"FastStone Image Viewer","version":"8.5","status":"needs-research","flags":[]}],
          "completedProducts":[
            {"id":"7zip","name":"7-Zip","status":"needs-research","flags":[
              {"label":"Yardstick recipe: 7zip","tone":"green","detail":"Yardstick recipe: Recipes\\7zip.yaml (7-Zip)"},
              {"label":"In Intune","tone":"green","detail":"In Intune: 7-Zip (Igor Pavlov; assigned)"}]},
            {"id":"zoom","name":"Zoom Workplace","status":"needs-research","flags":[
              {"label":"In Intune, no Yardstick recipe","tone":"amber","detail":"In Intune without a Yardstick recipe: Zoom Workplace (Zoom; assigned). Package it anyway to give it a recipe."}]}]}"#).unwrap();
        for stage in ["parse-ticket", "fetch-links", "triage", "stage-dropbox", "check-existing"] {
            existing.meta.steps.insert(stage.into(), plugin::StepRecord { state: StepState::Ok, ..Default::default() });
        }
        existing.save().unwrap();
        assert!(matches!(m.next_automatic(&existing), AutomationNext::RoutingReview(_, 1)), "already-available products get a look first");
        app.plugins.page("oneoff-packager").selected = Some(existing.dir.clone());
        app.plugins.page("oneoff-packager").panels = None;
        app.view = View::Plugin(0);
        render(&mut app, "oneoff-product-existing", 1400, 1000);
        let zoom = plugin::move_panel_row_between(&existing.dir, panel, 1, plugin::RowList::Completed, plugin::RowList::Active).unwrap();
        assert_eq!((zoom["id"].as_str(), zoom["completedOverride"].as_bool()), (Some("zoom"), Some(true)));
        assert_eq!(plugin::active_panel_rows(&existing.dir, panel).unwrap().len(), 2);
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

        let mut source_batch = Batch::from_items(
            "plugin-origin",
            vec![BatchItem {
                app: "Licensed Lab Tool".into(),
                handler: "oneoff-packager".into(),
                status: ItemStatus::Delegated,
                status_note: "Started in One-Off Packager.".into(),
                include: false,
                ..Default::default()
            }],
        );
        source_batch.items[0].handler = "oneoff-packager".into();
        source_batch.items[0].include = false;
        source_batch.id = dir.path().join("plugin-origin-batch").to_string_lossy().into_owned();
        let batch_id = source_batch.id.clone();
        app.batch.batch = Some(source_batch);
        app.batch.selected = Some(0);

        let made = create_from_recipe_builder(
            &mut app,
            "oneoff-packager",
            "Licensed Lab Tool",
            "TASK-42",
            "Requester supplied licensed media.",
            &format!("{batch_id}:0"),
        )
        .unwrap();
        app.batch.batch.as_mut().unwrap().items[0].handler_item = Some(made.clone());
        app.batch.batch.as_ref().unwrap().save().unwrap();
        assert!(made.starts_with(&oneoff));
        assert_eq!(ystudio_core::read_text(&made.join("ticket.md")).unwrap(), "Requester supplied licensed media.");
        let item = WorkItem::open(&made).unwrap();
        assert_eq!((item.meta.origin.kind.as_str(), item.meta.origin.from_plugin.as_str()), ("recipe-builder", "recipe-builder"));
        assert_eq!(item.meta.origin.key, format!("{batch_id}:0"));

        app.plugins.page("oneoff-packager").selected = Some(made.clone());
        render(&mut app, "oneoff-plugin-origin", 1400, 900);
        assert!(crate::views::batch::open_plugin_handoff_source(&mut app, &item.meta.origin.key, &made));
        assert_eq!(app.batch.selected, Some(0));
        render(&mut app, "recipe-builder-plugin-return", 1400, 900);
        render(&mut app, "recipe-builder-plugin-return-narrow", 900, 600);
        render(&mut app, "recipe-builder-plugin-destination", 1400, 900);

        let mut item = WorkItem::open(&made).unwrap();
        item.meta.automation.state = AutomationState::Running;
        item.meta.automation.note = "Running triage.".into();
        item.save().unwrap();
        sync_recipe_builder_origin(&mut app, "oneoff-packager", &made);
        let source = &app.batch.batch.as_ref().unwrap().items[0];
        assert_eq!(source.status, ItemStatus::Delegated);
        assert!(source.status_note.starts_with("Running in One-Off Packager"), "{}", source.status_note);

        item.meta.automation.state = AutomationState::Complete;
        item.meta.automation.note = "All products were routed to Recipe Builder.".into();
        item.save().unwrap();
        sync_recipe_builder_origin(&mut app, "oneoff-packager", &made);
        let source = &app.batch.batch.as_ref().unwrap().items[0];
        assert_eq!(source.status, ItemStatus::Done);
        assert!(source.status_note.starts_with("Complete in One-Off Packager"), "{}", source.status_note);
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
        let Some(plugin::ActionDef { kind: ActionKind::HostPluginWorkItem { plugin: target, files, title }, .. }) = m.actions.iter().find(|a| a.id == "to-oneoff").cloned() else { panic!("no to-oneoff action") };
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
                .replace("  hostActions: [new-work, refresh, work-folder, settings]\n", "  hostActions: [work-folder, settings]\n")
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

    #[test]
    fn safe_pipeline_runs_opted_in_work_and_stops_for_confirmation() {
        let (dir, mut app) = fake_app();
        let root = install_sample(&mut app, dir.path(), "sample-plugin", |m| {
            m.replace("    after: [triage]\n  - id: confirmed", "    automatic: true\n  - id: confirmed")
                .replace("  - id: confirmed\n", "  - id: confirmed\n    automatic: true\n")
        });
        let item = WorkItem::create(&root, "{date}-{ref}-{slug}", "Automatic echo", "", Origin::default(), SystemTime::now()).unwrap();

        assert!(set_automation(
            &mut app,
            "sample-plugin",
            &item.dir,
            AutomationState::Running,
            "Starting the safe pipeline.",
            true,
        ));
        advance_automation(&mut app, "sample-plugin", &item.dir);
        assert_eq!(WorkItem::open(&item.dir).unwrap().step("echo").state, StepState::Running);

        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while app.jobs.iter().any(|j| j.proc.is_running()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        app.poll_for_tests();

        let finished = WorkItem::open(&item.dir).unwrap();
        assert_eq!(finished.step("echo").state, StepState::Ok);
        assert_eq!(finished.step("confirmed").state, StepState::NotRun);
        assert_eq!(finished.meta.automation.state, AutomationState::AwaitingAction);
        assert!(finished.meta.automation.note.contains("Confirmed echo"), "{}", finished.meta.automation.note);
        assert!(app.jobs.iter().all(|job| !job.proc.is_running()), "the consent-gated step did not start");
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
                StepState::Interrupted => "interrupted",
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
    let before = Snapshot::of_dir(dir);
    let mut item = match WorkItem::open(dir) {
        Ok(item) => item,
        Err(e) => {
            app.error(format!("Could not record {} before it starts: {e:#}", task.label));
            return false;
        }
    };
    let rec = item.meta.steps.entry(task.id.clone()).or_default();
    rec.state = StepState::Running;
    rec.started = ystudio_core::intake::timestamp();
    rec.finished.clear();
    rec.summary.clear();
    rec.result = None;
    if task.automatic && matches!(item.meta.automation.state, AutomationState::AwaitingAction | AutomationState::Paused) {
        item.meta.automation.state = AutomationState::Running;
        item.meta.automation.updated = ystudio_core::intake::timestamp();
        item.meta.automation.note = format!("Running {}.", task.label);
    }
    if let Err(e) = item.save() {
        app.error(format!("Could not record {} before it starts: {e:#}. Nothing was run.", task.label));
        return false;
    }
    let kind = JobKind::PluginAgent { plugin: id.to_string(), dir: dir.to_path_buf(), task: task.id.clone(), before };
    let Some(job) = app.start_job(spec, kind) else {
        mark_interrupted(app, id, dir, &task.id, "The agent process could not be started. Review the error, then retry this stage.");
        return false;
    };
    app.plugins.locks.insert(job, lock);
    app.plugins.page(id).agent_changes.remove(dir);
    app.plugins.page(id).refresh_items(&root);
    true
}

/// Records a finished agent task: what it changed, what stopped it, and
/// whether it wrote what the task expects.
pub fn agent_finished(app: &mut StudioApp, job: u64, id: &str, dir: &Path, task_id: &str, before: Snapshot) {
    app.plugins.locks.remove(&job);
    let (ok, cancelled, text) = app.job(job).map(|j| (j.proc.succeeded(), j.proc.cancelled, j.proc.report_text())).unwrap_or((false, false, String::new()));
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
    let mut save_error = None;
    match WorkItem::open(dir) {
        Ok(mut item) => {
            let rec = item.meta.steps.entry(task_id.to_string()).or_default();
            rec.state = state;
            rec.finished = ystudio_core::intake::timestamp();
            rec.summary = summary.clone();
            if let Err(e) = item.save() {
                save_error = Some(format!("its result could not be saved: {e:#}"));
            }
        }
        Err(e) => save_error = Some(format!("its work item could not be opened: {e:#}")),
    }
    {
        let page = app.plugins.page(id);
        page.agent_changes.insert(dir.to_path_buf(), changes);
        page.panels = None;
        if let Some(root) = &c.root {
            page.refresh_items(root);
        }
    }
    let label = stage_label(&c, task_id);
    let msg = format!("{} · {label}: {summary}", c.plugin.manifest.name);
    if let Some(e) = &save_error {
        app.error(format!("{label} finished, but {e}"));
    } else if matches!(state, StepState::Ok | StepState::Warn) {
        app.notify(msg);
    } else {
        app.error(msg);
    }
    if save_error.is_none()
        && let Ok(item) = WorkItem::open(dir)
        && item.meta.automation.state == AutomationState::Running
    {
        if matches!(state, StepState::Ok | StepState::Warn) {
            advance_automation(app, id, dir);
        } else {
            set_automation(
                app,
                id,
                dir,
                AutomationState::Paused,
                format!("{label} did not finish successfully. Review or retry it, then continue safe stages."),
                false,
            );
        }
    }
    if save_error.is_none() {
        sync_recipe_builder_origin(app, id, dir);
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
