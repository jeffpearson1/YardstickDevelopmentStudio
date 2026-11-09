//! Studio-owned, sequential harness runs and bounded agent repairs.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;

use eframe::egui::{self, RichText, Ui};
use ystudio_core::agent::{self, AgentTask, Snapshot};
use ystudio_core::harness::{self, Consent, HarnessAction, InstallTestOptions, LabStage};
use ystudio_core::intake::BatchCheck;
use ystudio_core::lint::{self, Severity};
use ystudio_core::results::{self, RecipeResult, Verdict};

use crate::app::{JobKind, Step, StepState, StudioApp};
use crate::theme;

#[derive(Clone, Debug)]
pub struct Options {
    pub live: bool,
    pub attempts: u32,
    consent: Consent,
}

impl Default for Options {
    fn default() -> Self {
        Self { live: true, attempts: 5, consent: Consent::None }
    }
}

#[derive(Clone)]
pub struct Verification {
    pub recipe_id: String,
    pub options: Options,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub id: String,
    pub items: Vec<(String, usize)>,
}

pub enum Request {
    Agent(AgentTask),
    BatchAuthor(Vec<usize>),
    BatchTests(Vec<Target>),
    /// Existing recipes no check has confirmed: test them, and hand failures to the agent.
    BatchRepair(Vec<Target>),
}

#[derive(Default)]
pub struct State {
    pub options: Options,
    pub request: Option<Request>,
    /// Where the requested recipes' live tests would run, read once per request.
    contexts: Contexts,
    ack: bool,
    run: Option<Run>,
    pub last_job: Option<u64>,
    pub message: String,
    advance_pending: bool,
}

/// Recipe counts by where their live test runs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Contexts {
    system: usize,
    user: usize,
    /// Not authored yet; system-context is the default.
    unknown: usize,
}

impl Contexts {
    fn elevated(self) -> bool {
        self.system + self.unknown > 0
    }

    fn summary(self) -> String {
        let mut parts = Vec::new();
        if self.system > 0 {
            parts.push(format!("{} system-context (one elevated session)", self.system));
        }
        if self.user > 0 {
            parts.push(format!("{} user-scope (unelevated)", self.user));
        }
        if self.unknown > 0 {
            parts.push(format!("{} not authored yet (assumed system-context)", self.unknown));
        }
        parts.join(" · ")
    }
}

struct Run {
    root: PathBuf,
    queue: VecDeque<Target>,
    current: Target,
    options: Options,
    repair: bool,
    resume_batch: bool,
    attempt: u32,
    step: usize,
    stopped: bool,
    cancelled: bool,
    summary_dir: Option<PathBuf>,
    warned: bool,
    /// Whether the current recipe's live test ran elevated.
    elevated: Option<bool>,
    /// The live step is waiting for the UAC prompt to be answered.
    waiting_elevation: bool,
}

impl Run {
    fn can_repair(&self, blocker: bool) -> bool {
        !blocker && self.repair && !self.cancelled && self.attempt < self.options.attempts
    }
}

impl State {
    pub fn busy(&self) -> bool {
        self.run.is_some() || self.request.is_some()
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }
}

pub fn configure(options: &mut Options, ui: &mut Ui, repairs: bool) {
    options.consent = Consent::None;
    ui.checkbox(&mut options.live, "Include live install/uninstall tests (requires consent)");
    if repairs {
        ui.horizontal(|ui| {
            ui.label("Maximum test-and-fix attempts per recipe");
            ui.add(egui::DragValue::new(&mut options.attempts).range(1..=20));
        });
    }
}

pub fn busy(app: &StudioApp) -> bool {
    app.workflow.busy()
        || app.jobs.iter().any(|j| j.proc.is_running()
            && matches!(j.kind, JobKind::Agent { .. } | JobKind::BatchExtract { .. }
                | JobKind::Harness { .. } | JobKind::WorkflowHarness { .. } | JobKind::WorkflowRepair { .. }))
        || !app.bench.queue.is_empty() || app.bench.live_modal
}

pub fn request(app: &mut StudioApp, request: Request) {
    if busy(app) || !app.batch.queue.is_empty() || !app.writable() {
        app.error("Finish the active agent/test run and use a writable development workspace first.");
        return;
    }
    if app.recipe.as_ref().is_some_and(|r| r.dirty()) {
        app.error("Save or discard the open recipe's edits before starting automatic checks.");
        return;
    }
    app.workflow.ack = false;
    app.workflow.contexts = contexts(app, &request);
    app.workflow.request = Some(request);
}

fn contexts(app: &StudioApp, request: &Request) -> Contexts {
    match request {
        Request::Agent(_) => Contexts { unknown: 1, ..Contexts::default() },
        Request::BatchAuthor(indices) => Contexts { unknown: indices.len(), ..Contexts::default() },
        Request::BatchTests(targets) | Request::BatchRepair(targets) => {
            let mut c = Contexts::default();
            for path in targets.iter().map(|t| recipe_path(app, &t.id, false)) {
                match path.filter(|p| p.is_file()) {
                    Some(p) if harness::runs_elevated(&p) => c.system += 1,
                    Some(_) => c.user += 1,
                    None => c.unknown += 1,
                }
            }
            c
        }
    }
}

/// Authoring and repairs always target the Development copy; testing may use a shipped recipe.
fn recipe_path(app: &StudioApp, id: &str, repair: bool) -> Option<PathBuf> {
    let path = app.ws.as_ref()?.development_recipe_path(id);
    Some(if path.is_file() || repair { path } else {
        app.recipes.iter().find(|r| r.id == id).map(|r| r.path.clone()).unwrap_or(path)
    })
}

/// Whether an agent or a check run is working on an item of `batch_id`.
pub fn works_on(app: &StudioApp, batch_id: &str) -> bool {
    let agent = app.jobs.iter().any(|j| {
        j.proc.is_running() && matches!(&j.kind, JobKind::Agent { batch_item: Some((b, _)), .. } if b == batch_id)
    });
    agent || app.workflow.run.as_ref().is_some_and(|r| std::iter::once(&r.current).chain(&r.queue).any(|t| t.items.iter().any(|(b, _)| b == batch_id)))
}

pub fn task_id(task: &AgentTask) -> Option<String> {
    match task {
        AgentTask::Create { app, id_hint, .. } => Some(if id_hint.trim().is_empty() {
            ystudio_core::recipe::suggest_id(app)
        } else { id_hint.trim().to_string() }),
        AgentTask::Fix { id, .. } | AgentTask::Update { id, .. } => Some(id.clone()),
        _ => None,
    }
}

pub fn modal(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(request) = &app.workflow.request else { return };
    let shipped_only = |id: &str| app.ws.as_ref().is_some_and(|ws| !ws.development_recipe_path(id).is_file());
    let (heading, labels, repairs) = match request {
        Request::Agent(task) => ("Create, test and repair recipes", vec![task.title()], true),
        Request::BatchAuthor(indices) => ("Create, test and repair recipes",
            indices.iter().filter_map(|&i| app.batch.batch.as_ref()?.items.get(i))
                .map(|it| format!("{} ({})", it.app, it.recipe_id())).collect(), true),
        Request::BatchTests(targets) => ("Test selected batch recipes", targets.iter().map(|t| t.id.clone()).collect(), false),
        Request::BatchRepair(targets) => ("Repair batch recipes with agent", targets.iter().map(|t| if shipped_only(&t.id) {
            format!("{} (shipped recipe; copied into Recipes\\Development first)", t.id)
        } else { t.id.clone() }).collect(), true),
    };
    let repairing_tested = matches!(request, Request::BatchRepair(_));
    let mut go = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("workflow-consent")).show(ctx, |ui| {
        ui.set_max_width(580.0);
        ui.heading(heading);
        egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
            for label in &labels { ui.label(label); }
        });
        if repairing_tested {
            ui.label("These recipes have no passing check yet. Each is tested first; a recipe that passes is left unchanged.");
        }
        ui.label("Studio runs lint, schema, download, package and dry-run checks sequentially.");
        if repairs {
            ui.label(format!("Up to {} test attempts per recipe. Recipe failures go back to the agent; environmental blockers stop repair.", app.workflow.options.attempts));
        } else {
            ui.label("No recipes are changed. A failure stops that recipe, then testing continues with the next.");
        }
        if app.workflow.options.live {
            let warning = if repairs {
                "Live checks INSTALL and UNINSTALL real software on THIS machine, including repeated runs after repairs."
            } else {
                "Live checks INSTALL and UNINSTALL real software on THIS machine."
            };
            ui.label(RichText::new(warning).color(theme::red()));
            let contexts = app.workflow.contexts;
            if contexts.elevated() {
                ui.label(format!(
                    "{}  One UAC prompt for Yardstick Development Studio appears when you start. A hidden elevated worker then runs the system-context live tests and closes when this run ends.",
                    crate::elevation::SHIELD
                ));
                if repairs {
                    ui.label("Recipes the repair agent edits are tested again in that session without another prompt.");
                }
            } else {
                ui.label("Every listed recipe is user-scope, so live tests run unelevated with no UAC prompt.");
            }
            ui.label(RichText::new(contexts.summary()).weak());
            ui.label("No -Force or SkipUninstall: already-installed software may block full validation. Promotion and Intune deployment are never automatic.");
            ui.checkbox(&mut app.workflow.ack, "I consent to live install/uninstall tests for all applications listed above.");
        }
        ui.horizontal(|ui| {
            if theme::primary(ui, !app.workflow.options.live || app.workflow.ack, "Start").clicked() { go = true; }
            if ui.button("Go back").clicked() { cancel = true; }
        });
    });
    if cancel {
        app.workflow.request = None;
        app.workflow.ack = false;
    } else if go {
        let request = app.workflow.request.take().unwrap();
        app.workflow.ack = false;
        let mut options = app.workflow.options.clone();
        options.consent = if options.live { Consent::Confirmed } else { Consent::None };
        // Before any UAC prompt, so a failed copy asks for nothing.
        if let Request::BatchRepair(targets) = &request
            && let Err(e) = copy_shipped_to_development(app, targets)
        {
            app.error(format!("Could not prepare the repairs: {e:#}"));
            return;
        }
        if options.live && app.workflow.contexts.elevated() {
            crate::elevation::begin(app);
        }
        match request {
            Request::Agent(task) => crate::views::agent::AgentState::start(app, task, Some(options)),
            Request::BatchAuthor(indices) => crate::views::batch::start_authoring(app, indices, options),
            Request::BatchTests(targets) => start(app, targets, options, false, false),
            Request::BatchRepair(targets) => start(app, targets, options, true, false),
        }
    }
}

/// Repairs edit only Development copies, so a recipe that exists only as a
/// shipped recipe is copied there first under the same id (overriding the
/// shipped one while it is worked on). The shipped file is never changed.
fn copy_shipped_to_development(app: &mut StudioApp, targets: &[Target]) -> anyhow::Result<()> {
    let Some(ws) = app.ws.clone() else { anyhow::bail!("No workspace is open.") };
    let mut copied = false;
    for target in targets {
        if ws.development_recipe_path(&target.id).is_file() {
            continue;
        }
        let Some(src) = app.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(&target.id)).map(|r| (r.path.clone(), r.id.clone())) else {
            anyhow::bail!("Recipe {} was not found.", target.id);
        };
        ws.copy_to_development(&src.0, &src.1, &target.id)?;
        copied = true;
    }
    if copied {
        app.refresh_recipes();
    }
    Ok(())
}

pub fn start(app: &mut StudioApp, targets: Vec<Target>, options: Options, repair: bool, resume_batch: bool) {
    if options.live && options.consent != Consent::Confirmed {
        app.error("Live automatic checks require explicit consent for this run.");
        return;
    }
    let Some(root) = app.ws.as_ref().map(|w| w.root.clone()) else {
        app.error("No workspace is open for automatic checks.");
        return;
    };
    crate::views::batch::checks_started(app, &targets);
    let mut queue: VecDeque<_> = targets.into();
    let Some(current) = queue.pop_front() else {
        app.error("No recipe was produced or selected for testing.");
        return;
    };
    app.workflow.run = Some(Run {
        root, queue, current, options, repair, resume_batch, attempt: 1, step: 0,
        stopped: false, cancelled: false, summary_dir: None, warned: false, elevated: None, waiting_elevation: false,
    });
    app.workflow.advance_pending = false;
    advance(app);
}

pub fn poll(app: &mut StudioApp) {
    let answered = app.workflow.run.as_ref().is_some_and(|r| r.waiting_elevation) && !crate::elevation::waiting(app);
    if std::mem::take(&mut app.workflow.advance_pending) || answered {
        if let Some(run) = &mut app.workflow.run {
            run.waiting_elevation = false;
        }
        advance(app);
    }
}

fn advance(app: &mut StudioApp) {
    loop {
        let Some(run) = &app.workflow.run else { return };
        if app.ws.as_ref().is_none_or(|w| !ystudio_core::same_path(&w.root, &run.root) || !w.is_writable()) {
            finish(app, false, "Workspace changed; automatic checks stopped.".into());
            return;
        }
        if run.cancelled {
            finish(app, false, "Cancelled; live-test cleanup may need manual review.".into());
            return;
        }
        let count = if run.options.live { Step::ALL.len() } else { Step::ALL.len() - 1 };
        if run.step == count {
            let note = if !run.options.live { "Safe checks passed; live test still required" }
                else if run.warned { "Validated with warnings; review harness evidence" } else { "Validated" };
            finish(app, true, note.into());
            return;
        }
        let step = Step::ALL[run.step];
        let id = run.current.id.clone();
        let attempt = run.attempt;
        app.workflow.message = format!("{}: {} (attempt {attempt}/{})", id, step.title(), run.options.attempts);
        let consent = if step == Step::Live { run.options.consent } else { Consent::None };
        let Some(path) = recipe_path(app, &id, run.repair) else { return };
        if !ystudio_core::recipe::is_valid_id(&id) || !path.is_file() {
            finish(app, false, format!("Recipe {id} is missing; create its Development copy first."));
            return;
        }
        if step == Step::Lint {
            app.bench.steps.retain(|(recipe_id, _), _| recipe_id != &id);
            let text = match ystudio_core::read_text(&path) {
                Ok(text) => text,
                Err(e) => { finish(app, false, format!("Could not read {id}: {e:#}")); return; }
            };
            let diagnostics = match app.lint_diagnostics(&text, &id) {
                Ok(diagnostics) => diagnostics,
                Err(e) => { finish(app, false, e); return; }
            };
            let failed = lint::count(&diagnostics, Severity::Error) > 0;
            let state = if failed { StepState::Fail } else if lint::count(&diagnostics, Severity::Warning) > 0 { StepState::Warn } else { StepState::Pass };
            app.bench.steps.insert((id, step), state);
            if failed {
                let errors: Vec<&str> = diagnostics.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.as_str()).collect();
                let summary = format!("Studio lint: {}", errors.join("; "));
                fail(app, format!("{diagnostics:?}"), summary, false);
                return;
            }
            app.workflow.run.as_mut().unwrap().step += 1;
            continue;
        }
        if step == Step::Live && harness::runs_elevated(&path) {
            let proc = match crate::elevation::install_test(app, &path) {
                crate::elevation::Live::Started(proc) => proc,
                crate::elevation::Live::Waiting => {
                    app.workflow.message = format!("{id}: waiting for UAC approval to run the system-context live test");
                    app.workflow.run.as_mut().unwrap().waiting_elevation = true;
                    return;
                }
                crate::elevation::Live::Blocked(reason) => {
                    finish(app, false, format!("Blocked: {reason} The system-context live test did not run."));
                    return;
                }
            };
            app.workflow.run.as_mut().unwrap().elevated = Some(true);
            let job = app.add_job(*proc, JobKind::WorkflowHarness { step, recipe_id: id.clone() });
            track(app, id, step, Some(job), " · elevated session");
            return;
        }
        let action = match step {
            Step::Schema => HarnessAction::Schema { recipe: path },
            Step::Download => HarnessAction::Lab { recipe: path, stage: LabStage::Download },
            Step::Package => HarnessAction::Lab { recipe: path, stage: LabStage::Package },
            Step::DryRun => HarnessAction::InstallTest { recipes: vec![path], options: InstallTestOptions::dry_run() },
            // User scope: the harness must never raise a UAC prompt of its own.
            Step::Live => HarnessAction::InstallTest { recipes: vec![path], options: InstallTestOptions { no_elevate: true, ..InstallTestOptions::live() } },
            Step::Lint => unreachable!(),
        };
        let Some(ws) = &app.ws else { return };
        let spec = match harness::build(ws, &action, consent, &app.pwsh()) {
            Ok(spec) => spec,
            Err(e) => { finish(app, false, format!("Could not build harness command: {e:#}")); return; }
        };
        let job = app.start_job(spec, JobKind::WorkflowHarness { step, recipe_id: id.clone() });
        let context = if step == Step::Live {
            app.workflow.run.as_mut().unwrap().elevated = Some(false);
            " · unelevated (user scope)"
        } else { "" };
        track(app, id, step, job, context);
        return;
    }
}

/// Points the logs at a started harness job and marks the step running.
fn track(app: &mut StudioApp, id: String, step: Step, job: Option<u64>, context: &str) {
    app.workflow.last_job = job;
    app.bench.last_job = job;
    app.agent.job = job;
    let items = app.workflow.run.as_ref().map(|r| r.current.items.clone()).unwrap_or_default();
    if !items.is_empty() {
        app.batch.run_job = job;
    }
    app.console_open = true;
    if job.is_some() {
        crate::views::batch::checks_progress(app, &items, &format!("{}{context}", step.title()));
        app.bench.steps.insert((id, step), StepState::Running);
    } else {
        finish(app, false, "Could not start the harness; check the console.".into());
    }
}

/// A zero exit code is not sufficient: dry/live runs must contain matching, fresh evidence.
fn evidence_passes(step: Step, ok: bool, result: Option<&RecipeResult>) -> bool {
    ok && match step {
        Step::Live => result.is_some_and(|r| matches!(r.verdict(), Verdict::Validated | Verdict::ValidatedWithWarnings)),
        Step::DryRun => result.is_some_and(|r| r.verdict() == Verdict::DryRunPassed),
        _ => true,
    }
}

fn environmental(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["msi 1618", "exit code 1618", "exitcode: 1618", "another installation is already in progress",
        "access is denied", "requires elevation", "elevation was cancelled", "user canceled", "user cancelled",
        "intunewin32app module is not", "no licensed installer", "no installer found in",
        "already installed", "already detected", "elevated worker ended", "refused by the elevated worker"].iter().any(|s| text.contains(s))
}

pub fn harness_finished(app: &mut StudioApp, step: Step, id: &str, ok: bool, started: SystemTime, text: String) {
    if app.workflow.run.is_none() { return; }
    let run = if matches!(step, Step::DryRun | Step::Live) {
        app.ws.as_ref().and_then(|w| results::newest_run_since(&w.layout.harness_artifacts, started))
    } else { None };
    let result = run.as_ref().and_then(|r| r.summary.as_ref()).and_then(|s| s.results.iter().find(|r| r.id == id));
    let passed = evidence_passes(step, ok, result)
        && (!matches!(step, Step::DryRun | Step::Live) || run.as_ref().is_some_and(|r| r.error.is_none()));
    let warning = result.is_some_and(|r| r.verdict() == Verdict::ValidatedWithWarnings);
    let details = result.map(|r| format!("\nHarness evidence: {r:#?}")).unwrap_or_default();
    let evidence = format!("{} for {id}\n{text}{details}", step.title());
    let why = match result {
        Some(r) => r.problems(),
        None if matches!(step, Step::DryRun | Step::Live) && ok => "no matching harness evidence".into(),
        None => last_error(&text),
    };
    let summary = format!("{}: {why}", step.title());
    app.bench.steps.insert((id.into(), step), if passed {
        if warning { StepState::Warn } else { StepState::Pass }
    } else { StepState::Fail });
    if let Some(run) = run {
        app.workflow.run.as_mut().unwrap().summary_dir = Some(run.dir.clone());
        app.refresh_runs();
        if passed && step == Step::Live {
            app.record_validation(id, started, &run);
        }
        app.bench.last_run = Some(run);
    }
    if app.workflow.run.as_ref().is_some_and(|r| r.cancelled) {
        finish(app, false, "Cancelled; live-test cleanup may need manual review.".into());
    } else if passed {
        app.workflow.run.as_mut().unwrap().warned |= warning;
        app.workflow.run.as_mut().unwrap().step += 1;
        advance(app);
    } else {
        // Missing/skipped evidence is not a recipe defect and must not trigger edits.
        let blocker = environmental(&evidence) || (ok && !passed);
        fail(app, evidence, summary, blocker);
    }
}

/// The most telling output line: the last one mentioning an error, else the last one.
fn last_error(text: &str) -> String {
    let lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("> ") && !l.starts_with("Exited with code"));
    let lines: Vec<&str> = lines.collect();
    let line = lines.iter().rev().find(|l| l.to_ascii_lowercase().contains("error")).or(lines.last());
    line.map_or_else(|| "no output".into(), |l| l.to_string())
}

/// `evidence` is the full record for the repair agent; `summary` is one line
/// for status messages and batch notes.
fn fail(app: &mut StudioApp, evidence: String, summary: String, blocker: bool) {
    let Some(run) = &mut app.workflow.run else { return };
    if !run.can_repair(blocker) {
        let prefix = if blocker { "Blocked (environment or incomplete harness evidence)" } else { "Checks failed" };
        finish(app, false, format!("{prefix}: {summary}"));
        return;
    }
    run.attempt += 1;
    run.step = 0;
    run.summary_dir = None;
    run.warned = false;
    let id = run.current.id.clone();
    let task = AgentTask::Fix {
        id: id.clone(), evidence,
        notes: "Fix only this recipe's failure. Studio will re-run every check after your reply. Do not run live installs yourself. For environmental blockers report STUDIO-BLOCKED: <reason> without editing the recipe.".into(),
    };
    let ws = app.ws.as_ref().unwrap();
    let spec = match agent::build_command(ws, app.settings.agent_backend, &app.settings.agent_model, &app.settings.production_paths, &task) {
        Ok(spec) => spec,
        Err(e) => { finish(app, false, format!("Could not start repair: {e:#}")); return; }
    };
    let before = Snapshot::take(ws);
    app.workflow.message = format!("Repairing {id}; next test attempt {}", app.workflow.run.as_ref().unwrap().attempt);
    let job = app.start_job(spec, JobKind::WorkflowRepair { before });
    app.workflow.last_job = job;
    app.agent.job = job;
    if app.workflow.run.as_ref().is_some_and(|r| !r.current.items.is_empty()) {
        app.batch.run_job = job;
    }
    if job.is_none() { finish(app, false, "Could not start the repair agent.".into()); }
}

/// `text` on one line, cut to `limit` characters.
fn brief(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= limit { line } else { format!("{}…", line.chars().take(limit).collect::<String>()) }
}

pub fn repair_finished(app: &mut StudioApp, before: Snapshot, ok: bool, text: &str) {
    if app.workflow.run.is_none() { return; }
    let mut reload = None;
    if let Some(ws) = &app.ws {
        let changes = before.diff(&Snapshot::take(ws));
        reload = app.recipe.as_ref().filter(|r| !r.dirty() && changes.iter()
            .any(|c| ystudio_core::same_path(c.path(), &r.path))).map(|r| r.path.clone());
        app.agent.changes.extend(changes);
    }
    app.refresh_recipes();
    if let Some(path) = reload { app.open_recipe(&path); }
    if app.workflow.run.as_ref().is_some_and(|r| r.cancelled) {
        finish(app, false, "Cancelled.".into());
    } else if !ok || agent::blocked_reason(text).is_some() {
        let why = agent::blocked_reason(text).unwrap_or_else(|| "it exited with an error; see the console".into());
        finish(app, false, format!("Repair agent stopped: {why}"));
    } else {
        advance(app);
    }
}

fn finish(app: &mut StudioApp, passed: bool, note: String) {
    let Some(mut run) = app.workflow.run.take() else { return };
    // Notes appear in the progress line, batch rows and the copied report.
    let note = brief(&note, 400);
    app.workflow.message = format!("{}: {note}", run.current.id);
    let context = match run.elevated {
        Some(true) => ", live test elevated",
        Some(false) => ", live test unelevated",
        None => "",
    };
    let check = BatchCheck {
        recipe: run.current.id.clone(), passed, note: format!("{note} ({} test attempt(s){context})", run.attempt),
        timestamp: ystudio_core::intake::timestamp(), summary_dir: run.summary_dir.clone(), live: passed && run.options.live,
    };
    for (batch_id, idx) in &run.current.items {
        crate::views::batch::check_finished(app, batch_id, *idx, check.clone());
    }
    if passed { app.notify(app.workflow.message.clone()); } else { app.error(app.workflow.message.clone()); }
    if !run.stopped && !run.cancelled && let Some(next) = run.queue.pop_front() {
        run.current = next;
        run.attempt = 1;
        run.step = 0;
        run.summary_dir = None;
        run.warned = false;
        run.elevated = None;
        app.workflow.run = Some(run);
        app.workflow.advance_pending = true;
        (app.waker)();
    } else if run.resume_batch && !run.cancelled {
        crate::views::batch::start_next(app);
    } else {
        for target in run.queue {
            for (batch_id, idx) in target.items {
                crate::views::batch::check_finished(app, &batch_id, idx, BatchCheck {
                    recipe: target.id.clone(), passed: false, note: "Not tested: run stopped or cancelled".into(),
                    timestamp: ystudio_core::intake::timestamp(), summary_dir: None, live: false,
                });
            }
        }
    }
}

pub fn stop(app: &mut StudioApp, cancel: bool) {
    if let Some(run) = &mut app.workflow.run {
        run.stopped = true;
        run.cancelled |= cancel;
        // Nothing is running that could finish; let advance() see the cancellation.
        app.workflow.advance_pending |= cancel && run.waiting_elevation;
    }
    if cancel && let Some(id) = app.workflow.last_job && let Some(job) = app.job_mut(id) {
        job.proc.cancel();
    }
    crate::views::batch::stop_queue(app);
}

pub fn progress(app: &mut StudioApp, ui: &mut Ui) {
    crate::elevation::strip(app, ui);
    // One line: a long blocker reason must not push the page over the logs.
    let message = |ui: &mut Ui, text: &str| {
        ui.add(egui::Label::new(text).truncate()).on_hover_text(text);
    };
    if app.workflow.run.is_none() {
        if !app.workflow.message.is_empty() {
            ui.horizontal(|ui| message(ui, &app.workflow.message));
        }
        return;
    }
    ui.horizontal(|ui| {
        ui.spinner();
        if ui.button("Stop after this recipe").clicked() {
            stop(app, false);
        }
        if ui.button("Cancel now").clicked() {
            stop(app, true);
        }
        message(ui, &app.workflow.message);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_requires_complete_matching_evidence_and_successful_exit() {
        assert!(!evidence_passes(Step::Live, true, None));
        assert!(!evidence_passes(Step::Live, true, Some(&RecipeResult::default())));
        let result = RecipeResult {
            phases: results::VALIDATION_PHASES.iter().map(|name| results::Phase {
                name: Some((*name).into()), status: "Pass".into(), ..Default::default()
            }).collect(), ..Default::default()
        };
        assert!(evidence_passes(Step::Live, true, Some(&result)));
        assert!(!evidence_passes(Step::Live, false, Some(&result)));
    }

    #[test]
    fn environmental_failures_do_not_trigger_recipe_repairs() {
        assert!(environmental("MSI exit code 1618"));
        assert!(environmental("App already installed; refusing without -Force"));
        assert!(!environmental("PostInstallDetect failed: wrong detection rule"));
    }

    #[test]
    fn creation_consent_is_scoped_to_one_known_recipe_id() {
        let task = AgentTask::Create { app: "My App".into(), id_hint: String::new(), vendor_url: String::new(), notes: String::new() };
        assert_eq!(task_id(&task), Some("myapp".into()));
        assert_eq!(task_id(&AgentTask::Review { id: "x".into() }), None);
    }

    #[test]
    fn five_attempts_allow_exactly_four_repairs_and_never_repair_blockers() {
        let mut run = Run {
            root: PathBuf::new(), queue: VecDeque::new(), current: Target { id: "test".into(), items: Vec::new() },
            options: Options::default(), repair: true, resume_batch: false, attempt: 1, step: 0,
            stopped: false, cancelled: false, summary_dir: None, warned: false, elevated: None, waiting_elevation: false,
        };
        for expected in 1..=5 {
            assert_eq!(run.attempt, expected);
            assert!(!run.can_repair(true));
            assert_eq!(run.can_repair(false), expected < 5);
            if run.can_repair(false) { run.attempt += 1; }
        }
        run.attempt = 1;
        run.cancelled = true;
        assert!(!run.can_repair(false));
        run.cancelled = false;
        run.repair = false;
        assert!(!run.can_repair(false));
    }

    #[test]
    fn default_live_option_is_not_consent() {
        let options = Options::default();
        assert!(options.live);
        assert_eq!(options.consent, Consent::None);
    }

    fn fake_app() -> (tempfile::TempDir, StudioApp) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\nRuntimeRoot = '.'\nRecipesPath = 'Recipes'\nDevelopmentRecipes = 'Recipes\\Development'\n}\n").unwrap();
        std::fs::write(root.join("Yardstick.ps1"), "# never executed").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        std::fs::create_dir_all(root.join("Icons")).unwrap();
        std::fs::write(root.join(r"Icons\app.png"), [0]).unwrap();
        std::fs::write(root.join(r"Recipes\Development\app.yaml"), "id: app\nurl: https://example.com/app.msi\nversion: '1.0'\ninstallScript: msiexec /i <filename> /qn\nuninstallScript: msiexec /x <productcode> /qn\niconFile: app.png\ndisplayName: App\npublisher: Test\ndescription: Fake harness app\ndetectionType: msi\n").unwrap();
        let text = std::fs::read_to_string(root.join(r"Recipes\Development\app.yaml")).unwrap().replace("id: app", "id: broken");
        std::fs::write(root.join(r"Recipes\Development\broken.yaml"), &text).unwrap();
        let user = text.replace("id: broken", "id: user").replace("displayName: App", "displayName: App\ninstallExperience: user");
        std::fs::write(root.join(r"Recipes\Development\user.yaml"), user).unwrap();
        std::fs::write(root.join(r"Recipes\Development\present.yaml"), text.replace("id: broken", "id: present")).unwrap();
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        std::fs::create_dir_all(&ws.layout.harness).unwrap();
        let trace = harness::ps_quote(&root.join("trace.txt").to_string_lossy());
        std::fs::write(ws.layout.harness.join("Test-RecipeSchema.ps1"),
            format!("param([string]$RecipePath)\nAdd-Content -LiteralPath {trace} -Value Schema\nif ([IO.Path]::GetFileNameWithoutExtension($RecipePath) -eq 'broken') {{ throw 'Fake schema failure' }}\n")).unwrap();
        std::fs::write(ws.layout.harness.join("Invoke-RecipeLab.ps1"),
            format!("param([string]$RecipePath, [string]$Stage)\nAdd-Content -LiteralPath {trace} -Value $Stage\n")).unwrap();
        let artifacts = harness::ps_quote(&ws.layout.harness_artifacts.to_string_lossy());
        std::fs::write(ws.layout.harness.join("Invoke-RecipeInstallTest.ps1"), format!(r#"
param([string[]]$RecipePath, [switch]$WhatIf, [switch]$NoElevate, [switch]$DiscardInstaller, [switch]$ElevatedChild,
[int]$InstallTimeoutMinutes, [int]$DetectionSettleSeconds, [int]$ResidueSettleSeconds)
$label = if ($WhatIf) {{ 'DryRun' }} elseif ($ElevatedChild) {{ 'Live elevated' }} elseif ($NoElevate) {{ 'Live user' }} else {{ 'Live prompting' }}
Add-Content -LiteralPath {trace} -Value $label
$names = if ($WhatIf) {{ @('Schema','Download','PreDetect') }} else {{
    @('Schema','Download','PreDetect','Install','PostInstallDetect','Uninstall','PostUninstallDetect','Residue')
}}
$phases = @($names | ForEach-Object {{ @{{ Name = $_; Status = 'Pass' }} }})
if ($WhatIf) {{ $phases += @{{ Name = 'Install'; Status = 'Skip' }} }}
$id = [IO.Path]::GetFileNameWithoutExtension($RecipePath[0])
$overall = 'Pass'
if ($id -eq 'present') {{ $phases[2] = @{{ Name = 'PreDetect'; Status = 'Blocked'; Detail = 'App is already installed' }}; $overall = 'Blocked' }}
$dir = Join-Path (Join-Path {artifacts} '_install-tests') ([DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fffffff'))
New-Item -ItemType Directory -Path $dir -Force | Out-Null
@{{ Results = @(@{{ Id = $id; Overall = $overall; Phases = $phases }}) }} |
    ConvertTo-Json -Depth 8 | Set-Content (Join-Path $dir 'summary.json')
"#)).unwrap();
        (dir, StudioApp::isolated_for_tests(ws))
    }

    fn drain(app: &mut StudioApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        while app.workflow.run.is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            app.poll_for_tests();
        }
        if app.workflow.run.is_some() {
            stop(app, true);
            panic!("Fake harness pipeline timed out: {}", app.workflow.message);
        }
    }

    #[test]
    fn fake_harness_runs_all_stages_and_continues_after_missing_and_failed_recipes() {
        let (dir, mut app) = fake_app();
        let mut batch = ystudio_core::intake::Batch::new(ystudio_core::intake::Document {
            path: dir.path().join("requests.txt"), format: "text".into(), records: Vec::new(),
        }, &[]);
        // Absolute id keeps this test's batch persistence entirely in its temporary folder.
        batch.id = dir.path().join("saved-batch").to_string_lossy().into_owned();
        assert!(batch.dir().starts_with(dir.path()));
        batch.items = vec![
            ystudio_core::intake::BatchItem { app: "Missing".into(), ..Default::default() },
            ystudio_core::intake::BatchItem { app: "Broken".into(), ..Default::default() },
            ystudio_core::intake::BatchItem { app: "App".into(), ..Default::default() },
        ];
        let batch_id = batch.id.clone();
        let batch_dir = batch.dir();
        app.batch.batch = Some(batch);
        let options = Options { consent: Consent::Confirmed, ..Options::default() };
        connect_in_process(&mut app);
        start(&mut app, vec![
            Target { id: "missing".into(), items: vec![(batch_id.clone(), 0)] },
            Target { id: "broken".into(), items: vec![(batch_id.clone(), 1)] },
            Target { id: "app".into(), items: vec![(batch_id, 2)] },
        ], options, false, false);
        drain(&mut app);
        let trace = std::fs::read_to_string(dir.path().join("trace.txt")).unwrap();
        assert_eq!(trace.lines().collect::<Vec<_>>(), ["Schema", "Schema", "Download", "Package", "DryRun", "Live elevated"]);
        assert!(Step::ALL.iter().all(|step| app.step_state("app", *step) == Some(StepState::Pass)));
        assert!(app.workflow.message.contains("Validated"));
        let saved = ystudio_core::intake::Batch::load(&batch_dir).unwrap();
        assert_eq!(saved.items[0].status, ystudio_core::intake::ItemStatus::Failed);
        assert_eq!(saved.items[1].status, ystudio_core::intake::ItemStatus::Failed);
        assert_eq!(saved.items[2].status, ystudio_core::intake::ItemStatus::Done);
        assert!(saved.items[2].checks[0].passed);
        // The passing live run is recorded against the recipe's contents.
        assert!(app.completion("app").is_complete());
        assert!(!app.completion("broken").is_complete());
        let recipe = dir.path().join(r"Recipes\Development\app.yaml");
        std::fs::write(&recipe, std::fs::read_to_string(&recipe).unwrap() + "notes: edited\n").unwrap();
        app.refresh_recipes();
        assert!(matches!(app.completion("app"), ystudio_core::validation::Completion::Changed(_)));
        assert!(saved.items[2].checks[0].summary_dir.as_ref().unwrap().is_dir());
        assert!(saved.report().contains("app: Validated"));
        assert!(saved.items[2].checks[0].note.contains("live test elevated"), "{}", saved.items[2].checks[0].note);
        assert!(matches!(app.elevation, crate::elevation::State::Off), "the session ends with the run");
    }

    fn connect_in_process(app: &mut StudioApp) {
        let session = ystudio_core::elevation::Session::in_process(None).unwrap();
        app.elevation = crate::elevation::State::Ready { session, since: std::time::Instant::now(), tests: 0 };
    }

    fn trace(dir: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_to_string(dir.path().join("trace.txt")).unwrap().lines().map(str::to_string).collect()
    }

    fn wait_for_uac_step(app: &mut StudioApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        while !app.workflow.message.contains("waiting for UAC") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            app.poll_for_tests();
        }
        assert!(app.workflow.message.contains("waiting for UAC"), "{}", app.workflow.message);
    }

    #[test]
    fn blocked_evidence_is_summarised_in_one_line_and_never_repaired() {
        let (_dir, mut app) = fake_app();
        start(&mut app, vec![Target { id: "present".into(), items: Vec::new() }], Options { live: false, ..Options::default() }, true, false);
        drain(&mut app);
        let message = &app.workflow.message;
        assert_eq!(
            message,
            "present: Blocked (environment or incomplete harness evidence): Install test (dry run): overall Blocked: PreDetect Blocked (App is already installed)"
        );
        assert!(!app.jobs.iter().any(|j| matches!(j.kind, JobKind::WorkflowRepair { .. })));
    }

    #[test]
    fn repair_copies_shipped_recipes_to_development_and_leaves_passing_ones_alone() {
        let (dir, mut app) = fake_app();
        let original = std::fs::read_to_string(dir.path().join(r"Recipes\Development\app.yaml")).unwrap().replace("id: app", "id: shipped");
        let shipped = dir.path().join(r"Recipes\shipped.yaml");
        std::fs::write(&shipped, &original).unwrap();
        app.refresh_recipes();
        let targets = vec![Target { id: "shipped".into(), items: Vec::new() }];
        copy_shipped_to_development(&mut app, &targets).unwrap();
        let copy = app.ws.as_ref().unwrap().development_recipe_path("shipped");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), original);
        // Already copied: nothing to do, and the copy is not overwritten.
        copy_shipped_to_development(&mut app, &targets).unwrap();

        connect_in_process(&mut app);
        start(&mut app, targets, Options { consent: Consent::Confirmed, ..Options::default() }, true, false);
        drain(&mut app);
        assert!(app.workflow.message.starts_with("shipped: Validated"), "{}", app.workflow.message);
        assert!(!app.jobs.iter().any(|j| matches!(j.kind, JobKind::WorkflowRepair { .. })));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&shipped).unwrap(), original);
    }

    #[test]
    fn user_scope_live_tests_run_unelevated_without_asking_for_elevation() {
        let (dir, mut app) = fake_app();
        let options = Options { consent: Consent::Confirmed, ..Options::default() };
        start(&mut app, vec![Target { id: "user".into(), items: Vec::new() }], options, false, false);
        drain(&mut app);
        assert_eq!(trace(&dir).last().map(String::as_str), Some("Live user"));
        assert_eq!(app.step_state("user", Step::Live), Some(StepState::Pass));
        assert!(matches!(app.elevation, crate::elevation::State::Off));
    }

    #[test]
    fn unavailable_elevation_blocks_system_recipes_without_repair_and_continues() {
        let (dir, mut app) = fake_app();
        app.elevation = crate::elevation::State::Unavailable("The UAC prompt was declined.".into());
        let options = Options { consent: Consent::Confirmed, ..Options::default() };
        start(&mut app, vec![
            Target { id: "app".into(), items: Vec::new() },
            Target { id: "user".into(), items: Vec::new() },
        ], options, true, false);
        drain(&mut app);
        let trace = trace(&dir);
        assert!(!trace.iter().any(|l| l == "Live elevated" || l == "Live prompting"), "{trace:?}");
        assert_eq!(trace.last().map(String::as_str), Some("Live user"));
        assert!(!app.jobs.iter().any(|j| matches!(j.kind, JobKind::WorkflowRepair { .. })));
        assert_eq!(app.step_state("app", Step::Live), None);
        assert_eq!(app.step_state("user", Step::Live), Some(StepState::Pass));
    }

    #[test]
    fn live_step_waits_for_the_uac_answer_then_runs_elevated() {
        let (dir, mut app) = fake_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.elevation = crate::elevation::State::Waiting { rx, since: std::time::Instant::now() };
        let options = Options { consent: Consent::Confirmed, ..Options::default() };
        start(&mut app, vec![Target { id: "app".into(), items: Vec::new() }], options, false, false);
        wait_for_uac_step(&mut app);
        assert_eq!(trace(&dir).last().map(String::as_str), Some("DryRun"));
        tx.send(ystudio_core::elevation::Session::in_process(None)).unwrap();
        drain(&mut app);
        assert_eq!(trace(&dir).last().map(String::as_str), Some("Live elevated"));
        assert!(app.workflow.message.contains("Validated"), "{}", app.workflow.message);
    }

    #[test]
    fn cancelling_while_waiting_for_uac_stops_the_run() {
        let (_dir, mut app) = fake_app();
        let (_tx, rx) = std::sync::mpsc::channel();
        app.elevation = crate::elevation::State::Waiting { rx, since: std::time::Instant::now() };
        let options = Options { consent: Consent::Confirmed, ..Options::default() };
        start(&mut app, vec![Target { id: "app".into(), items: Vec::new() }], options, false, false);
        wait_for_uac_step(&mut app);
        stop(&mut app, true);
        app.poll_for_tests();
        assert!(app.workflow.run.is_none());
        assert!(app.workflow.message.contains("Cancelled"), "{}", app.workflow.message);
        // The next frame notices nothing needs the session any more.
        app.poll_for_tests();
        assert!(matches!(app.elevation, crate::elevation::State::Off));
    }

    #[test]
    fn safe_pipeline_never_runs_live_and_unconfirmed_live_starts_nothing() {
        let (dir, mut app) = fake_app();
        let target = Target { id: "app".into(), items: Vec::new() };
        start(&mut app, vec![target.clone()], Options::default(), false, false);
        assert!(app.jobs.is_empty());
        assert!(app.status.error);
        start(&mut app, vec![target], Options { live: false, ..Options::default() }, false, false);
        drain(&mut app);
        let trace = std::fs::read_to_string(dir.path().join("trace.txt")).unwrap();
        assert_eq!(trace.lines().collect::<Vec<_>>(), ["Schema", "Download", "Package", "DryRun"]);
        assert_eq!(app.step_state("app", Step::Live), None);
        assert!(app.workflow.message.contains("live test still required"));
    }

    #[test]
    fn cancelling_a_harness_job_never_starts_a_repair_or_the_next_recipe() {
        let (_dir, mut app) = fake_app();
        start(&mut app, vec![
            Target { id: "app".into(), items: Vec::new() },
            Target { id: "broken".into(), items: Vec::new() },
        ], Options { live: false, ..Options::default() }, true, false);
        // Exercise the shared console's cancellation path, not just the workflow button.
        let id = app.workflow.last_job.unwrap();
        app.job_mut(id).unwrap().proc.cancel();
        drain(&mut app);
        assert_eq!(app.jobs.len(), 1);
        assert!(app.workflow.message.contains("Cancelled"));
        assert_eq!(app.step_state("app", Step::Download), None);
    }

    fn render(app: &mut StudioApp, name: &str, width: u32, height: u32) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, app.settings.dark_mode);
        let mut renderer = crate::soft::SoftRenderer::new();
        for frame in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width as f32, height as f32))),
                time: Some(frame as f64 * 0.5),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.frame(ui));
            renderer.set_textures(&output.textures_delta);
            let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
            assert!(!primitives.is_empty());
            renderer.paint(width as usize, height as usize, output.pixels_per_point, ctx.global_style().visuals.panel_fill, &primitives);
            renderer.free_textures(&output.textures_delta);
            output.textures_delta.clear();
        }
        assert_eq!(renderer.pixels().len(), (width * height) as usize);
        if let Some(dir) = std::env::var_os("YSTUDIO_TEST_SCREENSHOT_DIR") {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let bytes = renderer.pixels().iter().flat_map(|pixel| [
                (pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8,
            ]).collect();
            image::RgbImage::from_raw(width, height, bytes).unwrap().save(dir.join(format!("{name}.png"))).unwrap();
        }
    }

    #[test]
    fn batch_agent_and_consent_surfaces_render_at_normal_and_minimum_widths() {
        let (_dir, mut app) = fake_app();
        let mut batch = ystudio_core::intake::Batch::new(ystudio_core::intake::Document {
            path: PathBuf::from("requests.txt"), format: "text".into(), records: Vec::new(),
        }, &[]);
        batch.items = vec![ystudio_core::intake::BatchItem {
            app: "App".into(), source: "TEST-1".into(), include: true, existing_recipe: "app".into(), ..Default::default()
        }];
        app.batch.batch = Some(batch);
        render(&mut app, "batch-wide", 1400, 900);
        request(&mut app, Request::BatchRepair(vec![Target { id: "app".into(), items: Vec::new() }]));
        render(&mut app, "batch-repair-consent", 1400, 900);
        app.workflow.request = None;
        let targets = vec![Target { id: "app".into(), items: Vec::new() }];
        request(&mut app, Request::BatchTests(targets));
        render(&mut app, "batch-consent-narrow", 900, 600);
        assert!(app.workflow.request.is_some());
        assert!(app.jobs.is_empty());
        app.workflow.request = None;
        app.open_recipe_by_id("app");
        app.view = crate::app::View::Agent;
        render(&mut app, "agent-narrow", 900, 600);
        app.view = crate::app::View::Batch;
        app.settings.dark_mode = false;
        render(&mut app, "batch-light", 1400, 900);
        // The elevated session's states, kept alive by a pending consent request.
        app.settings.dark_mode = true;
        app.workflow.request = Some(Request::BatchTests(vec![Target { id: "app".into(), items: Vec::new() }]));
        connect_in_process(&mut app);
        render(&mut app, "elevation-ready", 1400, 900);
        app.elevation = crate::elevation::State::Unavailable("The UAC prompt was declined.".into());
        render(&mut app, "elevation-unavailable-narrow", 900, 600);
        let (_tx, rx) = std::sync::mpsc::channel();
        app.elevation = crate::elevation::State::Waiting { rx, since: std::time::Instant::now() };
        app.workflow.request = None;
        app.workflow.run = Some(Run {
            root: app.ws.as_ref().unwrap().root.clone(), queue: VecDeque::new(), current: Target { id: "app".into(), items: Vec::new() },
            options: Options::default(), repair: false, resume_batch: false, attempt: 1, step: 5,
            stopped: false, cancelled: false, summary_dir: None, warned: false, elevated: None, waiting_elevation: true,
        });
        app.workflow.message = "app: waiting for UAC approval to run the system-context live test".into();
        render(&mut app, "elevation-waiting", 1400, 900);
        app.workflow.run = None;
    }

    #[test]
    fn settings_update_banner_current_icon_and_completion_render() {
        let (dir, mut app) = fake_app();
        let icon = image::RgbaImage::from_fn(64, 64, |x, y| image::Rgba([40, 120 + (x * 2) as u8, 200 - (y * 2) as u8, 255]));
        icon.save(dir.path().join(r"Icons\app.png")).unwrap();
        let ws = app.ws.clone().unwrap();
        let mut ledger = ystudio_core::validation::Ledger::load(&ws.layout.harness_artifacts);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        assert!(ledger.record("app", &ws.development_recipe_path("app"), later, results::Verdict::Validated, "2026-11-09 10:00:00", None).unwrap());
        ledger.save().unwrap();
        app.refresh_runs();
        assert!(app.completion("app").is_complete());

        app.update.status = crate::views::update::Status::Available(ystudio_core::update::Release {
            version: "9.9.9".into(), notes: "Big release".into(), html_url: String::new(),
            asset_name: "x.msi".into(), asset_url: String::new(), size: 1, sha256: None,
        });
        app.view = crate::app::View::Recipes;
        render(&mut app, "recipes-complete-update-banner", 1400, 900);
        app.view = crate::app::View::Settings;
        render(&mut app, "settings", 1400, 1100);
        render(&mut app, "settings-narrow", 900, 600);
        crate::views::icons::prepare_for_recipe(&mut app, "app");
        app.icons.auto_search = false;
        app.view = crate::app::View::Icons;
        render(&mut app, "icons-current", 1400, 900);
        std::fs::remove_file(dir.path().join(r"Icons\app.png")).unwrap();
        render(&mut app, "icons-current-missing", 1400, 900);
    }

    #[test]
    fn launcher_recipe_tabs_and_studio_made_requests_render() {
        let (dir, mut app) = fake_app();
        std::fs::create_dir_all(dir.path().join(r".claude\skills\yardstick-recipes")).unwrap();
        std::fs::write(dir.path().join(r".claude\skills\yardstick-recipes\SKILL.md"), "# skill").unwrap();
        app.refresh_recipes();
        app.view = crate::app::View::Home;
        render(&mut app, "home-launcher", 1400, 1000);
        render(&mut app, "home-launcher-narrow", 900, 700);

        // Every page of the open recipe shares its header and tabs.
        assert!(app.open_recipe_by_id("app"));
        for (view, _, name) in crate::views::RECIPE_TABS {
            crate::views::open_recipe_tab(&mut app, view);
            app.icons.auto_search = false;
            render(&mut app, &format!("recipe-tab-{}", name.to_lowercase()), 1400, 900);
        }
        assert_eq!(app.icons.recipe_id.as_deref(), Some("app"));
        assert_eq!(app.agent.target_id, "app");

        app.view = crate::app::View::Recipes;
        app.library.selecting = true;
        app.library.picked.extend(["app".to_string(), "user".to_string()]);
        render(&mut app, "library-update-several", 1400, 900);
        let items = crate::views::batch::update_items(&app, &["app".into(), "user".into()], "vendor moved");
        assert_eq!(items[0].app, "App");
        assert!(items.iter().all(|i| i.request == ystudio_core::intake::RequestKind::Update && i.notes == "vendor moved"));
        assert_eq!(items[1].existing_recipe, "user");

        app.library.selecting = false;
        app.library.new_open = true;
        app.library.new_display = "Obsidian".into();
        app.library.new_id = "obsidian".into();
        render(&mut app, "new-recipe-dialog", 1400, 900);
        app.library.new_open = false;
        app.batch.paste = Some("Notepad++\n7-Zip 24.08".into());
        render(&mut app, "paste-requests", 1400, 900);
        app.batch.paste = None;

        // A new recipe is a one-item request that goes straight to authoring.
        let mut batch = ystudio_core::intake::Batch::from_items("new Obsidian", vec![ystudio_core::intake::BatchItem {
            app: "Obsidian".into(), suggested_id: "obsidian".into(), ..Default::default()
        }]);
        batch.id = dir.path().join("made-batch").to_string_lossy().into_owned();
        assert!(crate::views::batch::open_made(&mut app, batch));
        assert_eq!(app.view, crate::app::View::Batch);
        request(&mut app, Request::BatchAuthor(vec![0]));
        assert!(app.workflow.request.is_some() && app.jobs.is_empty());
        render(&mut app, "requests-new-recipe-consent", 1400, 900);
        app.workflow.request = None;
        render(&mut app, "requests-new-recipe", 1400, 900);
    }

    #[test]
    fn publish_pushes_a_branch_and_offers_the_compare_page() {
        let Ok(git_exe) = ystudio_core::git::git_exe() else { return };
        let (dir, mut app) = fake_app();
        let root = dir.path().to_path_buf();
        let origin = tempfile::tempdir().unwrap();
        let g = |cwd: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new(&git_exe).args(args).current_dir(cwd).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        g(origin.path(), &["init", "--bare", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), "/Recipes/Development/*\n/Icons/*\n/Artifacts/\n").unwrap();
        g(&root, &["init", "-b", "main"]);
        g(&root, &["config", "user.name", "Studio Test"]);
        g(&root, &["config", "user.email", "studio@example.com"]);
        g(&root, &["add", "-A"]);
        g(&root, &["commit", "-m", "base"]);
        g(&root, &["remote", "add", "origin", &origin.path().to_string_lossy()]);
        g(&root, &["push", "-u", "origin", "main"]);
        g(&root, &["remote", "set-head", "origin", "main"]);

        app.view = crate::app::View::Promote;
        render(&mut app, "publish-local-remote", 1400, 900);
        // The bare remote has no GitHub address; point the view at one.
        app.promote.repo = Some((root.clone(), Ok(ystudio_core::promote::Repo {
            remote_url: origin.path().to_string_lossy().into_owned(), web_url: "https://github.com/o/r".into(), base: "main".into(),
        }), None));
        app.promote.selected.insert("app".into());
        render(&mut app, "publish", 1400, 900);
        app.promote.modal = true;
        render(&mut app, "publish-confirm", 1400, 900);
        app.promote.modal = false;

        crate::views::promote::start(&mut app, &["app".to_string()]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while app.promote.running() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
            crate::views::promote::poll(&mut app);
        }
        assert_eq!(app.promote.prompts.len(), 1, "{:?}", app.jobs.last().map(|j| j.proc.text()));
        assert_eq!(app.promote.prompts[0].1.url(), "https://github.com/o/r/compare/main...studio/app?expand=1");
        assert!(app.promote.selected.is_empty());
        let shipped = std::process::Command::new(&git_exe).args(["show", "studio/app:Recipes/app.yaml"]).current_dir(origin.path()).output().unwrap();
        assert!(String::from_utf8_lossy(&shipped.stdout).contains("id: app"));
        render(&mut app, "publish-browser-prompt", 1400, 900);
    }

    /// Runs from a recipe's Agent tab are tracked as requests and re-run as asked.
    #[test]
    fn agent_tab_runs_become_requests_that_repeat_the_task() {
        let (dir, mut app) = fake_app();
        assert!(app.open_recipe_by_id("app"));
        crate::views::open_recipe_tab(&mut app, crate::app::View::Agent);
        let fix = AgentTask::Fix { id: "app".into(), evidence: "MSI 1603 in the install log".into(), notes: "x64 only".into() };
        let review = AgentTask::Review { id: "app".into() };
        let tracked = crate::views::agent::tracking_request(&app, &fix).unwrap();
        assert_eq!(crate::views::home::request_label(&tracked), "fix app");
        assert_eq!(tracked.items[0].app, "App");
        assert_eq!(crate::views::batch::task_for(&app, &tracked, 0), Some(fix.clone()));

        let mut items = tracked.items.clone();
        items.extend(crate::views::agent::tracking_request(&app, &review).unwrap().items);
        let mut batch = ystudio_core::intake::Batch::from_items("agent tab runs", items);
        batch.id = dir.path().join("agent-tab-runs").to_string_lossy().into_owned();
        let batch_id = batch.id.clone();
        assert!(crate::views::batch::open_made(&mut app, batch));
        assert_eq!(crate::views::batch::task_for(&app, app.batch.batch.as_ref().unwrap(), 1), Some(review));
        // Both finish without checks: the fix wrote the recipe, the review only read it.
        app.agent.reported = vec!["app".into()];
        crate::views::batch::agent_finished(&mut app, &batch_id, 0, true, false);
        crate::views::batch::agent_finished(&mut app, &batch_id, 1, true, false);
        let saved = ystudio_core::intake::Batch::load(std::path::Path::new(&batch_id)).unwrap();
        assert!(saved.items.iter().all(|i| i.status == ystudio_core::intake::ItemStatus::Done));
        assert_eq!(saved.items[1].action, ystudio_core::intake::ItemAction::Review);
        app.batch.selected = Some(0);
        render(&mut app, "requests-agent-tab-runs", 1400, 900);
    }

    /// The wizard against a stand-in Install-Yardstick.ps1 that follows the
    /// installer's -Status / -ResultPath contract.
    #[test]
    fn setup_wizard_reads_status_and_runs_development_only_setup() {
        if ystudio_core::setup::pwsh7(None).is_none() {
            return;
        }
        let (dir, mut app) = fake_app();
        let root = dir.path();
        std::fs::write(root.join("status.fake.json"), r#"{
          "schemaVersion": 1, "powerShellVersion": "7.6.9", "powershellYaml": { "installed": true, "version": "0.4.12" },
          "optionalModules": [
            { "name": "Selenium", "purpose": "recipes that drive a browser", "usedByRecipes": true, "minimumVersion": "4.0.0-preview3", "installedVersion": null, "isCurrent": false },
            { "name": "TUN.CredentialManager", "purpose": "recipes that sign in to a vendor portal", "usedByRecipes": true, "minimumVersion": null, "installedVersion": "3.0.2", "isCurrent": true },
            { "name": "Pester", "purpose": "running the unit tests in Tests\\Unit", "usedByRecipes": false, "minimumVersion": "5.0.0", "installedVersion": null, "isCurrent": false }
          ],
          "executionPolicy": { "policy": "Restricted", "scope": "CurrentUser", "groupPolicy": false, "allowsScripts": false },
          "preferences": { "exists": false, "path": "x", "readError": null, "developmentOnly": false, "sharedIcons": "", "sharedRecipeOverrides": "", "syncSharedContent": false },
          "unwritablePaths": { "count": 2, "examples": ["C:\\x\\Tools"] },
          "intuneWinAppUtil": { "present": false, "path": "x" }
        }"#).unwrap();
        std::fs::write(root.join("result.fake.json"), r#"{
          "schemaVersion": 1, "succeeded": true, "developmentOnly": true,
          "steps": [
            { "step": "Prerequisites", "result": "Done", "level": "OK", "detail": "" },
            { "step": "Preferences", "result": "Done", "level": "OK", "detail": "" },
            { "step": "AppRegistration", "result": "Skipped", "level": "Skip", "detail": "development-only instance" },
            { "step": "Verify", "result": "Problems found", "level": "Warn", "detail": "" }
          ],
          "messages": [ { "step": "Verify", "level": "Warn", "message": "225 recipe icon(s) missing" } ]
        }"#).unwrap();
        let trace = harness::ps_quote(&root.join("setup-trace.txt").to_string_lossy());
        std::fs::write(root.join("Install-Yardstick.ps1"), format!(r#"
param([switch]$Status, [string]$ResultPath, [switch]$DevelopmentOnly, [switch]$NonInteractive,
      [string]$SharedIcons, [string]$SharedRecipeOverrides, [string[]]$InstallOptionalModule, [switch]$SetExecutionPolicy)
if ($Status) {{ Copy-Item (Join-Path $PSScriptRoot 'status.fake.json') $ResultPath; exit 0 }}
Add-Content -LiteralPath {trace} -Value "dev=$DevelopmentOnly ni=$NonInteractive modules=$($InstallOptionalModule -join '+') policy=$SetExecutionPolicy icons=[$SharedIcons]"
New-Item -ItemType Directory -Force (Join-Path $PSScriptRoot 'Local') | Out-Null
Set-Content (Join-Path $PSScriptRoot 'Local\preferences.yaml') 'developmentOnly: true'
Copy-Item (Join-Path $PSScriptRoot 'result.fake.json') $ResultPath
"#)).unwrap();
        app.reload_workspace();
        assert_eq!(app.ws.as_ref().unwrap().kind, ystudio_core::workspace::WorkspaceKind::Unconfigured);

        crate::views::setup::open(&mut app, crate::views::setup::Step::Workspace);
        render(&mut app, "setup-workspace", 1400, 900);
        app.setup.step = crate::views::setup::Step::Tools;
        render(&mut app, "setup-tools", 1400, 900);

        // Entering the Yardstick step reads the status in the background.
        app.setup.step = crate::views::setup::Step::Yardstick;
        render(&mut app, "setup-checking", 1400, 900);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while app.setup.status_for_tests().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        assert!(app.setup.status_for_tests().is_some(), "{:?}", app.jobs.last().map(|j| j.proc.text()));
        // Pre-filled: the missing recipe module, not the unit-test one; fix the policy.
        assert_eq!(app.setup.answers_for_tests(), (vec!["Selenium".to_string()], true));
        render(&mut app, "setup-yardstick", 1400, 1000);

        crate::views::setup::run_setup(&mut app);
        while app.setup.result_for_tests().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            app.poll_for_tests();
        }
        let trace = std::fs::read_to_string(root.join("setup-trace.txt")).unwrap();
        assert_eq!(trace.trim(), "dev=True ni=True modules=Selenium policy=True icons=[]");
        assert!(matches!(app.setup.result_for_tests(), Some(Ok(r)) if r.succeeded));
        assert_eq!(app.ws.as_ref().unwrap().kind, ystudio_core::workspace::WorkspaceKind::Development);
        render(&mut app, "setup-finished", 1400, 1000);
        app.setup.step = crate::views::setup::Step::Done;
        render(&mut app, "setup-done", 1400, 900);
    }
}
