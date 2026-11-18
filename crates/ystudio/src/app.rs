//! Application state, navigation, background jobs and the shared console.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use eframe::egui::{self, RichText, Ui};
use egui_commonmark::CommonMarkCache;
use ystudio_core::agent::{Change, Snapshot};
use ystudio_core::git;
use ystudio_core::harness::{self, InstallTestOptions};
use ystudio_core::lint::{self, Diagnostic, LintContext};
use ystudio_core::process::{ProcessSpec, RunningProcess, Stream, Waker, fmt_duration};
use ystudio_core::recipe::RecipeDoc;
use ystudio_core::results::{self, InstallRun, Verdict};
use ystudio_core::settings::Settings;
use ystudio_core::validation::{Completion, Ledger};
use ystudio_core::workspace::{RecipeEntry, RecipeLocation, Workspace, WorkspaceKind};

use crate::theme::{self, FluentText as _};
use crate::views;

/// The smallest size the layout is designed for. A smaller window scrolls
/// over a layout of this size rather than squeezing the pages into it.
pub const MIN_LAYOUT: egui::Vec2 = egui::vec2(1100.0, 680.0);

/// The layout size when `avail` is too small for `min`, or `None` when it
/// fits. Each scroll bar takes `bar` from the other direction, so a bar that
/// appears can make the other one necessary.
pub fn scroll_layout(avail: egui::Vec2, min: egui::Vec2, bar: f32) -> Option<egui::Vec2> {
    let (mut h, mut v) = (avail.x < min.x, avail.y < min.y);
    for _ in 0..2 {
        h = avail.x - if v { bar } else { 0.0 } < min.x;
        v = avail.y - if h { bar } else { 0.0 } < min.y;
    }
    if !h && !v {
        return None;
    }
    let w = if h { min.x } else { avail.x - bar };
    let height = if v { min.y } else { avail.y - bar };
    Some(egui::vec2(w, height))
}

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
    Settings,
    /// The first-run wizard; not in the sidebar.
    Setup,
    /// An installed plugin's tab, by its index in `plugins::Host::tabs`.
    Plugin(u16),
}

impl View {
    /// The sidebar. The open recipe's pages (Design, YAML, Test, Icon,
    /// Agent) are tabs inside Recipe Builder. Plugin tabs follow Publish
    /// ([`View::NAV_PLUGINS_AT`]).
    pub const NAV: [(View, &'static str, &'static str); 7] = [
        (View::Home, theme::icon::HOME, "Home"),
        (View::Recipes, theme::icon::LIBRARY, "Recipe Library"),
        (View::Batch, theme::icon::HAMMER, "Recipe Builder"),
        (View::Promote, theme::icon::UPLOAD, "Publish"),
        (View::Runs, theme::icon::CLOCK, "History"),
        (View::Settings, theme::icon::SETTINGS, "Settings"),
        (View::Docs, theme::icon::BOOK, "Help"),
    ];
    /// Where in [`View::NAV`] the plugin tabs go.
    pub const NAV_PLUGINS_AT: usize = 4;
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
    Agent { before: Snapshot, batch_item: Option<(String, usize)>, verification: Option<crate::workflow::Verification> },
    WorkflowHarness { step: Step, recipe_id: String },
    WorkflowRepair { before: Snapshot },
    /// An agent reading a request list into batch items.
    BatchExtract { batch_id: String, backend: String },
    Clone { dest: PathBuf },
    /// A plugin-owned development clone; it never replaces the open workspace.
    PluginClone { plugin: String, setting: String, dest: PathBuf },
    /// Branches pushed and pull requests opened for promoted recipes.
    PullRequests,
    GitFetch,
    GitPull,
    SyncShared,
    /// `Install-Yardstick -Status`, for the setup wizard.
    SetupStatus { result: PathBuf },
    /// The wizard's development-only `Install-Yardstick` run.
    Setup { result: PathBuf },
    /// `winget install` of a missing tool, or `gh auth setup-git`.
    InstallTool,
    /// A plugin step; `dir` is its work item, or the plugin's work root.
    PluginStep { plugin: String, dir: PathBuf, step: String, result: PathBuf, risk: ystudio_core::plugin::Risk, purpose: views::plugin_page::Purpose },
    /// An agent task of a plugin, working in a work item's folder.
    PluginAgent { plugin: String, dir: PathBuf, task: String, before: Snapshot },
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
        matches!(self.kind, JobKind::Agent { .. } | JobKind::WorkflowRepair { .. } | JobKind::PluginAgent { .. })
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
    /// Production's software dropbox, when this workspace has none of its own.
    pub production_dropbox: Option<PathBuf>,
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
    /// Every recorded agent run's AI credits, oldest first.
    pub usage: Vec<ystudio_core::usage::UsageRecord>,
    /// The credits of runs finished in this session, by job.
    pub usage_by_job: HashMap<u64, ystudio_core::usage::UsageRecord>,
    /// Where usage records are appended.
    usage_log: PathBuf,
    /// Recipe contents that passed a live install test.
    pub ledger: Ledger,
    /// [`Completion`] of every recipe in the ledger, by lower-case id.
    completion: BTreeMap<String, Completion>,
    pub runs: Vec<InstallRun>,
    pub perf: Perf,
    /// Branch and ahead/behind of the open workspace; `None` if it is not a git clone.
    pub git_status: Option<Result<git::Status, String>>,
    pub home: views::home::HomeState,
    pub library: views::recipes::LibraryState,
    pub bench: BenchState,
    pub agent: views::agent::AgentState,
    pub batch: views::batch::BatchState,
    pub icons: views::icons::IconsState,
    pub runs_ui: views::runs::RunsState,
    pub docs: views::docs::DocsState,
    pub promote: views::promote::PromoteState,
    pub update: views::update::UpdateState,
    pub settings_ui: views::settings::SettingsState,
    pub setup: views::setup::SetupState,
    pub tools: views::tools::ToolsState,
    /// The UAC prompt was just answered: bring the window back to the front,
    /// which the prompt and the hidden worker took the foreground from.
    pub refocus: bool,
    pub workflow: crate::workflow::State,
    pub elevation: crate::elevation::State,
    pub plugins: crate::plugins::Host,
    pub plugins_ui: views::plugins::ManagerState,
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

impl BenchState {
    fn take_live_consent(&mut self) -> harness::Consent {
        if std::mem::take(&mut self.live_ack) {
            harness::Consent::Confirmed
        } else {
            self.live_modal = true;
            harness::Consent::None
        }
    }
}

impl StudioApp {
    #[cfg(test)]
    pub(crate) fn isolated_for_tests(ws: Workspace) -> Self {
        let settings = Settings::default();
        let mut app = Self {
            home: views::home::HomeState::new(&settings), settings_ui: views::settings::SettingsState::new(&settings), setup: Default::default(), settings,
            ws: Some(ws), ws_error: None, view: View::Batch, recipes: Vec::new(),
            ws_cache: WsCache::default(), recipe: None, jobs: Vec::new(), next_job: 1,
            console_job: None, console_open: false, waker: Arc::new(|| {}),
            status: Status::default(), pending: None, md_cache: CommonMarkCache::default(),
            verdicts: BTreeMap::new(), ledger: Ledger::default(), completion: BTreeMap::new(),
            usage: Vec::new(), usage_by_job: HashMap::new(), usage_log: std::env::temp_dir().join(format!("ystudio-test-usage-{}.jsonl", std::process::id())),
            runs: Vec::new(), perf: Perf::default(), git_status: None,
            library: Default::default(), bench: Default::default(), agent: Default::default(),
            batch: Default::default(), icons: Default::default(), runs_ui: Default::default(),
            docs: Default::default(), promote: Default::default(), update: Default::default(), workflow: Default::default(),
            elevation: Default::default(), tools: views::tools::ToolsState::fixed(), refocus: false,
            plugins: Default::default(), plugins_ui: Default::default(),
        };
        app.refresh_recipes();
        app
    }

    #[cfg(test)]
    pub(crate) fn poll_for_tests(&mut self) {
        self.poll_jobs();
    }

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
            settings_ui: views::settings::SettingsState::new(&settings),
            setup: Default::default(),
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
            usage: ystudio_core::usage::UsageLog::at(ystudio_core::usage::UsageLog::default_path()).load(),
            usage_by_job: HashMap::new(),
            usage_log: ystudio_core::usage::UsageLog::default_path(),
            ledger: Ledger::default(),
            completion: BTreeMap::new(),
            runs: Vec::new(),
            perf: Perf::new(),
            git_status: None,
            library: Default::default(),
            bench: Default::default(),
            agent: Default::default(),
            batch: Default::default(),
            icons: Default::default(),
            runs_ui: Default::default(),
            docs: Default::default(),
            promote: Default::default(),
            update: Default::default(),
            workflow: Default::default(),
            elevation: Default::default(),
            tools: Default::default(),
            refocus: false,
            plugins: crate::plugins::Host::load(&ystudio_core::plugin::default_dir()),
            plugins_ui: Default::default(),
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
        views::setup::maybe_start(&mut app);
        views::tools::start(&mut app);
        if app.settings.diagnostic_logging {
            ystudio_core::diagnostics::prune();
            ystudio_core::diagnostics::note(&format!("Studio {} started.", env!("CARGO_PKG_VERSION")));
        }
        if app.settings.auto_check_updates {
            views::update::start_check(&mut app, true);
            views::plugins::start_check(&mut app, true);
        }
        app
    }

    /// `yardstick-studio <request list>` (or `--batch <file>`) opens Batch mode;
    /// `--icons <app name>` opens the Icon finder and searches.
    fn handle_args(&mut self, args: Vec<String>) {
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--updated" => {
                    // An update may need a newer tool: offer to install what is missing.
                    self.tools.open_if_missing = true;
                    if let Some(code) = it.next() {
                        let log = it.next();
                        views::update::report_installed(self, &code, log.as_deref());
                    }
                }
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
        let text = text.into();
        if self.settings.diagnostic_logging {
            ystudio_core::diagnostics::note(&text);
        }
        self.status = Status { text, error: false, at: Some(Instant::now()) };
    }

    pub fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.settings.diagnostic_logging {
            ystudio_core::diagnostics::note(&format!("ERROR {text}"));
        }
        self.status = Status { text, error: true, at: Some(Instant::now()) };
    }

    pub fn writable(&self) -> bool {
        self.ws.as_ref().is_some_and(Workspace::is_writable)
    }

    // ---- workspace -------------------------------------------------------

    pub fn open_workspace(&mut self, path: &Path) {
        if crate::workflow::busy(self) || !self.batch.queue.is_empty() {
            self.error("Finish or cancel the active agent/test pipeline before switching workspaces.");
            return;
        }
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
                self.refresh_git_status();
                self.refresh_remote(false);
                // Git's push check follows the workspace.
                views::tools::start(self);
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
        let production_dropbox = if ws.software_dropbox.is_none() {
            self.settings
                .primary_production()
                .and_then(|p| Workspace::open(p, &self.settings.production_paths, &[]).ok())
                .and_then(|prod| prod.software_dropbox)
        } else {
            None
        };
        self.ws_cache = WsCache {
            icons: ws.list_icons(),
            groups: ws.recipe_groups().into_keys().collect(),
            has_skill: ws.has_skill(),
            documents: ws.documents(),
            production_dropbox,
        };
        self.refresh_completion();
    }

    /// Re-hashes the recipes in the validation ledger.
    fn refresh_completion(&mut self) {
        let ledger = &self.ledger;
        self.completion = ledger
            .ids()
            .filter_map(|id| self.recipes.iter().find(|r| r.id.eq_ignore_ascii_case(id)).map(|r| (id.to_string(), ledger.completion(id, &r.path))))
            .collect();
    }

    /// Whether `id` is exactly what passed a live install test, or changed since.
    pub fn completion(&self, id: &str) -> &Completion {
        static NOT_VALIDATED: Completion = Completion::NotValidated;
        self.completion.get(&id.to_lowercase()).unwrap_or(&NOT_VALIDATED)
    }

    /// Records a live install test that validated `id`, if the recipe has not
    /// changed since the run started.
    pub fn record_validation(&mut self, id: &str, started: SystemTime, run: &InstallRun) {
        let Some(path) = self.ws.as_ref().and_then(|w| w.find_recipe(id)) else { return };
        let Some(result) = run.summary.as_ref().and_then(|s| s.results.iter().find(|r| r.id == id)) else { return };
        let saved = self
            .ledger
            .record(id, &path, started, result.verdict(), &run.timestamp, Some(run.dir.clone()))
            .and_then(|recorded| if recorded { self.ledger.save() } else { Ok(()) });
        if let Err(e) = saved {
            self.error(format!("Could not record that {id} passed: {e:#}"));
        }
        self.refresh_completion();
        // A recipe fixed and validated outside Recipe Builder (its Test or Agent tab)
        // must not keep a working-list item reading "failed".
        if self.completion(id).is_complete() {
            views::batch::recipe_validated(self, id, &run.timestamp, &run.dir);
        }
    }

    /// Reads the branch and ahead/behind counts (local only, no network).
    pub fn refresh_git_status(&mut self) {
        self.git_status = self
            .ws
            .as_ref()
            .filter(|w| git::is_repo(&w.root))
            .map(|w| git::status(&w.root).map_err(|e| format!("{e:#}")));
    }

    pub(crate) fn job_running(&self, pred: impl Fn(&JobKind) -> bool) -> bool {
        self.jobs.iter().any(|j| pred(&j.kind) && j.proc.is_running())
    }

    /// Brings the workspace up to date with its remotes: `git fetch` and
    /// Yardstick's shared icon/override sync. `manual` runs both regardless of
    /// the automatic settings; otherwise each follows its setting. Production
    /// folders and folders without git/sync are skipped quietly.
    pub fn refresh_remote(&mut self, manual: bool) {
        if manual || self.settings.auto_git_fetch {
            self.start_git_fetch(manual);
        }
        if manual || self.settings.auto_sync_shared {
            self.start_shared_sync(manual);
        }
    }

    /// `git fetch`. With `report`, explains why it could not run.
    pub fn start_git_fetch(&mut self, report: bool) {
        let Some(ws) = &self.ws else { return };
        if !ws.is_writable() || !git::is_repo(&ws.root) || self.job_running(|k| matches!(k, JobKind::GitFetch | JobKind::GitPull)) {
            if report && !ws.is_writable() {
                self.notify("Not fetching: production folders are read-only in the Studio.");
            }
            return;
        }
        match git::fetch(ws) {
            Ok(spec) => {
                self.start_background_job(spec, JobKind::GitFetch);
            }
            Err(e) if report => self.error(format!("{e:#}")),
            Err(_) => {}
        }
    }

    pub fn start_git_pull(&mut self) {
        let Some(ws) = &self.ws else { return };
        if self.job_running(|k| matches!(k, JobKind::GitFetch | JobKind::GitPull)) {
            return;
        }
        match git::pull(ws) {
            Ok(spec) => {
                self.start_job(spec, JobKind::GitPull);
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// Yardstick's shared-content sync. With `report`, explains why it could not run.
    pub fn start_shared_sync(&mut self, report: bool) {
        let Some(ws) = &self.ws else { return };
        if !ws.is_writable() || !ws.sync_shared_content || self.job_running(|k| matches!(k, JobKind::SyncShared)) {
            if report && ws.is_writable() && !ws.sync_shared_content {
                self.notify("Shared sync is off for this workspace (syncSharedContent in Local\\preferences.yaml).");
            }
            return;
        }
        match harness::sync_shared_content(ws, &self.pwsh()) {
            Ok(spec) => {
                self.start_background_job(spec, JobKind::SyncShared);
            }
            Err(e) if report => self.error(format!("{e:#}")),
            Err(_) => {}
        }
    }

    pub fn refresh_runs(&mut self) {
        let Some(ws) = &self.ws else { return };
        self.runs = results::list_install_runs(&ws.layout.harness_artifacts);
        self.ledger = Ledger::load(&ws.layout.harness_artifacts);
        self.verdicts.clear();
        for run in &self.runs {
            for r in run.summary.iter().flat_map(|s| &s.results) {
                self.verdicts.entry(r.id.clone()).or_insert((r.verdict(), run.timestamp.clone()));
            }
        }
        self.refresh_completion();
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
        let Some(r) = &self.recipe else { return };
        if r.lint_rev == r.rev {
            return;
        }
        let diags = match self.lint_diagnostics(&r.text, &r.id) {
            Ok(diags) => diags,
            Err(e) => { self.error(e); return; }
        };
        let r = self.recipe.as_mut().unwrap();
        r.diagnostics = diags;
        r.lint_rev = r.rev;
    }

    pub fn lint_diagnostics(&self, text: &str, id: &str) -> Result<Vec<Diagnostic>, String> {
        let ws = self.ws.as_ref().ok_or_else(|| "Open a workspace before linting a recipe.".to_string())?;
        let doc = RecipeDoc::parse(text).ok();
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
        let ctx = LintContext { file_id: Some(id), icon_exists: Some(&icon_exists), base: base.as_ref(), shared_recipe: true };
        let mut diags = lint::lint_text(text, &ctx);
        diags.sort_by_key(|d| d.severity);
        Ok(diags)
    }

    // ---- jobs ------------------------------------------------------------

    pub fn start_job(&mut self, spec: ProcessSpec, kind: JobKind) -> Option<u64> {
        match RunningProcess::spawn(spec, Some(self.waker.clone())) {
            Ok(proc) => Some(self.add_job(proc, kind)),
            Err(e) => {
                self.error(format!("{e:#}"));
                None
            }
        }
    }

    /// Tracks a process started elsewhere (the elevated worker's) as a job.
    pub fn add_job(&mut self, proc: RunningProcess, kind: JobKind) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        self.jobs.push(Job { id, proc, kind, started_sys: SystemTime::now(), handled: false });
        self.console_job = Some(id);
        id
    }

    /// Like [`Self::start_job`], but leaves the console on the job it was showing.
    pub fn start_background_job(&mut self, spec: ProcessSpec, kind: JobKind) -> Option<u64> {
        let shown = self.console_job;
        let id = self.start_job(spec, kind);
        if shown.is_some() {
            self.console_job = shown;
        }
        id
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
        crate::elevation::poll(self);
        crate::workflow::poll(self);
    }

    /// Reads what a finished agent run cost, notes it in its log, and keeps it.
    fn record_usage(&mut self, idx: usize) {
        let Some(pending) = self.jobs[idx].proc.spec.usage.clone() else { return };
        let Ok(record) = ystudio_core::usage::read(&pending) else { return };
        let subagents = if record.subagents > 0 { format!(", {} subagent(s)", record.subagents) } else { String::new() };
        self.jobs[idx].proc.lines.push(ystudio_core::process::LogLine {
            stream: Stream::Studio,
            text: format!("Used {:.2} AI credits ({}{subagents}).", record.credits, record.model),
        });
        if let Err(e) = ystudio_core::usage::UsageLog::at(self.usage_log.clone()).append(&record) {
            self.error(format!("Could not record the run's AI credits: {e:#}"));
        }
        self.usage_by_job.insert(self.jobs[idx].id, record.clone());
        self.usage.push(record);
    }

    fn on_job_finished(&mut self, id: u64) {
        let Some(idx) = self.jobs.iter().position(|j| j.id == id) else { return };
        self.jobs[idx].handled = true;
        self.record_usage(idx);
        if self.settings.diagnostic_logging {
            match ystudio_core::diagnostics::write_job(&self.jobs[idx].proc, self.jobs[idx].started_sys) {
                Ok(path) => ystudio_core::diagnostics::note(&format!("{} finished; log {}", self.jobs[idx].proc.spec.label, path.display())),
                Err(e) => ystudio_core::diagnostics::note(&format!("Could not write a job log: {e:#}")),
            }
        }
        let ok = self.jobs[idx].proc.succeeded();
        let cancelled = self.jobs[idx].proc.cancelled;
        let started = self.jobs[idx].started_sys;
        let text = self.jobs[idx].proc.report_text();
        let kind = self.jobs[idx].kind.clone();
        if cancelled && matches!(kind, JobKind::WorkflowHarness { .. } | JobKind::WorkflowRepair { .. }) {
            crate::workflow::stop(self, true);
        } else if cancelled && matches!(kind, JobKind::Agent { batch_item: Some(_), .. }) {
            views::batch::stop_queue(self);
        }
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
                    self.refresh_runs();
                    if step == Step::Live
                        && let Some(run) = &run
                    {
                        self.record_validation(&recipe_id, started, run);
                    }
                    self.bench.last_run = run;
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
            JobKind::SetupStatus { result } => views::setup::status_finished(self, ok, &result),
            JobKind::Setup { result } => views::setup::setup_finished(self, ok, &result),
            JobKind::WorkflowHarness { step, recipe_id } => {
                crate::workflow::harness_finished(self, step, &recipe_id, ok, started, text);
            }
            JobKind::WorkflowRepair { before } => {
                crate::workflow::repair_finished(self, before, ok, &text);
            }
            JobKind::Agent { before, batch_item, verification } => {
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
                let needs_approval = ystudio_core::agent::needs_approval(&text);
                // What the agent said stopped it, shown as is: a generic
                // "failed" hides blockers only the user can clear.
                let blocker = if needs_approval {
                    Some(crate::workflow::NEEDS_APPROVAL_NOTE.to_string())
                } else {
                    ystudio_core::agent::blocked_reason(&text).map(|r| format!("Blocked: {r}"))
                };
                match &blocker {
                    Some(b) => self.error(format!("Agent stopped ({n} file(s) changed). {b}")),
                    None if ok => self.notify(format!("Agent finished: {n} file(s) changed.")),
                    None => self.error(format!("Agent exited with an error ({n} file(s) changed).")),
                }
                let blocked = blocker.is_some();
                if let Some((batch_id, idx)) = &batch_item {
                    let outcome = match &blocker {
                        Some(b) => Err(b.clone()),
                        None if ok => Ok(()),
                        None => Err("agent exited with an error".to_string()),
                    };
                    views::batch::agent_finished(self, batch_id, *idx, outcome, verification.is_some());
                }
                if let Some(verification) = verification {
                    let resume_batch = batch_item.is_some();
                    let items = batch_item.into_iter().collect();
                    let target = crate::workflow::Target { id: verification.recipe_id.clone(), items };
                    if ok && !blocked {
                        crate::workflow::start(self, vec![target], verification.options, true, resume_batch);
                    } else {
                        if let Some((batch_id, idx)) = target.items.first() {
                            views::batch::check_finished(self, batch_id, *idx, ystudio_core::intake::BatchCheck {
                                recipe: target.id,
                                passed: false,
                                note: blocker.clone().unwrap_or_else(|| "The authoring agent exited with an error; see its log.".into()),
                                timestamp: ystudio_core::intake::timestamp(), summary_dir: None, live: false,
                            });
                            views::batch::start_next(self);
                        }
                    }
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
                    self.notify(msg);
                    // A fresh clone needs Yardstick's development-only setup next.
                    views::setup::open(self, views::setup::Step::Yardstick);
                } else {
                    self.error("git clone failed - see the console.");
                }
            }
            JobKind::PluginClone { plugin, setting, dest } => {
                if ok {
                    self.settings.plugins.entry(plugin.clone()).or_default().values.insert(setting, dest.display().to_string());
                    self.save_settings();
                    self.notify(format!("Private Yardstick clone ready for {}.", self.plugins.get(&plugin).map(|p| p.manifest.name.as_str()).unwrap_or(&plugin)));
                } else {
                    self.console_job = Some(id);
                    self.console_open = true;
                    self.error("Could not create the plugin's private Yardstick clone - see the console.");
                }
            }
            // Outcomes arrive through the Publish view, which offers to open them.
            JobKind::PullRequests => {}
            JobKind::GitFetch => {
                self.refresh_git_status();
                match (&self.git_status, ok) {
                    (_, false) => self.error("git fetch failed - see the console."),
                    (Some(Ok(s)), true) if s.behind > 0 => {
                        let msg = format!("{} new commit(s) on {} - Pull to update.", s.behind, s.upstream.as_deref().unwrap_or("upstream"));
                        self.notify(msg);
                    }
                    _ => {}
                }
            }
            JobKind::GitPull => {
                self.refresh_git_status();
                if ok {
                    self.refresh_recipes();
                    self.reload_clean_recipe();
                    self.notify("Pulled the latest Yardstick.");
                } else {
                    self.console_job = Some(id);
                    self.console_open = true;
                    self.error("git pull failed (it only fast-forwards) - see the console.");
                }
            }
            JobKind::SyncShared => {
                self.refresh_recipes();
                if let Some(r) = &mut self.recipe {
                    // Icon existence may have changed.
                    r.lint_rev = 0;
                }
                // The sync never fails outright: an unreachable share or a bad
                // overrides path is a warning, and must not read as success.
                let warnings: Vec<&str> = text.lines().filter_map(|l| l.trim().strip_prefix("WARNING:")).map(str::trim).collect();
                if let Some(first) = warnings.first() {
                    self.console_job = Some(id);
                    self.console_open = true;
                    let more = if warnings.len() > 1 { format!(" (and {} more)", warnings.len() - 1) } else { String::new() };
                    self.error(format!("Shared sync: {first}{more}"));
                } else if ok {
                    let summary = text.lines().rev().find(|l| l.starts_with("Synced ")).map(str::to_string);
                    self.notify(summary.unwrap_or_else(|| "Synced shared icons and recipe overrides.".into()));
                } else {
                    self.error("Shared icon/override sync failed - see the console.");
                }
            }
            JobKind::InstallTool => views::tools::install_finished(self, ok),
            JobKind::PluginStep { plugin, dir, step, result, purpose, .. } => {
                views::plugin_page::step_finished(self, id, &plugin, &dir, &step, &result, purpose);
            }
            JobKind::PluginAgent { plugin, dir, task, before } => {
                views::plugin_page::agent_finished(self, id, &plugin, &dir, &task, before);
            }
        }
    }

    /// Re-reads the open recipe from disk when it has no unsaved edits.
    fn reload_clean_recipe(&mut self) {
        let Some(r) = &self.recipe else { return };
        if r.dirty() {
            return;
        }
        let on_disk = ystudio_core::read_text(&r.path).ok();
        if on_disk.as_deref() != Some(r.saved.as_str()) && r.path.is_file() {
            let p = r.path.clone();
            self.open_recipe(&p);
        }
    }

    // ---- test bench ------------------------------------------------------

    pub fn step_state(&self, recipe_id: &str, step: Step) -> Option<StepState> {
        self.bench.steps.get(&(recipe_id.to_string(), step)).copied()
    }

    /// Queues steps for the open recipe and starts the first one.
    pub fn run_steps(&mut self, steps: &[Step]) {
        if crate::workflow::busy(self) || !self.batch.queue.is_empty() {
            self.error("Finish the active agent/test pipeline before starting another test.");
            return;
        }
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
        let consent = if step == Step::Live {
            let consent = self.bench.take_live_consent();
            if consent == harness::Consent::None {
                return true;
            }
            consent
        } else {
            harness::Consent::None
        };
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
        // Recipe editing is part of the unified Recipe Builder. The standalone
        // icon finder still belongs to the Library unless it is working on a
        // Builder item.
        let section = match self.view {
            View::Designer | View::Editor | View::TestBench | View::Agent => View::Batch,
            View::Icons if self.icons.batch_item.is_some() => View::Batch,
            View::Icons
                if self.recipe.as_ref().is_some_and(|r| self.icons.recipe_id.as_deref() == Some(r.id.as_str())) =>
            {
                View::Batch
            }
            View::Icons => View::Recipes,
            v => v,
        };
        let plugin_tabs: Vec<(View, String, String)> = self
            .plugins
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, id)| self.settings.plugins.get(*id).is_none_or(|p| p.enabled))
            .filter_map(|(i, id)| self.plugins.get(id).map(|p| (View::Plugin(i as u16), p.manifest.glyph(), p.manifest.name.clone())))
            .collect();
        for (i, (view, icon, name)) in View::NAV.into_iter().enumerate() {
            if i == View::NAV_PLUGINS_AT {
                for (view, glyph, name) in &plugin_tabs {
                    let View::Plugin(tab) = *view else { continue };
                    let busy = self.plugins.tabs.get(tab as usize).is_some_and(|id| views::plugin_page::plugin_busy(self, id));
                    let mut resp = theme::nav_item(ui, glyph, name, section == *view, busy);
                    if busy {
                        resp = resp.on_hover_text("Running");
                    }
                    if resp.clicked() {
                        self.view = *view;
                    }
                }
            }
            let busy = match view {
                View::Recipes => self.icons.searching(),
                View::Batch => {
                    self.workflow.busy()
                        || self.jobs.iter().any(|j| {
                            matches!(j.kind, JobKind::BatchExtract { .. } | JobKind::Agent { batch_item: None, .. }) && j.proc.is_running()
                        })
                        || self.bench.last_job.and_then(|id| self.job(id)).is_some_and(|j| j.proc.is_running())
                        || !self.batch.queue.is_empty()
                }
                View::Settings => self.update.badge_version(&self.settings.skipped_update).is_some(),
                _ => false,
            };
            let mut resp = theme::nav_item(ui, icon, name, section == view, busy);
            if busy {
                resp = resp.on_hover_text(if view == View::Settings { "An update is available" } else { "Running" });
            }
            if resp.clicked() {
                self.view = view;
            }
        }
        ui.spacing_mut().item_spacing.y = 6.0;
        let Some(r) = &self.recipe else { return };
        let mut show_recipe = false;
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        pad.show(ui, |ui| {
            ui.label(RichText::new("Open recipe").small().weak());
            let mut t = RichText::new(&r.id).semibold();
            if r.dirty() {
                t = RichText::new(format!("{} ●", r.id)).semibold().color(theme::amber());
            }
            if ui.link(t).on_hover_text("Back to this recipe").clicked() {
                show_recipe = true;
            }
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
                views::recipe_status_badge(self, ui, &r.id);
            });
        });
        if show_recipe && !matches!(self.view, View::Designer | View::Editor | View::TestBench | View::Agent) {
            self.view = View::Designer;
        }
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
            crate::elevation::badge(self, ui);
            views::update::badge(self, ui);
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
                let credits = self.usage_by_job.get(&j.id).map(|u| format!(" · {:.1} cr", u.credits)).unwrap_or_default();
                if ui.selectable_label(selected, format!("{icon} {}{credits}", j.proc.spec.label)).clicked() {
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
    ui.horizontal_wrapped(|ui| {
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
        if proc.lines.iter().any(|l| l.stream.is_narration()) {
            let mut show = narration_shown(ui, salt, proc.pid);
            if ui
                .checkbox(&mut show, "Thinking & tool calls")
                .on_hover_text("Show the agent's reasoning and each tool it calls, as it works.")
                .changed()
            {
                ui.data_mut(|d| {
                    d.insert_temp(egui::Id::unique(("job-log-narration", salt, proc.pid)), show);
                    d.remove::<LogSelection>(egui::Id::unique(("job-log-selection", salt, proc.pid)));
                });
            }
        }
        let view = log_view(proc, narration_shown(ui, salt, proc.pid));
        let sel_id = egui::Id::unique(("job-log-selection", salt, proc.pid));
        let selected = ui.data(|d| d.get_temp::<LogSelection>(sel_id)).and_then(|s| s.range(view.len()));
        if let Some(range) = selected.clone() {
            let n = range.len();
            if ui.small_button(format!("\u{E8C8}  Copy {n} line{}", if n == 1 { "" } else { "s" })).clicked() {
                ui.ctx().copy_text(log_text(&proc.lines, &view[range]));
            }
        }
        if ui.small_button("\u{E8C8}  Copy log").clicked() {
            ui.ctx().copy_text(log_text(&proc.lines, &view));
        }
        if selected.is_none() && !proc.lines.is_empty() {
            ui.label(RichText::new("Drag across lines to select them, then Ctrl+C.").small().weak());
        }
    });
    let view = log_view(proc, narration_shown(ui, salt, proc.pid));
    log_lines(ui, proc, salt, &view);
}

/// Whether this log shows agent narration (thinking, tool calls); on by default.
fn narration_shown(ui: &Ui, salt: &str, pid: u32) -> bool {
    ui.data(|d| d.get_temp::<bool>(egui::Id::unique(("job-log-narration", salt, pid)))).unwrap_or(true)
}

/// Indices of the lines the log shows, in order.
fn log_view(proc: &RunningProcess, narration: bool) -> Vec<usize> {
    proc.lines.iter().enumerate().filter(|(_, l)| narration || !l.stream.is_narration()).map(|(i, _)| i).collect()
}

/// A range of selected log lines, by index. Logs only grow, so indices stay valid
/// while new output arrives and while rows scroll out of the virtual view.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct LogSelection {
    anchor: usize,
    end: usize,
    dragging: bool,
}

impl LogSelection {
    fn range(&self, len: usize) -> Option<std::ops::Range<usize>> {
        let lo = self.anchor.min(self.end);
        let hi = self.anchor.max(self.end);
        (lo < len).then(|| lo..(hi + 1).min(len))
    }
}

fn log_text(lines: &[ystudio_core::process::LogLine], view: &[usize]) -> String {
    view.iter().map(|&i| lines[i].text.as_str()).collect::<Vec<_>>().join("\n")
}

/// The virtualised log body with line selection: click a line, drag or Shift+click to
/// extend, Ctrl+C or the context menu to copy. egui's own label selection cannot span
/// rows that `show_rows` has scrolled out of existence, so selection is by line.
/// `view` lists the line indices shown; selection positions index into it.
fn log_lines(ui: &mut Ui, proc: &RunningProcess, salt: &str, view: &[usize]) {
    let sel_id = egui::Id::unique(("job-log-selection", salt, proc.pid));
    let mut sel = ui.data(|d| d.get_temp::<LogSelection>(sel_id));
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let n = view.len();
    let normal = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let stderr_color = if proc.spec.stderr_is_progress { ui.visuals().weak_text_color() } else { theme::red() };
    let select_bg = ui.visuals().selection.bg_fill;
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let (shift, pointer, primary_down) = ui.input(|i| (i.modifiers.shift, i.pointer.interact_pos(), i.pointer.primary_down()));
    let mut menu_action: Option<LogMenu> = None;
    let output = egui::ScrollArea::both()
        .id_salt(("job-log", salt, proc.pid))
        .stick_to_bottom(true)
        .auto_shrink(false)
        .show_rows(ui, row_h, n, |ui, range| {
            let selected = sel.and_then(|s| s.range(n));
            let mut rows: Vec<(usize, egui::Rect)> = Vec::with_capacity(range.len());
            for i in range {
                let line = &proc.lines[view[i]];
                let color = match line.stream {
                    Stream::Stdout => normal,
                    Stream::Stderr => stderr_color,
                    Stream::Studio => theme::blue(),
                    Stream::Thinking | Stream::Tool => weak,
                };
                // `show_rows` requires every child to have exactly `row_h` height.
                // Keep process output on one line so long commands scroll sideways
                // instead of changing the virtual row geometry between repaints.
                let galley = ui.painter().layout_no_wrap(line.text.clone(), font.clone(), color);
                let width = galley.size().x.max(ui.available_width());
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::click_and_drag());
                if selected.as_ref().is_some_and(|r| r.contains(&i)) {
                    ui.painter().rect_filled(rect.expand2(egui::vec2(4.0, ui.spacing().item_spacing.y / 2.0)), 0.0, select_bg);
                }
                // Thinking is set in from a thin accent rule, like a quotation.
                let indent = if line.stream == Stream::Thinking {
                    let x = rect.left() + 3.0;
                    ui.painter().vline(x, rect.y_range(), egui::Stroke::new(2.0, theme::accent().gamma_multiply(0.6)));
                    12.0
                } else {
                    0.0
                };
                ui.painter().galley(rect.left_top() + egui::vec2(indent, 0.0), galley, color);
                rows.push((i, rect));
                if resp.drag_started_by(egui::PointerButton::Primary) || resp.clicked() {
                    sel = Some(match sel {
                        Some(s) if shift => LogSelection { end: i, dragging: resp.dragged(), ..s },
                        _ => LogSelection { anchor: i, end: i, dragging: resp.dragged() },
                    });
                }
                resp.context_menu(|ui| {
                    if !selected.as_ref().is_some_and(|r| r.contains(&i)) {
                        sel = Some(LogSelection { anchor: i, end: i, dragging: false });
                    }
                    if ui.button("\u{E8C8}  Copy selected lines").clicked() {
                        menu_action = Some(LogMenu::CopySelection);
                        ui.close();
                    }
                    if ui.button("\u{E8C8}  Copy log").clicked() {
                        menu_action = Some(LogMenu::CopyAll);
                        ui.close();
                    }
                    if ui.button("\u{E8B3}  Select all").clicked() {
                        menu_action = Some(LogMenu::SelectAll);
                        ui.close();
                    }
                });
            }
            // While dragging, the line under the pointer (clamped to the visible rows) ends the range.
            if let (Some(mut s), Some(p)) = (sel, pointer)
                && s.dragging
                && primary_down
                && let (Some(first), Some(last)) = (rows.first(), rows.last())
            {
                s.end = if p.y < first.1.top() {
                    first.0
                } else if p.y > last.1.bottom() {
                    last.0
                } else {
                    rows.iter().find(|(_, r)| p.y <= r.bottom()).map_or(last.0, |(i, _)| *i)
                };
                sel = Some(s);
            }
        });
    // Drag past the top or bottom edge to scroll and keep extending the selection.
    if let (Some(s), Some(p)) = (sel, pointer)
        && s.dragging
        && primary_down
    {
        let view = output.inner_rect;
        let dy = if p.y < view.top() { view.top() - p.y } else if p.y > view.bottom() { view.bottom() - p.y } else { 0.0 };
        if dy != 0.0 {
            ui.scroll_with_delta(egui::vec2(0.0, dy.clamp(-row_h * 3.0, row_h * 3.0)));
            ui.ctx().request_repaint();
        }
    }
    if let Some(s) = &mut sel
        && !primary_down
    {
        s.dragging = false;
    }
    match menu_action {
        Some(LogMenu::SelectAll) if n > 0 => sel = Some(LogSelection { anchor: 0, end: n - 1, dragging: false }),
        Some(LogMenu::CopyAll) => ui.ctx().copy_text(log_text(&proc.lines, view)),
        _ => {}
    }
    let hovered = pointer.is_some_and(|p| output.inner_rect.contains(p));
    let focus_elsewhere = ui.memory(|m| m.focused().is_some());
    let (copy, select_all, escape) = ui.input(|i| {
        (
            i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            i.modifiers.command && i.key_pressed(egui::Key::A),
            i.key_pressed(egui::Key::Escape),
        )
    });
    if let Some(range) = sel.and_then(|s| s.range(n)) {
        if menu_action == Some(LogMenu::CopySelection) || (copy && !focus_elsewhere) {
            ui.ctx().copy_text(log_text(&proc.lines, &view[range]));
        }
        if escape && !focus_elsewhere {
            sel = None;
        }
    }
    if select_all && hovered && !focus_elsewhere && n > 0 {
        sel = Some(LogSelection { anchor: 0, end: n - 1, dragging: false });
    }
    ui.data_mut(|d| match sel {
        Some(s) => {
            d.insert_temp(sel_id, s);
        }
        None => {
            d.remove::<LogSelection>(sel_id);
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum LogMenu {
    CopySelection,
    CopyAll,
    SelectAll,
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
        views::update::poll(self);
        views::plugins::poll(self);
        views::tools::poll(self, &ctx);
        if std::mem::take(&mut self.refocus) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.shortcuts(&ctx);
        self.relint();
        if self.running_jobs() > 0 {
            // Keep the elapsed timers ticking.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        // Below the minimum the pages overlap, so the whole layout keeps that
        // size and the window scrolls over it instead.
        let avail = ui.available_size();
        let bar = egui::style::ScrollStyle::solid().allocated_width();
        match scroll_layout(avail, MIN_LAYOUT, bar) {
            None => self.layout(ui),
            Some(size) => {
                ui.scope(|ui| {
                    // Solid bars: floating ones are invisible until hovered.
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    ui.spacing_mut().scroll.foreground_color = false;
                    let text = theme::palette().text;
                    let w = &mut ui.visuals_mut().widgets;
                    w.inactive.bg_fill = text.gamma_multiply(0.45);
                    w.hovered.bg_fill = text.gamma_multiply(0.65);
                    w.active.bg_fill = text.gamma_multiply(0.85);
                    egui::ScrollArea::both().id_salt("window-scroll").auto_shrink(false).show(ui, |ui| {
                        ui.allocate_ui(size, |ui| {
                            ui.set_min_size(size);
                            self.layout(ui);
                        });
                    });
                });
            }
        }

        self.pending_modal(&ctx);
        views::recipes::windows(self, &ctx);
        views::batch::paste_window(self, &ctx);
        views::batch::modals(self, &ctx);
        views::testbench::live_modal(self, &ctx);
        views::promote::modals(self, &ctx);
        crate::workflow::modal(self, &ctx);
        views::plugin_page::modals(self, &ctx);
        views::plugins::modals(self, &ctx);
        self.perf.record(self.view, frame_start.elapsed().as_secs_f32() * 1000.0);
        if self.perf.continuous {
            ctx.request_repaint();
        }
    }

    /// The sidebar, status bar, console and current page, filling `ui`.
    fn layout(&mut self, ui: &mut Ui) {
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
            // Never so tall that the page above loses its content.
            let max = (ui.available_height() - 400.0).max(100.0);
            egui::Panel::bottom("console").resizable(true).default_size(240.0).size_range(100.0..=max).show(ui, |ui| self.console(ui));
        }
        let content = {
            let p = theme::palette();
            let top_left = if self.console_open { egui::CornerRadius { nw: 8, ..Default::default() } } else { egui::CornerRadius { nw: 8, sw: 8, ..Default::default() } };
            egui::Frame::new().fill(p.layer).stroke(egui::Stroke::new(1.0, p.card_stroke)).corner_radius(top_left).inner_margin(egui::Margin { left: 24, right: 24, top: 20, bottom: 12 })
        };
        egui::CentralPanel::default().frame(content).show(ui, |ui| {
            views::update::banner(self, ui);
            match self.view {
                View::Home => views::home::show(self, ui),
                View::Recipes => views::recipes::show(self, ui),
                View::Designer | View::Editor | View::TestBench | View::Agent => views::batch::show_recipe(self, ui),
                View::Batch => views::batch::show(self, ui),
                View::Icons
                    if self.icons.batch_item.is_none()
                        && self.recipe.as_ref().is_some_and(|r| self.icons.recipe_id.as_deref() == Some(r.id.as_str())) =>
                {
                    views::batch::show_recipe(self, ui)
                }
                View::Icons => views::icons::show(self, ui),
                View::Runs => views::runs::show(self, ui),
                View::Docs => views::docs::show(self, ui),
                View::Promote => views::promote::show(self, ui),
                View::Settings => views::settings::show(self, ui),
                View::Setup => views::setup::show(self, ui),
                View::Plugin(tab) => views::plugin_page::show(self, ui, tab),
            }
        });
    }

    pub fn shutdown(&mut self) {
        for j in &mut self.jobs {
            if j.is_agent() || matches!(j.kind, JobKind::BatchExtract { .. }) {
                j.proc.cancel();
            }
        }
        // Stops the elevated worker and any test it is running.
        self.elevation = Default::default();
        self.save_settings();
    }
}

#[cfg(test)]
mod bench_tests {
    use super::*;

    #[test]
    fn live_checks_pause_for_consent_and_consume_it_once() {
        let mut bench = BenchState::default();
        assert_eq!(bench.take_live_consent(), harness::Consent::None);
        assert!(bench.live_modal);
        assert!(!bench.live_ack);

        bench.live_ack = true;
        bench.live_modal = false;
        assert_eq!(bench.take_live_consent(), harness::Consent::Confirmed);
        assert!(!bench.live_ack);
        assert!(!bench.live_modal);

        assert_eq!(bench.take_live_consent(), harness::Consent::None);
        assert!(bench.live_modal);
    }

    #[test]
    fn all_checks_run_in_order_with_live_last() {
        assert_eq!(Step::ALL, [Step::Lint, Step::Schema, Step::Download, Step::Package, Step::DryRun, Step::Live]);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn long_console_lines_keep_the_virtual_row_height() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.set_max_width(120.0);
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            let response = ui.add(
                egui::Label::new(RichText::new("a long console line that must scroll horizontally instead of wrapping").monospace())
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
            assert!(response.rect.height() <= row_h);
        });
        assert!(!output.shapes.is_empty());
        output.textures_delta.clear();
    }

    #[test]
    fn log_selection_is_ordered_and_clamped_to_the_log() {
        let s = LogSelection { anchor: 5, end: 2, dragging: false };
        assert_eq!(s.range(10), Some(2..6));
        assert_eq!(s.range(4), Some(2..4));
        assert_eq!(s.range(2), None);
    }

    #[test]
    fn dragging_across_log_lines_selects_and_copies_them() {
        use ystudio_core::process::{LogLine, ProcessSpec};
        let (mut proc, _feed) = RunningProcess::remote(ProcessSpec::new("test", "pwsh", &std::env::temp_dir()), "test", None, Box::new(|| {}));
        proc.lines = (0..20).map(|i| LogLine { stream: Stream::Stdout, text: format!("line {i}") }).collect();
        let ctx = egui::Context::default();
        let top = std::cell::Cell::new(0.0);
        let pitch = std::cell::Cell::new(0.0);
        let frame = |events: Vec<egui::Event>, measure: bool| {
            let input = egui::RawInput { events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                if measure {
                    top.set(ui.cursor().top());
                    pitch.set(ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y);
                }
                log_lines(ui, &proc, "test", &(0..proc.lines.len()).collect::<Vec<_>>());
            });
            out.textures_delta.clear();
            out.platform_output
        };
        frame(Vec::new(), true);
        let at = |row: f32| egui::pos2(40.0, top.get() + pitch.get() * row + pitch.get() / 2.0);
        let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        frame(vec![egui::Event::PointerMoved(at(1.0)), button(at(1.0), true)], false);
        frame(vec![egui::Event::PointerMoved(at(2.0))], false);
        frame(vec![egui::Event::PointerMoved(at(3.0))], false);
        frame(vec![button(at(3.0), false)], false);
        let out = frame(vec![egui::Event::Copy], false);
        let copied: Vec<_> = out.commands.iter().filter_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        }).collect();
        assert_eq!(copied, ["line 1\nline 2\nline 3"]);
    }

    #[test]
    fn small_windows_get_a_scrolled_layout_at_the_minimum_size() {
        let min = egui::vec2(1100.0, 680.0);
        assert_eq!(scroll_layout(egui::vec2(1400.0, 900.0), min, 10.0), None);
        assert_eq!(scroll_layout(egui::vec2(1100.0, 680.0), min, 10.0), None);
        // Too narrow: scrolls sideways, and the bar takes some height.
        assert_eq!(scroll_layout(egui::vec2(900.0, 900.0), min, 10.0), Some(egui::vec2(1100.0, 890.0)));
        // Too short: scrolls down; the layout is as wide as is left beside the bar.
        assert_eq!(scroll_layout(egui::vec2(1400.0, 500.0), min, 10.0), Some(egui::vec2(1390.0, 680.0)));
        assert_eq!(scroll_layout(egui::vec2(700.0, 450.0), min, 10.0), Some(egui::vec2(1100.0, 680.0)));
        // A bar that appears can make the other direction necessary too.
        assert_eq!(scroll_layout(egui::vec2(1105.0, 500.0), min, 10.0), Some(egui::vec2(1100.0, 680.0)));
        assert_eq!(scroll_layout(egui::vec2(1500.0, 675.0), min, 10.0), Some(egui::vec2(1490.0, 680.0)));
    }
}
