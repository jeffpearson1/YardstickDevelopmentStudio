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
}

#[derive(Default)]
pub struct State {
    pub options: Options,
    pub request: Option<Request>,
    ack: bool,
    run: Option<Run>,
    pub last_job: Option<u64>,
    pub message: String,
    advance_pending: bool,
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
    app.workflow.request = Some(request);
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
    let (labels, repairs) = match request {
        Request::Agent(task) => (vec![task.title()], true),
        Request::BatchAuthor(indices) => (
            indices.iter().filter_map(|&i| app.batch.batch.as_ref()?.items.get(i))
                .map(|it| format!("{} ({})", it.app, it.recipe_id())).collect(), true),
        Request::BatchTests(targets) => (targets.iter().map(|t| t.id.clone()).collect(), false),
    };
    let mut go = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("workflow-consent")).show(ctx, |ui| {
        ui.set_max_width(580.0);
        ui.heading(if repairs { "Create, test and repair recipes" } else { "Test selected batch recipes" });
        egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
            for label in &labels { ui.label(label); }
        });
        ui.label("Studio runs lint, schema, download, package and dry-run checks sequentially.");
        if repairs {
            ui.label(format!("Up to {} test attempts per recipe. Recipe failures go back to the agent; environmental blockers stop repair.", app.workflow.options.attempts));
        } else {
            ui.label("No recipes are changed. A failure stops that recipe, then testing continues with the next.");
        }
        if app.workflow.options.live {
            let warning = if repairs {
                "Live checks INSTALL and UNINSTALL real software on THIS machine, including repeated runs after repairs. System installs may show UAC prompts."
            } else {
                "Live checks INSTALL and UNINSTALL real software on THIS machine. System installs may show UAC prompts."
            };
            ui.label(RichText::new(warning).color(theme::red()));
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
        match request {
            Request::Agent(task) => crate::views::agent::AgentState::start(app, task, Some(options)),
            Request::BatchAuthor(indices) => crate::views::batch::start_authoring(app, indices, options),
            Request::BatchTests(targets) => start(app, targets, options, false, false),
        }
    }
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
        stopped: false, cancelled: false, summary_dir: None, warned: false,
    });
    app.workflow.advance_pending = false;
    advance(app);
}

pub fn poll(app: &mut StudioApp) {
    if std::mem::take(&mut app.workflow.advance_pending) {
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
        let Some(ws) = &app.ws else { return };
        let path = ws.development_recipe_path(&id);
        // Authoring and repairs always target the Development copy; testing may use a shipped recipe.
        let path = if path.is_file() { path } else if !run.repair {
            app.recipes.iter().find(|r| r.id == id).map(|r| r.path.clone()).unwrap_or(path)
        } else { path };
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
                fail(app, format!("{diagnostics:?}"), false);
                return;
            }
            app.workflow.run.as_mut().unwrap().step += 1;
            continue;
        }
        let action = match step {
            Step::Schema => HarnessAction::Schema { recipe: path },
            Step::Download => HarnessAction::Lab { recipe: path, stage: LabStage::Download },
            Step::Package => HarnessAction::Lab { recipe: path, stage: LabStage::Package },
            Step::DryRun => HarnessAction::InstallTest { recipes: vec![path], options: InstallTestOptions::dry_run() },
            Step::Live => HarnessAction::InstallTest { recipes: vec![path], options: InstallTestOptions::live() },
            Step::Lint => unreachable!(),
        };
        let consent = if step == Step::Live { run.options.consent } else { Consent::None };
        let spec = match harness::build(ws, &action, consent, &app.pwsh()) {
            Ok(spec) => spec,
            Err(e) => { finish(app, false, format!("Could not build harness command: {e:#}")); return; }
        };
        let job = app.start_job(spec, JobKind::WorkflowHarness { step, recipe_id: id.clone() });
        app.workflow.last_job = job;
        app.bench.last_job = job;
        app.agent.job = job;
        if app.workflow.run.as_ref().is_some_and(|r| !r.current.items.is_empty()) {
            app.batch.run_job = job;
        }
        app.console_open = true;
        if job.is_some() {
            app.bench.steps.insert((id, step), StepState::Running);
        } else {
            finish(app, false, "Could not start the harness; check the console.".into());
        }
        return;
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
        "already installed", "already detected"].iter().any(|s| text.contains(s))
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
    app.bench.steps.insert((id.into(), step), if passed {
        if warning { StepState::Warn } else { StepState::Pass }
    } else { StepState::Fail });
    if let Some(run) = run {
        app.workflow.run.as_mut().unwrap().summary_dir = Some(run.dir.clone());
        app.bench.last_run = Some(run);
        app.refresh_runs();
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
        fail(app, evidence, blocker);
    }
}

fn fail(app: &mut StudioApp, evidence: String, blocker: bool) {
    let Some(run) = &mut app.workflow.run else { return };
    if !run.can_repair(blocker) {
        let prefix = if blocker { "Blocked (environment or incomplete harness evidence)" } else { "Checks failed" };
        finish(app, false, format!("{prefix}: {}", tail(&evidence, 1600)));
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

fn tail(text: &str, limit: usize) -> String {
    text.chars().rev().take(limit).collect::<String>().chars().rev().collect()
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
        finish(app, false, format!("Repair agent stopped: {}", tail(text, 1600)));
    } else {
        advance(app);
    }
}

fn finish(app: &mut StudioApp, passed: bool, note: String) {
    let Some(mut run) = app.workflow.run.take() else { return };
    app.workflow.message = format!("{}: {note}", run.current.id);
    let check = BatchCheck {
        recipe: run.current.id.clone(), passed, note: format!("{note} ({} test attempt(s))", run.attempt),
        timestamp: ystudio_core::intake::timestamp(), summary_dir: run.summary_dir.clone(),
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
                    timestamp: ystudio_core::intake::timestamp(), summary_dir: None,
                });
            }
        }
    }
}

pub fn stop(app: &mut StudioApp, cancel: bool) {
    if let Some(run) = &mut app.workflow.run {
        run.stopped = true;
        run.cancelled |= cancel;
    }
    if cancel && let Some(id) = app.workflow.last_job && let Some(job) = app.job_mut(id) {
        job.proc.cancel();
    }
    crate::views::batch::stop_queue(app);
}

pub fn progress(app: &mut StudioApp, ui: &mut Ui) {
    if app.workflow.run.is_none() {
        if !app.workflow.message.is_empty() { ui.label(&app.workflow.message); }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spinner();
        ui.label(&app.workflow.message);
        if ui.button("Stop after this recipe").clicked() {
            stop(app, false);
        }
        if ui.button("Cancel now").clicked() {
            stop(app, true);
        }
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
            stopped: false, cancelled: false, summary_dir: None, warned: false,
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
        std::fs::write(root.join(r"Recipes\Development\broken.yaml"), text).unwrap();
        let ws = ystudio_core::workspace::Workspace::open(root, &[], &[]).unwrap();
        std::fs::create_dir_all(&ws.layout.harness).unwrap();
        let trace = harness::ps_quote(&root.join("trace.txt").to_string_lossy());
        std::fs::write(ws.layout.harness.join("Test-RecipeSchema.ps1"),
            format!("param([string]$RecipePath)\nAdd-Content -LiteralPath {trace} -Value Schema\nif ([IO.Path]::GetFileNameWithoutExtension($RecipePath) -eq 'broken') {{ throw 'Fake schema failure' }}\n")).unwrap();
        std::fs::write(ws.layout.harness.join("Invoke-RecipeLab.ps1"),
            format!("param([string]$RecipePath, [string]$Stage)\nAdd-Content -LiteralPath {trace} -Value $Stage\n")).unwrap();
        let artifacts = harness::ps_quote(&ws.layout.harness_artifacts.to_string_lossy());
        std::fs::write(ws.layout.harness.join("Invoke-RecipeInstallTest.ps1"), format!(r#"
param([string[]]$RecipePath, [switch]$WhatIf, [switch]$NoElevate, [switch]$DiscardInstaller,
[int]$InstallTimeoutMinutes, [int]$DetectionSettleSeconds, [int]$ResidueSettleSeconds)
$label = if ($WhatIf) {{ 'DryRun' }} else {{ 'Live' }}
Add-Content -LiteralPath {trace} -Value $label
$names = if ($WhatIf) {{ @('Schema','Download','PreDetect') }} else {{
    @('Schema','Download','PreDetect','Install','PostInstallDetect','Uninstall','PostUninstallDetect','Residue')
}}
$phases = @($names | ForEach-Object {{ @{{ Name = $_; Status = 'Pass' }} }})
if ($WhatIf) {{ $phases += @{{ Name = 'Install'; Status = 'Skip' }} }}
$dir = Join-Path (Join-Path {artifacts} '_install-tests') ([DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fffffff'))
New-Item -ItemType Directory -Path $dir -Force | Out-Null
@{{ Results = @(@{{ Id = 'app'; Overall = 'Pass'; Phases = $phases }}) }} |
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
        start(&mut app, vec![
            Target { id: "missing".into(), items: vec![(batch_id.clone(), 0)] },
            Target { id: "broken".into(), items: vec![(batch_id.clone(), 1)] },
            Target { id: "app".into(), items: vec![(batch_id, 2)] },
        ], options, false, false);
        drain(&mut app);
        let trace = std::fs::read_to_string(dir.path().join("trace.txt")).unwrap();
        assert_eq!(trace.lines().collect::<Vec<_>>(), ["Schema", "Schema", "Download", "Package", "DryRun", "Live"]);
        assert!(Step::ALL.iter().all(|step| app.step_state("app", *step) == Some(StepState::Pass)));
        assert!(app.workflow.message.contains("Validated"));
        let saved = ystudio_core::intake::Batch::load(&batch_dir).unwrap();
        assert_eq!(saved.items[0].status, ystudio_core::intake::ItemStatus::Failed);
        assert_eq!(saved.items[1].status, ystudio_core::intake::ItemStatus::Failed);
        assert_eq!(saved.items[2].status, ystudio_core::intake::ItemStatus::Done);
        assert!(saved.items[2].checks[0].passed);
        assert!(saved.items[2].checks[0].summary_dir.as_ref().unwrap().is_dir());
        assert!(saved.report().contains("app: Validated"));
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
        let targets = vec![Target { id: "app".into(), items: Vec::new() }];
        request(&mut app, Request::BatchTests(targets));
        render(&mut app, "batch-consent-narrow", 900, 600);
        assert!(app.workflow.request.is_some());
        assert!(app.jobs.is_empty());
        app.workflow.request = None;
        app.view = crate::app::View::Agent;
        app.agent.app_name = "App".into();
        render(&mut app, "agent-narrow", 900, 600);
        app.view = crate::app::View::Batch;
        app.settings.dark_mode = false;
        render(&mut app, "batch-light", 1400, 900);
    }
}
