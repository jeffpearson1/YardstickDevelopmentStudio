//! Application state, navigation, background jobs and the shared console.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkCache;
use ystudio_core::agent::{Change, Snapshot};
use ystudio_core::harness::{self, InstallTestOptions};
use ystudio_core::lint::{self, Diagnostic, LintContext};
use ystudio_core::process::{ProcessSpec, RunningProcess, Stream, Waker, fmt_duration};
use ystudio_core::recipe::RecipeDoc;
use ystudio_core::results::{self, InstallRun, Verdict};
use ystudio_core::settings::Settings;
use ystudio_core::workspace::{RecipeEntry, RecipeLocation, Workspace, WorkspaceKind};

use crate::theme::{self, FluentText as _};
use crate::views;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Home,
    Recipes,
    Designer,
    Editor,
    TestBench,
    Agent,
    Batch,
    Icons,
    Runs,
    Docs,
    Promote,
}

impl View {
    pub const NAV: [(View, &'static str, &'static str); 11] = [
        (View::Home, theme::icon::HOME, "Home"),
        (View::Recipes, theme::icon::LIBRARY, "Recipes"),
        (View::Designer, theme::icon::EDIT, "Designer"),
        (View::Editor, theme::icon::CODE, "YAML Editor"),
        (View::TestBench, theme::icon::CHECKLIST, "Test Bench"),
        (View::Agent, theme::icon::ROBOT, "Agent"),
        (View::Batch, theme::icon::INBOX, "Batch"),
        (View::Icons, theme::icon::PHOTO, "Icons"),
        (View::Runs, theme::icon::CHART, "Runs"),
        (View::Docs, theme::icon::BOOK, "Docs"),
        (View::Promote, theme::icon::UPLOAD, "Promote"),
    ];
}

/// Test Bench pipeline steps, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Step {
    Lint,
    Schema,
    Download,
    Package,
    DryRun,
    Live,
}

impl Step {
    pub const ALL: [Step; 6] = [Step::Lint, Step::Schema, Step::Download, Step::Package, Step::DryRun, Step::Live];

    pub fn title(self) -> &'static str {
        match self {
            Step::Lint => "Studio lint",
            Step::Schema => "Schema validation",
            Step::Download => "Lab: download",
            Step::Package => "Lab: package",
            Step::DryRun => "Install test (dry run)",
            Step::Live => "Install test (LIVE)",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Step::Lint => "Instant in-process checks ported from Test-RecipeSchema plus skill rules.",
            Step::Schema => "Test-RecipeSchema.ps1 - the authoritative validator.",
            Step::Download => "Invoke-RecipeLab.ps1 -Stage Download: runs the scripts and downloads the payload.",
            Step::Package => "Invoke-RecipeLab.ps1 -Stage Package: builds the .intunewin (needs IntuneWin32App).",
            Step::DryRun => "Invoke-RecipeInstallTest.ps1 -WhatIf -NoElevate -DiscardInstaller: everything except install/uninstall.",
            Step::Live => "Installs, detects, uninstalls and checks residue on THIS machine. Needs your consent.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Pass,
    Warn,
    Fail,
}

#[derive(Clone)]
pub enum JobKind {
    Harness { step: Step, recipe_id: String },
    /// `batch_item` is (batch id, item index) when a batch queued the run.
    Agent { before: Snapshot, batch_item: Option<(String, usize)> },
    /// An agent reading a request list into batch items.
    BatchExtract { batch_id: String, backend: String },
    Clone { dest: PathBuf },
    Publish { what_if: bool },
}

pub struct Job {
    pub id: u64,
    pub proc: RunningProcess,
    pub kind: JobKind,
    pub started_sys: SystemTime,
    handled: bool,
}

impl Job {
    pub fn is_agent(&self) -> bool {
        matches!(self.kind, JobKind::Agent { .. })
    }
}

/// The recipe being designed. The YAML text is the source of truth; the
/// Designer edits it through [`RecipeDoc`] and the Editor edits it directly.
pub struct OpenRecipe {
    pub path: PathBuf,
    pub id: String,
    pub location: RecipeLocation,
    pub text: String,
    pub saved: String,
    pub read_only: bool,
    /// Bumped on every change to `text`.
    pub rev: u64,
    pub lint_rev: u64,
    pub diagnostics: Vec<Diagnostic>,
    pub designer_bufs: HashMap<String, String>,
    pub bufs_rev: u64,
    pub accept_comment_loss: bool,
    pub section: usize,
    pub show_all_fields: bool,
    /// The parse of `text` at some revision; see [`OpenRecipe::take_parsed`].
    pub parsed: Option<ParsedRecipe>,
}

pub struct ParsedRecipe {
    pub rev: u64,
    pub doc: Result<RecipeDoc, String>,
    pub has_comments: bool,
}

impl OpenRecipe {
    /// Takes the parse of the current text, reusing the cached one when the
    /// text has not changed since. Callers put it back in `parsed`.
    pub fn take_parsed(&mut self) -> ParsedRecipe {
        match self.parsed.take() {
            Some(p) if p.rev == self.rev => p,
            _ => ParsedRecipe {
                rev: self.rev,
                doc: RecipeDoc::parse(&self.text).map_err(|e| format!("{e:#}")),
                has_comments: ystudio_core::recipe::has_comments(&self.text),
            },
        }
    }

    pub fn dirty(&self) -> bool {
        self.text != self.saved
    }

    pub fn changed(&mut self) {
        self.rev += 1;
    }
}

#[derive(Default)]
pub struct Status {
    pub text: String,
    pub error: bool,
    pub at: Option<Instant>,
}

/// Frame timing. Ctrl+Shift+P toggles a readout in the status bar; setting
/// `YSTUDIO_PERF_LOG=<file>` also appends one `view<TAB>ms` line per frame.
#[derive(Default)]
pub struct Perf {
    pub show: bool,
    /// Exponential moving average of the UI build time, in ms.
    pub avg_ms: f32,
    pub max_ms: f32,
    log: Option<std::io::BufWriter<std::fs::File>>,
    epoch: Option<Instant>,
    /// `YSTUDIO_PERF_CONTINUOUS=1` repaints continuously to measure throughput.
    continuous: bool,
    /// The paint backend in use, shown with the readout.
    pub renderer: String,
    /// Time the renderer spent rasterising the previous frame, if it reports it.
    pub paint_ms: Option<f32>,
}

impl Perf {
    fn new() -> Self {
        let log = std::env::var_os("YSTUDIO_PERF_LOG")
            .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok())
            .map(std::io::BufWriter::new);
        let continuous = std::env::var_os("YSTUDIO_PERF_CONTINUOUS").is_some();
        Self { log, continuous, ..Default::default() }
    }

    fn note(&mut self, text: &str) {
        if let Some(log) = &mut self.log {
            use std::io::Write;
            let _ = writeln!(log, "# {text}");
            let _ = log.flush();
        }
    }

    fn record(&mut self, view: View, ms: f32) {
        self.avg_ms = if self.avg_ms == 0.0 { ms } else { self.avg_ms * 0.9 + ms * 0.1 };
        self.max_ms = self.max_ms.max(ms);
        if let Some(log) = &mut self.log {
            use std::io::Write;
            let t = self.epoch.get_or_insert_with(Instant::now).elapsed().as_secs_f64() * 1000.0;
            let paint = self.paint_ms.unwrap_or(0.0);
            let _ = writeln!(log, "{view:?}\t{ms:.3}\t{t:.1}\t{paint:.3}");
            let _ = log.flush();
        }
    }
}

/// Workspace listings refreshed by [`StudioApp::refresh_recipes`].
#[derive(Default)]
pub struct WsCache {
    pub icons: Vec<String>,
    pub groups: Vec<String>,
    pub has_skill: bool,
    pub documents: Vec<(String, PathBuf)>,
}

pub enum Pending {
    /// Switching recipes would discard unsaved edits.
    OpenRecipe(PathBuf),
}

pub struct StudioApp {
    pub settings: Settings,
    pub ws: Option<Workspace>,
    pub ws_error: Option<String>,
    pub view: View,
    pub recipes: Vec<RecipeEntry>,
    pub ws_cache: WsCache,
    pub recipe: Option<OpenRecipe>,
    pub jobs: Vec<Job>,
    next_job: u64,
    pub console_job: Option<u64>,
    pub console_open: bool,
    pub waker: Waker,
    pub status: Status,
    pub pending: Option<Pending>,
    pub md_cache: CommonMarkCache,
    /// Latest known verdict per recipe id, from the newest install-test runs.
    pub verdicts: BTreeMap<String, (Verdict, String)>,
    pub runs: Vec<InstallRun>,
    pub perf: Perf,

    pub home: views::home::HomeState,
    pub library: views::recipes::LibraryState,
    pub bench: BenchState,
    pub agent: views::agent::AgentState,
    pub batch: views::batch::BatchState,
    pub icons: views::icons::IconsState,
    pub runs_ui: views::runs::RunsState,
    pub docs: views::docs::DocsState,
    pub promote: views::promote::PromoteState,
}

pub struct BenchState {
    pub steps: HashMap<(String, Step), StepState>,
    pub queue: VecDeque<Step>,
    pub queue_recipe: Option<String>,
    pub live_options: InstallTestOptions,
    pub live_modal: bool,
    pub live_ack: bool,
    pub last_job: Option<u64>,
    pub last_run: Option<InstallRun>,
}

impl Default for BenchState {
    fn default() -> Self {
        Self {
            steps: HashMap::new(),
            queue: VecDeque::new(),
            queue_recipe: None,
            live_options: InstallTestOptions::live(),
            live_modal: false,
            live_ack: false,
            last_job: None,
            last_run: None,
        }
    }
}

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let renderer = match &cc.wgpu_render_state {
            Some(rs) => {
                let info = rs.adapter.get_info();
                format!("wgpu: {} ({:?}, {:?})", info.name, info.device_type, info.backend)
            }
            None => "wgpu".to_string(),
        };
        Self::with_context(&cc.egui_ctx, renderer)
    }

    /// `renderer` describes the paint backend, for the perf log and status bar.
    pub fn with_context(egui_ctx: &egui::Context, renderer: String) -> Self {
        let settings = Settings::load();
        let ctx = egui_ctx.clone();
        let waker: Waker = Arc::new(move || ctx.request_repaint());
        let mut app = Self {
            home: views::home::HomeState::new(&settings),
            settings,
            ws: None,
            ws_error: None,
            view: View::Home,
            recipes: Vec::new(),
            ws_cache: WsCache::default(),
            recipe: None,
            jobs: Vec::new(),
            next_job: 1,
            console_job: None,
            console_open: false,
            waker,
            status: Status::default(),
            pending: None,
            md_cache: CommonMarkCache::default(),
            verdicts: BTreeMap::new(),
            runs: Vec::new(),
            perf: Perf::new(),
            library: Default::default(),
            bench: Default::default(),
            agent: Default::default(),
            batch: Default::default(),
            icons: Default::default(),
            runs_ui: Default::default(),
            docs: Default::default(),
            promote: Default::default(),
        };
        theme::install_fonts(egui_ctx);
        app.apply_theme(egui_ctx);
        app.perf.note(&format!("renderer {renderer}"));
        app.perf.renderer = renderer;
        let start = app.settings.workspace.clone().or_else(|| {
            app.settings
                .primary_production()
                .and_then(|p| ystudio_core::workspace::discover_default(p))
        });
        if let Some(path) = start {
            app.open_workspace(&path);
        }
        app.handle_args(std::env::args().skip(1).collect());
        app
    }

    /// `yardstick-studio <request list>` (or `--batch <file>`) opens Batch mode;
    /// `--icons <app name>` opens the Icon finder and searches.
    fn handle_args(&mut self, args: Vec<String>) {
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--icons" => {
                    if let Some(name) = it.next() {
                        self.icons.prepare(&name, "", "", "");
                        self.view = View::Icons;
                    }
                }
                "--batch" => {
                    if let Some(f) = it.next() {
                        views::batch::open_file(self, Path::new(&f));
                        self.view = View::Batch;
                    }
                }
                f if Path::new(f).is_file() => {
                    views::batch::open_file(self, Path::new(f));
                    self.view = View::Batch;
                }
                _ => {}
            }
        }
    }

    pub fn apply_theme(&self, ctx: &egui::Context) {
        theme::apply(ctx, self.settings.dark_mode);
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.error(format!("Could not save settings: {e:#}"));
        }
    }

    pub fn pwsh(&self) -> PathBuf {
        harness::powershell_exe(self.settings.powershell_path.as_deref())
    }

    pub fn notify(&mut self, text: impl Into<String>) {
        self.status = Status { text: text.into(), error: false, at: Some(Instant::now()) };
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Status { text: text.into(), error: true, at: Some(Instant::now()) };
    }

    pub fn writable(&self) -> bool {
        self.ws.as_ref().is_some_and(Workspace::is_writable)
    }

    // ---- workspace -------------------------------------------------------

    pub fn open_workspace(&mut self, path: &Path) {
        match Workspace::open(path, &self.settings.production_paths, &self.settings.trusted_dev_paths) {
            Ok(ws) => {
                self.settings.remember_workspace(path.to_path_buf());
                self.save_settings();
                let label = ws.kind.label();
                self.ws = Some(ws);
                self.ws_error = None;
                self.recipe = None;
                self.bench = BenchState::default();
                self.docs = Default::default();
                self.refresh_recipes();
                self.refresh_runs();
                self.notify(format!("Opened {} - {label}", path.display()));
            }
            Err(e) => {
                self.ws_error = Some(format!("{e:#}"));
                self.error(format!("{e:#}"));
            }
        }
    }

    /// Re-classifies the open workspace after settings change.
    pub fn reload_workspace(&mut self) {
        if let Some(root) = self.ws.as_ref().map(|w| w.root.clone())
            && let Ok(ws) = Workspace::open(&root, &self.settings.production_paths, &self.settings.trusted_dev_paths)
        {
            let writable = ws.is_writable();
            self.ws = Some(ws);
            if let Some(r) = &mut self.recipe {
                r.read_only = !writable;
            }
        }
    }

    /// Rescans recipes and the other workspace listings the views display, so
    /// that no view has to touch the file system while drawing a frame.
    pub fn refresh_recipes(&mut self) {
        let Some(ws) = &self.ws else {
            self.recipes.clear();
            self.ws_cache = WsCache::default();
            return;
        };
        self.recipes = ws.list_recipes();
        self.ws_cache = WsCache {
            icons: ws.list_icons(),
            groups: ws.recipe_groups().into_keys().collect(),
            has_skill: ws.has_skill(),
            documents: ws.documents(),
        };
    }

    pub fn refresh_runs(&mut self) {
        let Some(ws) = &self.ws else { return };
        self.runs = results::list_install_runs(&ws.layout.harness_artifacts);
        self.verdicts.clear();
        for run in &self.runs {
            for r in run.summary.iter().flat_map(|s| &s.results) {
                self.verdicts.entry(r.id.clone()).or_insert((r.verdict(), run.timestamp.clone()));
            }
        }
    }

    // ---- recipes ---------------------------------------------------------

    pub fn request_open_recipe(&mut self, path: PathBuf) {
        if self.recipe.as_ref().is_some_and(|r| r.dirty() && r.path != path) {
            self.pending = Some(Pending::OpenRecipe(path));
        } else {
            self.open_recipe(&path);
        }
    }

    pub fn open_recipe(&mut self, path: &Path) {
        let text = match ystudio_core::read_text(path) {
            Ok(t) => t,
            Err(e) => return self.error(format!("Could not read {}: {e}", path.display())),
        };
        let id = harness::recipe_id(path);
        let entry = self.recipes.iter().find(|r| ystudio_core::same_path(&r.path, path));
        let location = entry.map(|r| r.location.clone()).unwrap_or(RecipeLocation::Development);
        // Prefer the library's spelling of the path so views can compare paths without canonicalizing.
        let path = entry.map(|r| r.path.clone()).unwrap_or_else(|| path.to_path_buf());
        self.recipe = Some(OpenRecipe {
            path,
            id,
            location,
            saved: text.clone(),
            text,
            read_only: !self.writable(),
            rev: 1,
            lint_rev: 0,
            diagnostics: Vec::new(),
            designer_bufs: HashMap::new(),
            bufs_rev: 0,
            accept_comment_loss: false,
            section: 0,
            show_all_fields: false,
            parsed: None,
        });
        self.relint();
    }

    pub fn open_recipe_by_id(&mut self, id: &str) -> bool {
        let found = self.ws.as_ref().and_then(|w| w.find_recipe(id));
        match found {
            Some(p) => {
                self.request_open_recipe(p);
                true
            }
            None => {
                self.error(format!("Recipe '{id}' was not found."));
                false
            }
        }
    }

    pub fn save_recipe(&mut self) -> bool {
        let Some(ws) = &self.ws else { return false };
        let Some(r) = &mut self.recipe else { return false };
        if let Err(e) = ws.ensure_writable("save recipes") {
            let msg = format!("{e:#}");
            self.error(msg);
            return false;
        }
        if r.read_only {
            self.error("This recipe is read-only.");
            return false;
        }
        if let Some(dir) = r.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&r.path, &r.text) {
            Ok(()) => {
                r.saved = r.text.clone();
                let msg = format!("Saved {}", r.path.display());
                self.refresh_recipes();
                self.notify(msg);
                true
            }
            Err(e) => {
                let msg = format!("Save failed: {e}");
                self.error(msg);
                false
            }
        }
    }

    /// Re-runs the in-process linter if the text changed.
    pub fn relint(&mut self) {
        let Some(ws) = &self.ws else { return };
        let Some(r) = &mut self.recipe else { return };
        if r.lint_rev == r.rev {
            return;
        }
        let doc = RecipeDoc::parse(&r.text).ok();
        let base = doc
            .as_ref()
            .and_then(|d| d.get_str("base"))
            .filter(|b| !b.trim().is_empty())
            .and_then(|b| {
                let b = b.trim();
                // `recipes` lists development copies first, matching `Workspace::find_recipe`.
                self.recipes.iter().find(|e| e.id.eq_ignore_ascii_case(b)).map(|e| e.path.clone())
            })
            .and_then(|p| RecipeDoc::load(&p).ok());
        let icon_exists = |name: &str| ws.icon_exists(name);
        let ctx = LintContext { file_id: Some(&r.id), icon_exists: Some(&icon_exists), base: base.as_ref(), shared_recipe: true };
        let mut diags = lint::lint_text(&r.text, &ctx);
        diags.sort_by_key(|d| d.severity);
        r.diagnostics = diags;
        r.lint_rev = r.rev;
    }

    // ---- jobs ------------------------------------------------------------

    pub fn start_job(&mut self, spec: ProcessSpec, kind: JobKind) -> Option<u64> {
        match RunningProcess::spawn(spec, Some(self.waker.clone())) {
            Ok(proc) => {
                let id = self.next_job;
                self.next_job += 1;
                self.jobs.push(Job { id, proc, kind, started_sys: SystemTime::now(), handled: false });
                self.console_job = Some(id);
                Some(id)
            }
            Err(e) => {
                self.error(format!("{e:#}"));
                None
            }
        }
    }

    pub fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn job_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    pub fn running_jobs(&self) -> usize {
        self.jobs.iter().filter(|j| j.proc.is_running()).count()
    }

    fn poll_jobs(&mut self) {
        for j in &mut self.jobs {
            j.proc.poll();
        }
        let finished: Vec<u64> = self.jobs.iter().filter(|j| !j.handled && !j.proc.is_running()).map(|j| j.id).collect();
        for id in finished {
            self.on_job_finished(id);
        }
    }

    fn on_job_finished(&mut self, id: u64) {
        let Some(idx) = self.jobs.iter().position(|j| j.id == id) else { return };
        self.jobs[idx].handled = true;
        let ok = self.jobs[idx].proc.succeeded();
        let started = self.jobs[idx].started_sys;
        let text = self.jobs[idx].proc.text();
        let label = self.jobs[idx].proc.spec.label.clone();
        let kind = self.jobs[idx].kind.clone();
        match kind {
            JobKind::Harness { step, recipe_id } => {
                let mut state = if ok { StepState::Pass } else { StepState::Fail };
                if matches!(step, Step::DryRun | Step::Live) {
                    let run = self.ws.as_ref().and_then(|w| results::newest_run_since(&w.layout.harness_artifacts, started));
                    if let Some(result) = run.as_ref().and_then(|r| r.summary.as_ref()).and_then(|s| s.results.iter().find(|r| r.id == recipe_id)) {
                        state = match result.verdict() {
                            Verdict::Validated | Verdict::DryRunPassed => StepState::Pass,
                            Verdict::ValidatedWithWarnings => StepState::Warn,
                            Verdict::Failed => StepState::Fail,
                            Verdict::Incomplete => {
                                if ok { StepState::Warn } else { StepState::Fail }
                            }
                        };
                    }
                    self.bench.last_run = run;
                    self.refresh_runs();
                }
                self.bench.steps.insert((recipe_id.clone(), step), state);
                if state == StepState::Fail {
                    self.bench.queue.clear();
                    self.error(format!("{} failed for {recipe_id}.", step.title()));
                } else {
                    self.notify(format!("{} finished for {recipe_id}.", step.title()));
                    self.advance_queue();
                }
            }
            JobKind::BatchExtract { batch_id, backend } => {
                views::batch::extract_finished(self, &batch_id, ok, &backend);
            }
            JobKind::Agent { before, batch_item } => {
                if let Some(ws) = &self.ws {
                    let after = Snapshot::take(ws);
                    let mut changes = before.diff(&after);
                    changes.sort_by(|a, b| a.path().cmp(b.path()));
                    self.agent.changes = changes;
                }
                self.agent.reported = ystudio_core::agent::reported_recipes(&text);
                self.refresh_recipes();
                // Reload the open recipe if the agent changed it and there are no local edits.
                if let Some(r) = &self.recipe {
                    let touched = self.agent.changes.iter().any(|c| ystudio_core::same_path(c.path(), &r.path));
                    if touched && !r.dirty() {
                        let p = r.path.clone();
                        self.open_recipe(&p);
                    }
                }
                let n = self.agent.changes.iter().filter(|c| !matches!(c, Change::Removed(_))).count();
                if ok {
                    self.notify(format!("Agent finished: {n} file(s) changed."));
                } else {
                    self.error(format!("Agent exited with an error ({n} file(s) changed)."));
                }
                if let Some((batch_id, idx)) = batch_item {
                    views::batch::agent_finished(self, &batch_id, idx, ok);
                }
            }
            JobKind::Clone { dest } => {
                if ok {
                    let mut msg = format!("Cloned into {}.", dest.display());
                    if let Some(prod) = self.settings.primary_production().cloned() {
                        let from = prod.join("Icons");
                        if from.is_dir() {
                            match harness::copy_missing_icons(&from, &dest.join("Icons")) {
                                Ok(n) => msg += &format!(" Copied {n} icon(s) from production."),
                                Err(e) => msg += &format!(" Icon copy failed: {e}."),
                            }
                        }
                    }
                    self.open_workspace(&dest);
                    self.notify(msg + " Next: run Install-Yardstick -DevelopmentOnly.");
                } else {
                    self.error("git clone failed - see the console.");
                }
            }
            JobKind::Publish { what_if } => {
                if what_if {
                    self.promote.preview_finished(id, ok);
                }
                if ok {
                    self.notify(if what_if { "Publish preview finished - review the console." } else { "Published to production." });
                } else {
                    self.error(format!("{label} failed - see the console."));
                }
            }
        }
    }

    // ---- test bench ------------------------------------------------------

    pub fn step_state(&self, recipe_id: &str, step: Step) -> Option<StepState> {
        self.bench.steps.get(&(recipe_id.to_string(), step)).copied()
    }

    /// Queues steps for the open recipe and starts the first one.
    pub fn run_steps(&mut self, steps: &[Step]) {
        let Some(r) = &self.recipe else { return };
        self.bench.queue = steps.iter().copied().collect();
        self.bench.queue_recipe = Some(r.id.clone());
        self.advance_queue();
    }

    fn advance_queue(&mut self) {
        let Some(step) = self.bench.queue.pop_front() else { return };
        let same = self.recipe.as_ref().map(|r| &r.id) == self.bench.queue_recipe.as_ref();
        if !same {
            self.bench.queue.clear();
            return;
        }
        if !self.run_step(step) {
            self.bench.queue.clear();
        }
    }

    /// Runs one step against the open recipe. Returns false if it could not start.
    pub fn run_step(&mut self, step: Step) -> bool {
        let Some(r) = &self.recipe else { return false };
        let id = r.id.clone();
        if step == Step::Lint {
            self.relint();
            let diags = &self.recipe.as_ref().unwrap().diagnostics;
            let errors = lint::count(diags, lint::Severity::Error);
            let warns = lint::count(diags, lint::Severity::Warning);
            let state = if errors > 0 { StepState::Fail } else if warns > 0 { StepState::Warn } else { StepState::Pass };
            self.bench.steps.insert((id.clone(), step), state);
            if state == StepState::Fail {
                self.bench.queue.clear();
                self.error(format!("Lint found {errors} error(s). Fix them before running the harness."));
                return false;
            }
            self.advance_queue();
            return true;
        }
        if self.recipe.as_ref().is_some_and(OpenRecipe::dirty) && !self.save_recipe() {
            return false;
        }
        let Some(ws) = &self.ws else { return false };
        let path = self.recipe.as_ref().unwrap().path.clone();
        let action = match step {
            Step::Schema => harness::HarnessAction::Schema { recipe: path },
            Step::Download => harness::HarnessAction::Lab { recipe: path, stage: harness::LabStage::Download },
            Step::Package => harness::HarnessAction::Lab { recipe: path, stage: harness::LabStage::Package },
            Step::DryRun => harness::HarnessAction::InstallTest { recipes: vec![path], options: InstallTestOptions::dry_run() },
            Step::Live => harness::HarnessAction::InstallTest { recipes: vec![path], options: self.bench.live_options.clone() },
            Step::Lint => unreachable!(),
        };
        let consent = if step == Step::Live { harness::Consent::Confirmed } else { harness::Consent::None };
        match harness::build(ws, &action, consent, &self.pwsh()) {
            Ok(spec) => {
                let job = self.start_job(spec, JobKind::Harness { step, recipe_id: id.clone() });
                if job.is_some() {
                    self.bench.steps.insert((id, step), StepState::Running);
                    self.bench.last_job = job;
                    self.console_open = true;
                }
                job.is_some()
            }
            Err(e) => {
                self.error(format!("{e:#}"));
                false
            }
        }
    }

    // ---- frame -----------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::P)) {
            self.perf.show = !self.perf.show;
            self.perf.max_ms = 0.0;
        }
        let save = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        if save && self.recipe.as_ref().is_some_and(|r| r.dirty()) {
            self.save_recipe();
        }
    }

    fn nav(&mut self, ui: &mut Ui) {
        let pad = egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 0));
        pad.show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                theme::brand::image(ui, theme::brand::Art::Mark, 34.0, "Yardstick logo");
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    theme::brand::image(ui, theme::brand::Art::Wordmark, 19.0, "Yardstick");
                    ui.label(RichText::new("Development Studio").small().weak());
                });
            });
        });
        ui.add_space(14.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        for (view, icon, name) in View::NAV {
            let busy = match view {
                View::Agent => self.jobs.iter().any(|j| j.is_agent() && j.proc.is_running()),
                View::Batch => self.jobs.iter().any(|j| matches!(j.kind, JobKind::BatchExtract { .. }) && j.proc.is_running()) || !self.batch.queue.is_empty(),
                View::Icons => self.icons.searching(),
                View::TestBench => self.bench.last_job.and_then(|id| self.job(id)).is_some_and(|j| j.proc.is_running()),
                _ => false,
            };
            let mut resp = theme::nav_item(ui, icon, name, self.view == view, busy);
            if busy {
                resp = resp.on_hover_text("Running");
            }
            if resp.clicked() {
                self.view = view;
            }
        }
        ui.spacing_mut().item_spacing.y = 6.0;
        let Some(r) = &self.recipe else { return };
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        pad.show(ui, |ui| {
            ui.label(RichText::new("Open recipe").small().weak());
            let mut t = RichText::new(&r.id).semibold();
            if r.dirty() {
                t = RichText::new(format!("{} ●", r.id)).semibold().color(theme::amber());
            }
            ui.label(t);
            let errors = lint::count(&r.diagnostics, lint::Severity::Error);
            let warnings = lint::count(&r.diagnostics, lint::Severity::Warning);
            ui.horizontal_wrapped(|ui| {
                if errors > 0 {
                    theme::badge(ui, format!("{errors} error(s)"), theme::red());
                }
                if warnings > 0 {
                    theme::badge(ui, format!("{warnings} warning(s)"), theme::amber());
                }
                if errors == 0 && warnings == 0 {
                    theme::badge(ui, "lint clean", theme::green());
                }
                if let Some((v, _)) = self.verdicts.get(&r.id) {
                    theme::badge(ui, v.label(), theme::verdict_color(*v));
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        // Every item uses the Caption style so the row shares one baseline.
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
            match &self.ws {
                Some(ws) => {
                    let color = match ws.kind {
                        WorkspaceKind::Development => theme::green(),
                        WorkspaceKind::Unconfigured => theme::amber(),
                        WorkspaceKind::Production { .. } => theme::red(),
                    };
                    theme::badge(ui, ws.kind.label(), color);
                    ui.label(RichText::new(ws.root.display().to_string()).weak());
                }
                None => {
                    theme::badge(ui, "No workspace", theme::grey());
                }
            }
            ui.separator();
            let running = self.running_jobs();
            let console_label = if running > 0 { format!("{}  Console ({running} running)", theme::icon::CONSOLE) } else { format!("{}  Console", theme::icon::CONSOLE) };
            if ui.selectable_label(self.console_open, console_label).clicked() {
                self.console_open = !self.console_open;
            }
            if self.perf.show {
                ui.separator();
                let paint = self.perf.paint_ms.map(|p| format!(", paint {p:.1} ms")).unwrap_or_default();
                ui.label(RichText::new(format!("UI {:.1} ms (max {:.1}){paint}", self.perf.avg_ms, self.perf.max_ms)).weak())
                    .on_hover_text(format!("Time to build (and paint) the last frames, renderer: {}. Ctrl+Shift+P hides this.", self.perf.renderer));
            }
            if let Some(at) = self.status.at
                && at.elapsed().as_secs() < 20
            {
                ui.separator();
                let color = if self.status.error { theme::red() } else { ui.visuals().text_color() };
                ui.add(egui::Label::new(RichText::new(&self.status.text).color(color)).truncate());
            }
        });
    }

    fn console(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Jobs").semibold());
            let mut select = None;
            for j in self.jobs.iter().rev().take(12) {
                let icon = if j.proc.is_running() {
                    "\u{F16A}"
                } else if j.proc.succeeded() {
                    "\u{E930}"
                } else {
                    "\u{EA39}"
                };
                let selected = self.console_job == Some(j.id);
                if ui.selectable_label(selected, format!("{icon} {}", j.proc.spec.label)).clicked() {
                    select = Some(j.id);
                }
            }
            if select.is_some() {
                self.console_job = select;
            }
            if self.jobs.iter().any(|j| !j.proc.is_running()) && ui.small_button("Clear finished").clicked() {
                self.jobs.retain(|j| j.proc.is_running());
                if self.console_job.is_some_and(|id| self.job(id).is_none()) {
                    self.console_job = self.jobs.last().map(|j| j.id);
                }
            }
        });
        ui.separator();
        let Some(id) = self.console_job else {
            ui.weak("No jobs yet. Harness runs, agent sessions, clones and publishes stream their output here.");
            return;
        };
        if let Some(job) = self.job_mut(id) {
            job_log(ui, &mut job.proc, "console");
        }
    }

    fn pending_modal(&mut self, ctx: &egui::Context) {
        let Some(Pending::OpenRecipe(path)) = &self.pending else { return };
        let path = path.clone();
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("discard-modal")).show(ctx, |ui| {
            ui.heading("Unsaved changes");
            let id = self.recipe.as_ref().map(|r| r.id.clone()).unwrap_or_default();
            ui.label(format!("'{id}' has unsaved changes."));
            ui.horizontal(|ui| {
                if theme::primary(ui, true, "\u{E74E}  Save and switch").clicked() {
                    choice = Some(0);
                }
                if ui.button("Discard").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(2);
                }
            });
        });
        match choice {
            Some(0) => {
                self.pending = None;
                if self.save_recipe() {
                    self.open_recipe(&path);
                }
            }
            Some(1) => {
                self.pending = None;
                self.open_recipe(&path);
            }
            Some(2) => self.pending = None,
            _ => {}
        }
    }
}

/// Renders a process log with cancel/copy controls. Shared by the console,
/// Test Bench and Agent views.
pub fn job_log(ui: &mut Ui, proc: &mut RunningProcess, salt: &str) {
    ui.horizontal(|ui| {
        if proc.is_running() {
            ui.spinner();
            ui.label(format!("Running {}", fmt_duration(proc.elapsed())));
            if ui.button("\u{E71A}  Cancel").clicked() {
                proc.cancel();
            }
        } else if proc.succeeded() {
            theme::badge(ui, format!("Succeeded in {}", fmt_duration(proc.elapsed())), theme::green());
        } else if proc.cancelled {
            theme::badge(ui, "Cancelled", theme::grey());
        } else {
            let code = proc.exit.flatten().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            theme::badge(ui, format!("Failed (exit {code})"), theme::red());
        }
        if ui.small_button("\u{E8C8}  Copy log").clicked() {
            ui.ctx().copy_text(proc.text());
        }
    });
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let n = proc.lines.len();
    let normal = ui.visuals().text_color();
    egui::ScrollArea::both()
        .id_salt(("job-log", salt, proc.pid))
        .stick_to_bottom(true)
        .auto_shrink(false)
        .show_rows(ui, row_h, n, |ui, range| {
            for line in &proc.lines[range] {
                let color = match line.stream {
                    Stream::Stdout => normal,
                    Stream::Stderr => theme::red(),
                    Stream::Studio => theme::blue(),
                };
                ui.label(RichText::new(&line.text).monospace().color(color));
            }
        });
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn on_exit(&mut self) {
        self.shutdown();
    }
}

impl StudioApp {
    /// Builds one frame; shared by the wgpu (eframe) and software runners.
    pub fn frame(&mut self, ui: &mut Ui) {
        let frame_start = Instant::now();
        let ctx = ui.ctx().clone();
        self.poll_jobs();
        self.shortcuts(&ctx);
        self.relint();
        if self.running_jobs() > 0 {
            // Keep the elapsed timers ticking.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        let base = theme::palette().base;
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(232.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(base).inner_margin(egui::Margin::symmetric(0, 8)))
            .show(ui, |ui| self.nav(ui));
        egui::Panel::bottom("status")
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(base).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| self.status_bar(ui));
        if self.console_open {
            egui::Panel::bottom("console").resizable(true).default_size(240.0).min_size(100.0).show(ui, |ui| self.console(ui));
        }
        let content = {
            let p = theme::palette();
            let top_left = if self.console_open { egui::CornerRadius { nw: 8, ..Default::default() } } else { egui::CornerRadius { nw: 8, sw: 8, ..Default::default() } };
            egui::Frame::new().fill(p.layer).stroke(egui::Stroke::new(1.0, p.card_stroke)).corner_radius(top_left).inner_margin(egui::Margin { left: 24, right: 24, top: 20, bottom: 12 })
        };
        egui::CentralPanel::default().frame(content).show(ui, |ui| match self.view {
            View::Home => views::home::show(self, ui),
            View::Recipes => views::recipes::show(self, ui),
            View::Designer => views::designer::show(self, ui),
            View::Editor => views::editor::show(self, ui),
            View::TestBench => views::testbench::show(self, ui),
            View::Agent => views::agent::show(self, ui),
            View::Batch => views::batch::show(self, ui),
            View::Icons => views::icons::show(self, ui),
            View::Runs => views::runs::show(self, ui),
            View::Docs => views::docs::show(self, ui),
            View::Promote => views::promote::show(self, ui),
        });

        self.pending_modal(&ctx);
        views::testbench::live_modal(self, &ctx);
        views::promote::publish_modal(self, &ctx);
        self.perf.record(self.view, frame_start.elapsed().as_secs_f32() * 1000.0);
        if self.perf.continuous {
            ctx.request_repaint();
        }
    }

    pub fn shutdown(&mut self) {
        for j in &mut self.jobs {
            if j.is_agent() || matches!(j.kind, JobKind::BatchExtract { .. }) {
                j.proc.cancel();
            }
        }
        self.save_settings();
    }
}
