//! Plugins: installable packages that each add a tab to the Studio.
//!
//! A plugin is a zip holding a `plugin.yaml` manifest, PowerShell step scripts
//! and agent instructions, and never native code. The Studio draws the
//! plugin's page with one of its built-in page types and runs each step as
//! `pwsh -File <script> -StudioInput <input.json> -StudioResult <result.json>`,
//! so only file paths reach a command line.
//!
//! Steps that install software or write to the tenant must be gated by an
//! approval bound to the SHA-256 of the file describing the change. The host
//! re-hashes it when the step starts, the grant is single-use, and the script
//! checks the hash again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::process::ProcessSpec;
use crate::update::{Release, is_newer};

/// The page and widget kit this Studio provides. A manifest asking for more
/// needs a newer Studio.
pub const HOST_API: u32 = 1;
pub const MANIFEST: &str = "plugin.yaml";
pub const WORK_ITEM_FILE: &str = "work-item.json";
/// Every plugin declares this folder setting; work items live under it.
pub const WORK_ROOT_KEY: &str = "workRoot";
const INSTALL_RECORD: &str = ".installed.json";
const STAGING: &str = ".staging";
const PREVIOUS: &str = ".previous";
pub(crate) const LOCK_FILE: &str = ".studio-lock";
/// Studio-owned scratch inside a work item: step inputs/results, agent copies.
pub const STUDIO_DIR: &str = ".studio";
pub(crate) const APPROVALS_DIR: &str = "approvals";
const ALLOWED_EXTENSIONS: &[&str] = &["yaml", "yml", "ps1", "psm1", "psd1", "md", "json", "txt", "png", "ico", "svg", "xml"];

/// `%APPDATA%\YardstickStudio\plugins`.
pub fn default_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("YardstickStudio").join("plugins")
}

// ---------------------------------------------------------------- manifest

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub description: String,
    /// Segoe Fluent Icons code point in hex, e.g. `E7B8`.
    pub icon: String,
    pub studio: StudioReq,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub requires: Requires,
    #[serde(default)]
    pub settings: Vec<SettingDef>,
    pub page: PageDef,
    #[serde(default)]
    pub agent: Option<AgentDef>,
    #[serde(default)]
    pub steps: Vec<StepDef>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioReq {
    #[serde(default)]
    pub min_version: String,
    pub host_api: u32,
}

/// Where updates come from: GitHub releases tagged `<tagPrefix><version>`
/// with an asset `<id>-<version>.zip`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub github: String,
    pub tag_prefix: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Requires {
    pub plugins: Vec<PluginReq>,
    pub modules: Vec<ModuleReq>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginReq {
    pub id: String,
    #[serde(default)]
    pub min_version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModuleReq {
    pub name: String,
    #[serde(default)]
    pub min: String,
    #[serde(default)]
    pub max: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingDef {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(rename = "type")]
    pub kind: SettingKind,
    /// May contain `{workspace}`, the open Development workspace.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub choices: Vec<String>,
}

impl SettingDef {
    pub fn label(&self) -> &str {
        if self.label.is_empty() { &self.key } else { &self.label }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    Folder,
    File,
    Guid,
    String,
    Bool,
    Choice,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageDef {
    #[serde(rename = "type")]
    pub kind: PageKind,
    /// What one work item is called, e.g. `Ticket`.
    #[serde(default = "default_noun")]
    pub noun: String,
    /// Work item folder name: `{date}`, `{ref}` and `{slug}` are filled in.
    #[serde(default = "default_folder")]
    pub folder: String,
    /// How new work items start: `text` (pasted request), `files` (dropped files).
    #[serde(default)]
    pub intake: Vec<String>,
    /// A plugin-scope step whose result `rows` become new work items.
    #[serde(default)]
    pub importer: Option<String>,
    /// Catalog pages: the step that checks the connection.
    #[serde(default)]
    pub probe: Option<String>,
    /// Catalog pages: the search step; its `rows` fill the table.
    #[serde(default)]
    pub query: Option<String>,
    /// Catalog pages: result row fields shown as columns.
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub panels: Vec<PanelDef>,
    /// Work-item steps and agent tasks in the order the page lists them.
    /// Defaults to the order `after` implies, ties in declaration order.
    #[serde(default)]
    pub pipeline: Vec<String>,
}

fn default_noun() -> String {
    "Item".into()
}

fn default_folder() -> String {
    "{date}-{ref}-{slug}".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageKind {
    /// A list of work-item folders, each with a step pipeline.
    Workitems,
    /// A searchable table of external objects with per-row actions.
    Catalog,
}

/// A read-only view of files in the selected work item.
#[derive(Debug, Clone, Deserialize)]
pub struct PanelDef {
    #[serde(rename = "type")]
    pub kind: PanelKind,
    pub title: String,
    /// Work-item-relative file (table, markdown).
    #[serde(default)]
    pub file: Option<String>,
    /// JSON pointer to the row array in `file`, e.g. `/products`.
    #[serde(default)]
    pub rows: Option<String>,
    #[serde(default)]
    pub columns: Vec<String>,
    /// Work-item-relative globs (files).
    #[serde(default)]
    pub globs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelKind {
    Table,
    Markdown,
    Files,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDef {
    pub instructions: String,
    #[serde(default)]
    pub references: Vec<String>,
    /// Added to the Studio's own deny list for this plugin's agent runs.
    #[serde(default)]
    pub denied_commands: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<AgentTaskDef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskDef {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Plugin-relative markdown file with the task prompt.
    pub prompt: String,
    #[serde(default)]
    pub after: Vec<String>,
    /// Work-item-relative paths the task is expected to write.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Steps whose latest results are passed to the agent as evidence.
    #[serde(default)]
    pub evidence_from: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepDef {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Plugin-relative `.ps1`.
    pub script: String,
    /// Fixed parameters passed in `input.json`.
    #[serde(default)]
    pub params: Map<String, Value>,
    pub risk: Risk,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub gate: Gate,
    #[serde(default)]
    pub scope: Scope,
}

impl StepDef {
    pub fn label(&self) -> &str {
        if self.label.is_empty() { &self.id } else { &self.label }
    }
}

/// What a step can touch. Shown on the trust prompt and decides the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    LocalRead,
    LocalWrite,
    /// Installs software on this machine.
    LocalInstall,
    TenantRead,
    /// Creates or changes objects in Intune.
    TenantWrite,
    SccmRead,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::LocalRead => "Reads local files",
            Risk::LocalWrite => "Writes local files",
            Risk::LocalInstall => "Installs software on this PC",
            Risk::TenantRead => "Reads the Intune tenant",
            Risk::TenantWrite => "Changes the Intune tenant",
            Risk::SccmRead => "Reads Configuration Manager",
        }
    }

    pub fn needs_hash_gate(self) -> bool {
        matches!(self, Risk::LocalInstall | Risk::TenantWrite)
    }
}

/// Where a step runs: in the selected work item, or in the plugin's work root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Workitem,
    Plugin,
}

/// What the user must do before a step runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "GateRepr")]
pub enum Gate {
    #[default]
    None,
    /// A confirmation click.
    Confirm,
    /// An approval bound to the SHA-256 of `file`; `summary` is the markdown
    /// shown alongside it.
    Hash { file: String, summary: Option<String> },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GateRepr {
    Word(String),
    Full { kind: String, file: Option<String>, summary: Option<String> },
}

impl TryFrom<GateRepr> for Gate {
    type Error = String;

    fn try_from(r: GateRepr) -> std::result::Result<Self, String> {
        let (kind, file, summary) = match r {
            GateRepr::Word(w) => (w, None, None),
            GateRepr::Full { kind, file, summary } => (kind, file, summary),
        };
        match kind.as_str() {
            "none" => Ok(Gate::None),
            "confirm" => Ok(Gate::Confirm),
            "hash" => Ok(Gate::Hash { file: file.ok_or("a hash gate needs a file")?, summary }),
            other => Err(format!("unknown gate '{other}'")),
        }
    }
}

/// One row of a work item's pipeline.
#[derive(Debug, Clone, Copy)]
pub enum Stage<'a> {
    Task(&'a AgentTaskDef),
    Step(&'a StepDef),
}

impl<'a> Stage<'a> {
    pub fn id(&self) -> &'a str {
        match self {
            Stage::Task(t) => &t.id,
            Stage::Step(s) => &s.id,
        }
    }

    pub fn label(&self) -> &'a str {
        match self {
            Stage::Task(t) => &t.label,
            Stage::Step(s) => s.label(),
        }
    }

    pub fn description(&self) -> &'a str {
        match self {
            Stage::Task(t) => &t.description,
            Stage::Step(s) => &s.description,
        }
    }

    pub fn after(&self) -> &'a [String] {
        match self {
            Stage::Task(t) => &t.after,
            Stage::Step(s) => &s.after,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionDef {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(flatten)]
    pub kind: ActionKind,
}

/// A button on a catalog row or a work item.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum ActionKind {
    /// Runs one of this plugin's steps.
    #[serde(rename = "step")]
    Step { step: String },
    /// Starts a work item in another plugin, copying `files` into its `intake`.
    #[serde(rename = "handoff.workitem")]
    HandoffWorkItem {
        plugin: String,
        files: Vec<String>,
        #[serde(default)]
        title: String,
    },
    /// Opens the Studio's recipe agent with notes from `notesFile`.
    #[serde(rename = "handoff.recipe", rename_all = "camelCase")]
    HandoffRecipe { notes_file: String },
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        serde_yaml::from_str(text).map_err(|e| anyhow!("{MANIFEST}: {e}"))
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST);
        Self::parse(&crate::read_text(&path).with_context(|| format!("reading {}", path.display()))?)
    }

    /// The nav icon.
    pub fn glyph(&self) -> String {
        u32::from_str_radix(&self.icon, 16).ok().and_then(char::from_u32).map(String::from).unwrap_or_default()
    }

    pub fn step(&self, id: &str) -> Option<&StepDef> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn task(&self, id: &str) -> Option<&AgentTaskDef> {
        self.agent.as_ref()?.tasks.iter().find(|t| t.id == id)
    }

    /// A work item's pipeline: agent tasks and work-item steps, in page order.
    pub fn pipeline(&self) -> Vec<Stage<'_>> {
        let tasks = self.agent.iter().flat_map(|a| &a.tasks).map(Stage::Task);
        let all: Vec<Stage> = tasks.chain(self.steps.iter().filter(|s| s.scope == Scope::Workitem).map(Stage::Step)).collect();
        if !self.page.pipeline.is_empty() {
            return self.page.pipeline.iter().filter_map(|id| all.iter().find(|s| s.id() == id).copied()).collect();
        }
        // Kahn's algorithm, always taking the first-declared stage that is ready.
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut out = Vec::new();
        let mut left = all;
        while !left.is_empty() {
            let ready = left.iter().position(|s| s.after().iter().all(|a| done.contains(a.as_str()) || !left.iter().any(|o| o.id() == a)));
            let stage = left.remove(ready.unwrap_or(0));
            done.insert(stage.id());
            out.push(stage);
        }
        out
    }

    /// Why this Studio cannot run the plugin, if it can't.
    pub fn incompatibility(&self, studio_version: &str) -> Option<String> {
        if self.studio.host_api > HOST_API {
            return Some(format!("needs a newer Studio (plugin API {}, this Studio has {HOST_API})", self.studio.host_api));
        }
        if !self.studio.min_version.is_empty() && is_newer(&self.studio.min_version, studio_version) {
            return Some(format!("needs Studio {} or later", self.studio.min_version));
        }
        None
    }

    /// Structural checks. `exists` says whether a plugin-relative file is in
    /// the package.
    pub fn validate(&self, exists: &dyn Fn(&str) -> bool) -> Result<()> {
        let mut problems = Vec::new();
        let mut p = |s: String| problems.push(s);
        if self.schema != 1 {
            p(format!("schema {} is not supported", self.schema));
        }
        if !id_pattern().is_match(&self.id) {
            p(format!("id '{}' must be 3-41 lower-case letters, digits or dashes, starting with a letter", self.id));
        }
        if !version_pattern().is_match(&self.version) {
            p(format!("version '{}' must look like 1.2.3", self.version));
        }
        if self.name.trim().is_empty() {
            p("name is empty".into());
        }
        let icon = u32::from_str_radix(&self.icon, 16).unwrap_or(0);
        if !(0xE000..=0xF8FF).contains(&icon) {
            p(format!("icon '{}' must be a Segoe Fluent Icons code point (E000-F8FF)", self.icon));
        }
        if let Some(src) = &self.source
            && (src.github.split('/').count() != 2 || src.tag_prefix.is_empty())
        {
            p("source needs github: owner/repo and a tagPrefix".into());
        }

        let mut file = |what: &str, rel: &str, ext: Option<&str>| {
            if !safe_relative(rel) {
                p(format!("{what} '{rel}' must be a relative path inside the plugin"));
            } else if ext.is_some_and(|e| !rel.to_ascii_lowercase().ends_with(e)) {
                p(format!("{what} '{rel}' must be a {} file", ext.unwrap_or_default()));
            } else if !exists(rel) {
                p(format!("{what} '{rel}' is not in the package"));
            }
        };
        for s in &self.steps {
            file(&format!("step {} script", s.id), &s.script, Some(".ps1"));
        }
        if let Some(agent) = &self.agent {
            file("agent instructions", &agent.instructions, Some(".md"));
            for t in &agent.tasks {
                file(&format!("agent task {} prompt", t.id), &t.prompt, Some(".md"));
            }
            for r in &agent.references {
                if !safe_relative(r) {
                    p(format!("agent reference '{r}' must be a relative path inside the plugin"));
                }
            }
        }

        let mut keys = BTreeSet::new();
        for s in &self.settings {
            if !setting_key_pattern().is_match(&s.key) || !keys.insert(s.key.as_str()) {
                p(format!("setting '{}' is a duplicate or not a simple name", s.key));
            }
            if s.kind == SettingKind::Choice && s.choices.is_empty() {
                p(format!("setting '{}' is a choice with no choices", s.key));
            }
        }
        if !self.settings.iter().any(|s| s.key == WORK_ROOT_KEY && s.kind == SettingKind::Folder) {
            p(format!("a folder setting '{WORK_ROOT_KEY}' is required"));
        }

        // One id namespace for steps and agent tasks, so `after` can mix them.
        let tasks = self.agent.iter().flat_map(|a| &a.tasks);
        let mut ids = BTreeSet::new();
        for id in self.steps.iter().map(|s| &s.id).chain(tasks.clone().map(|t| &t.id)) {
            if !step_id_pattern().is_match(id) || !ids.insert(id.as_str()) {
                p(format!("step or task id '{id}' is a duplicate or not a simple name"));
            }
        }
        let graph: HashMap<&str, &[String]> =
            self.steps.iter().map(|s| (s.id.as_str(), s.after.as_slice())).chain(tasks.clone().map(|t| (t.id.as_str(), t.after.as_slice()))).collect();
        for (id, after) in &graph {
            for a in *after {
                if !graph.contains_key(a.as_str()) {
                    p(format!("'{id}' runs after unknown '{a}'"));
                }
            }
        }
        if let Some(id) = find_cycle(&graph) {
            p(format!("'{id}' depends on itself through 'after'"));
        }
        for t in tasks {
            for e in &t.evidence_from {
                if self.step(e).is_none() {
                    p(format!("agent task {} takes evidence from unknown step '{e}'", t.id));
                }
            }
        }

        for s in &self.steps {
            match &s.gate {
                Gate::Hash { file, summary } => {
                    for f in std::iter::once(file).chain(summary) {
                        if !safe_relative(f) {
                            p(format!("step {} gate file '{f}' must be a relative path", s.id));
                        }
                    }
                }
                _ if s.risk.needs_hash_gate() => p(format!("step {} ({}) must have a hash gate", s.id, s.risk.label())),
                _ => {}
            }
        }

        let plugin_step = |field: &str, id: &Option<String>, p: &mut dyn FnMut(String)| {
            if let Some(id) = id {
                match self.step(id) {
                    Some(s) if s.scope == Scope::Plugin => {}
                    Some(_) => p(format!("page {field} step '{id}' must have scope: plugin")),
                    None => p(format!("page {field} refers to unknown step '{id}'")),
                }
            }
        };
        plugin_step("importer", &self.page.importer, &mut p);
        plugin_step("probe", &self.page.probe, &mut p);
        plugin_step("query", &self.page.query, &mut p);
        for id in &self.page.pipeline {
            if self.task(id).is_none() && !self.step(id).is_some_and(|s| s.scope == Scope::Workitem) {
                p(format!("page pipeline lists '{id}', which is not an agent task or work-item step"));
            }
        }
        if self.page.kind == PageKind::Catalog && self.page.query.is_none() {
            p("a catalog page needs a query step".into());
        }
        for panel in &self.page.panels {
            let rel = panel.file.iter().chain(&panel.globs);
            if rel.clone().any(|f| !safe_relative(f)) {
                p(format!("panel '{}' paths must be relative", panel.title));
            }
            if matches!(panel.kind, PanelKind::Table | PanelKind::Markdown) && panel.file.is_none() {
                p(format!("panel '{}' needs a file", panel.title));
            }
        }

        for a in &self.actions {
            match &a.kind {
                ActionKind::Step { step } if self.step(step).is_none() => p(format!("action {} runs unknown step '{step}'", a.id)),
                ActionKind::HandoffWorkItem { plugin, files, .. } => {
                    if !self.requires.plugins.iter().any(|r| &r.id == plugin) {
                        p(format!("action {} hands off to '{plugin}', which is not in requires.plugins", a.id));
                    }
                    if files.iter().any(|f| !safe_relative(f)) {
                        p(format!("action {} file paths must be relative", a.id));
                    }
                }
                ActionKind::HandoffRecipe { notes_file } if !safe_relative(notes_file) => p(format!("action {} notesFile must be relative", a.id)),
                _ => {}
            }
        }

        if problems.is_empty() { Ok(()) } else { bail!("{} is not valid:\n- {}", self.id, problems.join("\n- ")) }
    }
}

fn id_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{2,40}$").unwrap())
}

fn version_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\d+\.\d+\.\d+$").unwrap())
}

fn setting_key_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9]{0,40}$").unwrap())
}

fn step_id_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{0,40}$").unwrap())
}

fn guid_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$").unwrap())
}

/// Relative, no drive, no root, no `..`.
pub fn safe_relative(rel: &str) -> bool {
    !rel.trim().is_empty()
        && !rel.contains(':')
        && !rel.starts_with(['/', '\\'])
        && rel.split(['/', '\\']).all(|c| !c.trim().is_empty() && c != ".." && c != ".")
}

fn find_cycle<'a>(graph: &HashMap<&'a str, &'a [String]>) -> Option<&'a str> {
    // 0 = unvisited, 1 = on the stack, 2 = done.
    fn visit<'a>(id: &'a str, graph: &HashMap<&'a str, &'a [String]>, mark: &mut HashMap<&'a str, u8>) -> bool {
        match mark.get(id) {
            Some(1) => return true,
            Some(2) => return false,
            _ => {}
        }
        mark.insert(id, 1);
        for next in graph.get(id).copied().unwrap_or_default() {
            if let Some((k, _)) = graph.get_key_value(next.as_str())
                && visit(k, graph, mark)
            {
                return true;
            }
        }
        mark.insert(id, 2);
        false
    }
    let mut mark = HashMap::new();
    let mut ids: Vec<&str> = graph.keys().copied().collect();
    ids.sort_unstable();
    ids.into_iter().find(|id| visit(id, graph, &mut mark))
}

// ---------------------------------------------------------------- packages

/// A plugin zip read into memory and validated, ready for the trust prompt.
#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: Manifest,
    /// SHA-256 of the zip file itself.
    pub sha256: String,
    files: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: u64,
}

const LIMITS: Limits = Limits { entries: 2000, bytes: 50 * 1024 * 1024 };

impl Package {
    pub fn open(zip: &Path) -> Result<Self> {
        let bytes = std::fs::read(zip).with_context(|| format!("reading {}", zip.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::read(bytes, LIMITS)
    }

    fn read(bytes: &[u8], limits: Limits) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a zip file")?;
        if zip.len() > limits.entries {
            bail!("the package has {} entries (the limit is {})", zip.len(), limits.entries);
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            if entry.is_symlink() {
                bail!("the package contains a link ({}), which is not allowed", entry.name().unwrap_or_default());
            }
            let Some(rel) = entry.enclosed_name() else { bail!("the package contains an unsafe path ({})", entry.name().unwrap_or_default()) };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let ext = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
            if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
                bail!("the package contains {rel}; plugins may only hold scripts, text and images");
            }
            let mut data = Vec::new();
            entry.by_ref().take(limits.bytes - total + 1).read_to_end(&mut data)?;
            total += data.len() as u64;
            if total > limits.bytes {
                bail!("the package unpacks to more than {} MB", limits.bytes / (1024 * 1024));
            }
            files.push((rel, data));
        }
        // Accept a zip whose files sit in one top-level folder.
        if !files.iter().any(|(n, _)| n == MANIFEST)
            && let Some((top, _)) = files.first().and_then(|(n, _)| n.split_once('/'))
        {
            let prefix = format!("{top}/");
            if files.iter().all(|(n, _)| n.starts_with(&prefix)) {
                for (n, _) in &mut files {
                    *n = n[prefix.len()..].to_string();
                }
            }
        }
        let manifest_bytes = files.iter().find(|(n, _)| n == MANIFEST).map(|(_, b)| b).ok_or_else(|| anyhow!("the package has no {MANIFEST}"))?;
        let manifest = Manifest::parse(&String::from_utf8_lossy(manifest_bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(manifest_bytes)))?;
        let names: BTreeSet<String> = files.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        manifest.validate(&|rel| names.contains(&rel.replace('\\', "/").to_ascii_lowercase()))?;
        Ok(Self { manifest, sha256: sha256_bytes(bytes), files })
    }

    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|(n, _)| n.as_str())
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|(_, b)| b.len() as u64).sum()
    }
}

/// Packs a plugin folder into a zip, files in name order with fixed
/// timestamps so the same folder always gives the same SHA-256.
pub fn zip_dir(dir: &Path) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).last_modified_time(zip::DateTime::default());
    let mut files = Vec::new();
    walk_files(dir, dir, &mut |rel, path| files.push((rel.to_string(), path.to_path_buf())));
    for (rel, path) in files {
        if rel == INSTALL_RECORD {
            continue;
        }
        w.start_file(rel, opts)?;
        w.write_all(&std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)?;
    }
    Ok(w.finish()?.into_inner())
}

/// Written next to an installed plugin so later edits are noticed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    pub id: String,
    pub version: String,
    pub package_sha256: String,
    /// The zip path or release URL it came from.
    pub source: String,
    pub installed_utc: String,
    /// Plugin-relative path (forward slashes) to SHA-256.
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// Files changed, added or removed since install. Steps refuse to run.
    Modified(Vec<String>),
    /// No install record (copied in by hand). Steps refuse to run.
    Unrecorded,
    Incompatible(String),
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    pub root: PathBuf,
    pub record: Option<InstallRecord>,
    pub health: Health,
}

impl Installed {
    pub fn load(root: &Path, studio_version: &str) -> Result<Self> {
        let manifest = Manifest::load(root)?;
        let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !folder.eq_ignore_ascii_case(&manifest.id) {
            bail!("folder '{folder}' holds plugin '{}'", manifest.id);
        }
        manifest.validate(&|rel| root.join(rel).is_file())?;
        let record: Option<InstallRecord> = crate::read_text(&root.join(INSTALL_RECORD)).ok().and_then(|t| serde_json::from_str(&t).ok());
        let health = if let Some(why) = manifest.incompatibility(studio_version) {
            Health::Incompatible(why)
        } else {
            match &record {
                None => Health::Unrecorded,
                Some(rec) => {
                    let changed = integrity_problems(root, rec);
                    if changed.is_empty() { Health::Ok } else { Health::Modified(changed) }
                }
            }
        };
        Ok(Self { manifest, root: root.to_path_buf(), record, health })
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Steps and agent runs need an intact, compatible plugin.
    pub fn ensure_runnable(&self) -> Result<()> {
        match &self.health {
            Health::Ok => Ok(()),
            Health::Modified(files) => bail!(
                "{} has been changed since it was installed ({}). Reinstall it before running its steps.",
                self.manifest.name,
                files.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ),
            Health::Unrecorded => bail!("{} was not installed by the Studio. Install it from its package before running its steps.", self.manifest.name),
            Health::Incompatible(why) => bail!("{} {why}.", self.manifest.name),
        }
    }
}

fn integrity_problems(root: &Path, rec: &InstallRecord) -> Vec<String> {
    let mut found = BTreeMap::new();
    walk_files(root, root, &mut |rel, path| {
        if rel != INSTALL_RECORD {
            found.insert(rel.to_string(), sha256_file(path).unwrap_or_default());
        }
    });
    let mut problems = Vec::new();
    for (rel, sha) in &rec.files {
        match found.get(rel) {
            None => problems.push(format!("{rel} missing")),
            Some(actual) if actual != sha => problems.push(format!("{rel} changed")),
            _ => {}
        }
    }
    problems.extend(found.keys().filter(|k| !rec.files.contains_key(*k)).map(|k| format!("{k} added")));
    problems
}

fn walk_files(root: &Path, dir: &Path, f: &mut dyn FnMut(&str, &Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            walk_files(root, &path, f);
        } else if let Ok(rel) = path.strip_prefix(root) {
            f(&rel.to_string_lossy().replace('\\', "/"), &path);
        }
    }
}

/// The installed plugins in a folder.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub dir: PathBuf,
    pub plugins: Vec<Installed>,
    /// Folders that hold something the Studio couldn't load, with the reason.
    pub broken: Vec<(PathBuf, String)>,
}

impl Registry {
    pub fn discover(dir: &Path, studio_version: &str) -> Self {
        let mut reg = Self { dir: dir.to_path_buf(), ..Default::default() };
        let Ok(rd) = std::fs::read_dir(dir) else { return reg };
        let mut folders: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
        folders.sort();
        for root in folders {
            if root.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            match Installed::load(&root, studio_version) {
                Ok(p) => reg.plugins.push(p),
                Err(e) => reg.broken.push((root, format!("{e:#}"))),
            }
        }
        reg
    }

    pub fn get(&self, id: &str) -> Option<&Installed> {
        self.plugins.iter().find(|p| p.manifest.id == id)
    }

    /// Required plugins that are missing or too old, as readable reasons.
    pub fn missing_requirements(&self, plugin: &Installed) -> Vec<String> {
        plugin
            .manifest
            .requires
            .plugins
            .iter()
            .filter_map(|r| match self.get(&r.id) {
                None => Some(format!("needs the {} plugin", r.id)),
                Some(p) if !r.min_version.is_empty() && is_newer(&r.min_version, &p.manifest.version) => {
                    Some(format!("needs {} {} or later (installed: {})", r.id, r.min_version, p.manifest.version))
                }
                _ => None,
            })
            .collect()
    }
}

/// Installs or updates a validated package, keeping the replaced version in
/// `.previous`. Reinstalling the same version repairs a modified plugin.
pub fn install(pkg: &Package, dir: &Path, studio_version: &str, source: &str, allow_downgrade: bool) -> Result<Installed> {
    let m = &pkg.manifest;
    if let Some(why) = m.incompatibility(studio_version) {
        bail!("{} {} {why}.", m.name, m.version);
    }
    let target = dir.join(&m.id);
    if let Ok(current) = Manifest::load(&target)
        && is_newer(&current.version, &m.version)
        && !allow_downgrade
    {
        bail!("{} {} is installed; this package is the older {}.", m.name, current.version, m.version);
    }
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let staging = dir.join(STAGING).join(format!("{}-{}-{nanos}", m.id, std::process::id()));
    let result = (|| -> Result<Installed> {
        let mut record = InstallRecord {
            id: m.id.clone(),
            version: m.version.clone(),
            package_sha256: pkg.sha256.clone(),
            source: source.to_string(),
            installed_utc: crate::intake::timestamp(),
            files: BTreeMap::new(),
        };
        for (rel, data) in &pkg.files {
            let path = staging.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
            record.files.insert(rel.clone(), sha256_bytes(data));
        }
        std::fs::write(staging.join(INSTALL_RECORD), serde_json::to_string_pretty(&record)?)?;

        let previous = dir.join(PREVIOUS).join(&m.id);
        if target.exists() {
            if previous.exists() {
                std::fs::remove_dir_all(&previous).with_context(|| format!("removing {}", previous.display()))?;
            }
            std::fs::create_dir_all(dir.join(PREVIOUS))?;
            std::fs::rename(&target, &previous).with_context(|| format!("moving the installed {} aside (is a file in it open?)", m.id))?;
        }
        if let Err(e) = std::fs::rename(&staging, &target) {
            if previous.exists() && !target.exists() {
                let _ = std::fs::rename(&previous, &target);
            }
            return Err(e).with_context(|| format!("installing {}", m.id));
        }
        Installed::load(&target, studio_version)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Removes an installed plugin. Its settings and work items are kept.
pub fn uninstall(dir: &Path, id: &str) -> Result<()> {
    if !id_pattern().is_match(id) {
        bail!("'{id}' is not a plugin id");
    }
    let target = dir.join(id);
    if target.exists() {
        std::fs::remove_dir_all(&target).with_context(|| format!("removing {} (is a file in it open?)", target.display()))?;
    }
    let previous = dir.join(PREVIOUS).join(id);
    if previous.exists() {
        let _ = std::fs::remove_dir_all(previous);
    }
    Ok(())
}

// ---------------------------------------------------------------- releases

pub fn releases_api(source: &Source) -> String {
    format!("https://api.github.com/repos/{}/releases?per_page=50", source.github)
}

/// The plugin's releases in a GitHub release list, newest first. Drafts,
/// pre-releases and releases without a `<id>-<version>.zip` are skipped.
pub fn parse_releases(json: &str, id: &str, source: &Source) -> Result<Vec<Release>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing the release list")?;
    let mut out = Vec::new();
    for r in v.as_array().into_iter().flatten() {
        let Some(version) = r["tag_name"].as_str().and_then(|t| t.strip_prefix(source.tag_prefix.as_str())) else { continue };
        if r["draft"].as_bool() == Some(true) || r["prerelease"].as_bool() == Some(true) || !version_pattern().is_match(version) {
            continue;
        }
        let name = format!("{id}-{version}.zip");
        let Some(asset) = r["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&name))) else {
            continue;
        };
        let Some(url) = asset["browser_download_url"].as_str() else { continue };
        out.push(Release {
            version: version.to_string(),
            notes: r["body"].as_str().unwrap_or_default().to_string(),
            html_url: r["html_url"].as_str().unwrap_or_default().to_string(),
            asset_name: name,
            asset_url: url.to_string(),
            size: asset["size"].as_u64().unwrap_or(0),
            sha256: crate::update::asset_sha256(asset),
        });
    }
    out.sort_by(|a, b| if is_newer(&a.version, &b.version) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater });
    Ok(out)
}

/// The newest release of a plugin from its source (network).
pub fn latest_release(id: &str, source: &Source) -> Result<Option<Release>> {
    let body = crate::update::get_text(&releases_api(source))?;
    Ok(parse_releases(&body, id, source)?.into_iter().next())
}

// ---------------------------------------------------------------- work items

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepState {
    #[default]
    NotRun,
    Running,
    Ok,
    Warn,
    Fail,
    Blocked,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StepRecord {
    pub state: StepState,
    pub started: String,
    pub finished: String,
    pub summary: String,
    pub result: Option<PathBuf>,
    pub approval: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Origin {
    /// `text`, `files`, `import`, `handoff` or `existing` (a folder made before the Studio).
    pub kind: String,
    pub from_plugin: String,
    /// The source's own key: a ticket number, an SCCM CI unique id...
    pub key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkItemMeta {
    pub schema: u32,
    pub title: String,
    pub reference: String,
    pub created: String,
    pub origin: Origin,
    pub steps: BTreeMap<String, StepRecord>,
}

/// One unit of work: a folder under the plugin's work root.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub dir: PathBuf,
    pub meta: WorkItemMeta,
}

impl WorkItem {
    /// Creates `<work_root>\<folder pattern>`. Refuses an existing folder.
    pub fn create(work_root: &Path, pattern: &str, title: &str, reference: &str, origin: Origin, now: SystemTime) -> Result<Self> {
        if title.trim().is_empty() {
            bail!("A title is needed.");
        }
        let name = folder_name(pattern, &crate::intake::utc_timestamp(now)[..10], reference, title);
        let dir = work_root.join(&name);
        if dir.exists() {
            bail!("{} already exists.", dir.display());
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let item = Self {
            dir,
            meta: WorkItemMeta {
                schema: 1,
                title: title.trim().to_string(),
                reference: reference.trim().to_string(),
                created: crate::intake::utc_timestamp(now),
                origin,
                steps: BTreeMap::new(),
            },
        };
        item.save()?;
        Ok(item)
    }

    /// Reads `work-item.json`, or describes a folder made before the Studio.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(WORK_ITEM_FILE);
        let meta = match crate::read_text(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
            Err(_) => WorkItemMeta {
                schema: 1,
                title: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                origin: Origin { kind: "existing".into(), ..Default::default() },
                ..Default::default()
            },
        };
        Ok(Self { dir: dir.to_path_buf(), meta })
    }

    /// Work items under `work_root`, newest folder name first. Folders count
    /// if they have `work-item.json`, or `ticket.md`/`recipes` from before.
    pub fn list(work_root: &Path) -> Vec<Self> {
        let Ok(rd) = std::fs::read_dir(work_root) else { return Vec::new() };
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .filter(|d| d.join(WORK_ITEM_FILE).is_file() || d.join("ticket.md").is_file() || d.join("recipes").is_dir())
            .collect();
        dirs.sort();
        dirs.reverse();
        dirs.iter().filter_map(|d| Self::open(d).ok()).collect()
    }

    pub fn save(&self) -> Result<()> {
        let path = self.dir.join(WORK_ITEM_FILE);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.meta)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))?;
        Ok(())
    }

    pub fn name(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn step(&self, id: &str) -> StepRecord {
        self.meta.steps.get(id).cloned().unwrap_or_default()
    }

    /// Whether everything `ids` names has finished well enough to go on.
    pub fn satisfied(&self, ids: &[String]) -> bool {
        ids.iter().all(|id| matches!(self.step(id).state, StepState::Ok | StepState::Warn))
    }
}

/// `{date}-{ref}-{slug}` with empty parts and their dashes dropped.
pub fn folder_name(pattern: &str, date: &str, reference: &str, title: &str) -> String {
    let filled = pattern.replace("{date}", date).replace("{ref}", &slug(reference, true)).replace("{slug}", &slug(title, false));
    let mut out = String::new();
    for part in filled.split('-').filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(part);
    }
    out
}

/// Letters, digits and dashes; references keep their case (`TASK1430821`).
fn slug(s: &str, keep_case: bool) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(if keep_case { c } else { c.to_ascii_lowercase() });
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(48).collect::<String>().trim_end_matches('-').to_string()
}

/// Who holds a work item while a step or agent runs there. A file, because
/// the work root may be a share used from several PCs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub host: String,
    pub user: String,
    pub pid: u32,
    pub since: String,
    pub purpose: String,
}

/// Removes the lock file when dropped.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire(dir: &Path, purpose: &str) -> Result<Self> {
        let path = dir.join(LOCK_FILE);
        let info = LockInfo { host: host_name(), user: user_name(), pid: std::process::id(), since: crate::intake::timestamp(), purpose: purpose.to_string() };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(serde_json::to_string_pretty(&info)?.as_bytes())?;
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match Self::holder(dir) {
                Some(h) => bail!("{} is in use: {} by {} on {} since {} UTC.", dir.display(), h.purpose, h.user, h.host, h.since),
                None => bail!("{} is in use (see {}).", dir.display(), path.display()),
            },
            Err(e) => Err(e).with_context(|| format!("locking {}", dir.display())),
        }
    }

    pub fn holder(dir: &Path) -> Option<LockInfo> {
        crate::read_text(&dir.join(LOCK_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    /// Clears a lock left behind by a crash.
    pub fn force_release(dir: &Path) -> Result<()> {
        let path = dir.join(LOCK_FILE);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn user_name() -> String {
    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) if !d.is_empty() => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- approvals

/// A user's approval of exactly one version of a gated file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub step: String,
    /// Work-item-relative gate file.
    pub file: String,
    pub sha256: String,
    pub approved_utc: String,
    pub user: String,
    pub host: String,
    pub plugin_id: String,
    pub plugin_version: String,
    pub package_sha256: String,
    #[serde(default)]
    pub summary_sha256: Option<String>,
    /// Set when a step run used the approval; it can't be used again.
    #[serde(default)]
    pub consumed_utc: Option<String>,
}

/// Records the user's approval of the gate file they were shown, refusing if
/// it changed since (`shown_sha256`).
pub fn grant(dir: &Path, plugin: &Installed, step: &StepDef, shown_sha256: &str, now: SystemTime) -> Result<(PathBuf, Approval)> {
    let Gate::Hash { file, summary } = &step.gate else { bail!("{} has no approval gate.", step.label()) };
    let sha = sha256_file(&dir.join(file)).with_context(|| format!("reading {file}"))?;
    if !sha.eq_ignore_ascii_case(shown_sha256) {
        bail!("{file} changed while you were reviewing it. Review it again.");
    }
    let approval = Approval {
        step: step.id.clone(),
        file: file.clone(),
        sha256: sha,
        approved_utc: crate::intake::utc_timestamp(now),
        user: user_name(),
        host: host_name(),
        plugin_id: plugin.manifest.id.clone(),
        plugin_version: plugin.manifest.version.clone(),
        package_sha256: plugin.record.as_ref().map(|r| r.package_sha256.clone()).unwrap_or_default(),
        summary_sha256: summary.as_ref().and_then(|s| sha256_file(&dir.join(s)).ok()),
        consumed_utc: None,
    };
    let folder = dir.join(APPROVALS_DIR);
    std::fs::create_dir_all(&folder)?;
    let path = folder.join(format!("{}-{}.json", step.id, file_stamp(now)));
    std::fs::write(&path, serde_json::to_string_pretty(&approval)?)?;
    Ok((path, approval))
}

/// The newest unused approval for a step.
pub fn open_grant(dir: &Path, step: &str) -> Option<(PathBuf, Approval)> {
    let rd = std::fs::read_dir(dir.join(APPROVALS_DIR)).ok()?;
    let mut found: Vec<(PathBuf, Approval)> = rd
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| crate::read_text(&p).ok().and_then(|t| serde_json::from_str::<Approval>(&t).ok()).map(|a| (p, a)))
        .filter(|(_, a)| a.step == step && a.consumed_utc.is_none())
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.pop()
}

fn consume(path: &Path, approval: &Approval) -> Result<()> {
    let mut used = approval.clone();
    used.consumed_utc = Some(crate::intake::timestamp());
    std::fs::write(path, serde_json::to_string_pretty(&used)?).with_context(|| format!("recording the use of {}", path.display()))
}

// ---------------------------------------------------------------- running steps

/// Studio facts passed to every step.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioContext {
    pub version: String,
    pub workspace: Option<PathBuf>,
    pub workspace_kind: String,
    pub production_paths: Vec<PathBuf>,
}

pub struct StepRequest<'a> {
    pub plugin: &'a Installed,
    pub step: &'a StepDef,
    /// The work item, or the work root for plugin-scope steps.
    pub dir: &'a Path,
    pub work_root: &'a Path,
    /// From [`resolve_settings`].
    pub settings: &'a Map<String, Value>,
    /// The page's own parameters (e.g. a catalog selection), merged over the manifest's.
    pub params: Map<String, Value>,
    /// The user confirmed a `confirm` gate.
    pub confirmed: bool,
}

#[derive(Debug)]
pub struct PreparedStep {
    pub spec: ProcessSpec,
    pub input: PathBuf,
    pub result: PathBuf,
    /// The approval this run used up.
    pub approval: Option<PathBuf>,
}

/// Checks everything a step needs, writes its `input.json`, and returns the
/// PowerShell command. A hash-gated step's approval is used up here.
pub fn prepare_step(req: StepRequest<'_>, studio: &StudioContext, pwsh: &Path, now: SystemTime) -> Result<PreparedStep> {
    let StepRequest { plugin, step, dir, work_root, settings, params, confirmed } = req;
    plugin.ensure_runnable()?;
    if !is_within(dir, work_root) {
        bail!("{} is outside the plugin's work folder {}.", dir.display(), work_root.display());
    }
    if let Some(prod) = studio.production_paths.iter().find(|p| is_within(dir, p) || is_within(p, work_root)) {
        bail!("Plugin steps never run in or around the production folder {}.", prod.display());
    }
    if !dir.is_dir() {
        bail!("{} does not exist.", dir.display());
    }
    let approval = match &step.gate {
        Gate::None => None,
        Gate::Confirm if confirmed => None,
        Gate::Confirm => bail!("{} needs your confirmation.", step.label()),
        Gate::Hash { file, .. } => {
            let (path, approval) = open_grant(dir, &step.id).ok_or_else(|| anyhow!("{} needs your approval of {file}.", step.label()))?;
            let now_sha = sha256_file(&dir.join(file)).with_context(|| format!("reading {file}"))?;
            if !now_sha.eq_ignore_ascii_case(&approval.sha256) || approval.file != *file {
                bail!("{file} changed after it was approved. Review and approve it again.");
            }
            consume(&path, &approval)?;
            Some((path, approval))
        }
    };

    let run = dir.join(STUDIO_DIR).join("runs").join(format!("{}-{}", file_stamp(now), step.id));
    std::fs::create_dir_all(&run).with_context(|| format!("creating {}", run.display()))?;
    let input = run.join("input.json");
    let result = run.join("result.json");
    let mut merged = step.params.clone();
    merged.extend(params);
    let doc = serde_json::json!({
        "schema": 1,
        "plugin": {
            "id": plugin.manifest.id,
            "version": plugin.manifest.version,
            "root": plugin.root,
            "packageSha256": plugin.record.as_ref().map(|r| r.package_sha256.clone()).unwrap_or_default(),
        },
        "step": step.id,
        "workItem": dir,
        "workRoot": work_root,
        "settings": settings,
        "params": merged,
        "studio": studio,
        "approval": approval.as_ref().map(|(_, a)| a),
    });
    std::fs::write(&input, serde_json::to_string_pretty(&doc)?).with_context(|| format!("writing {}", input.display()))?;

    let script = plugin.path(&step.script);
    let spec = ProcessSpec::new(format!("{} · {}", plugin.manifest.name, step.label()), pwsh, dir)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script.to_string_lossy())
        .arg("-StudioInput")
        .arg(input.to_string_lossy())
        .arg("-StudioResult")
        .arg(result.to_string_lossy())
        .env("NO_COLOR", "1");
    Ok(PreparedStep { spec, input, result, approval: approval.map(|(p, _)| p) })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Ok,
    Warn,
    #[default]
    Fail,
    Blocked,
}

impl ResultStatus {
    pub fn state(self) -> StepState {
        match self {
            ResultStatus::Ok => StepState::Ok,
            ResultStatus::Warn => StepState::Warn,
            ResultStatus::Fail => StepState::Fail,
            ResultStatus::Blocked => StepState::Blocked,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
    pub label: String,
}

/// What a step script reports in its `result.json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StepResult {
    pub status: ResultStatus,
    pub summary: String,
    /// Name to work-item-relative path.
    pub outputs: BTreeMap<String, String>,
    pub artifacts: Vec<Artifact>,
    /// Catalog rows or importer items.
    pub rows: Vec<Value>,
}

/// Reads a finished step's result. A non-zero exit or a missing result is a failure.
pub fn read_result(path: &Path, exit: Option<i32>) -> StepResult {
    let parsed: Option<StepResult> = crate::read_text(path).ok().and_then(|t| serde_json::from_str(&t).ok());
    match (parsed, exit) {
        (Some(r), Some(0)) => r,
        (Some(mut r), code) => {
            r.status = ResultStatus::Fail;
            if r.summary.is_empty() {
                r.summary = exit_text(code);
            }
            r
        }
        (None, Some(0)) => StepResult { summary: "The step finished but wrote no result.".into(), ..Default::default() },
        (None, code) => StepResult { summary: exit_text(code), ..Default::default() },
    }
}

fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("The step failed (exit code {c})."),
        None => "The step was stopped.".into(),
    }
}

/// Turns stored values into the settings passed to steps, applying defaults
/// and checking types. `{workspace}` is the open Development workspace.
pub fn resolve_settings(manifest: &Manifest, values: &BTreeMap<String, String>, workspace: Option<&Path>) -> std::result::Result<Map<String, Value>, Vec<String>> {
    let mut out = Map::new();
    let mut problems = Vec::new();
    for def in &manifest.settings {
        let stored = values.get(&def.key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let value = stored.or_else(|| {
            let d = def.default.clone()?;
            match workspace {
                Some(ws) => Some(d.replace("{workspace}", &ws.to_string_lossy())),
                None if d.contains("{workspace}") => None,
                None => Some(d),
            }
        });
        let Some(value) = value else {
            if def.required {
                problems.push(format!("{} is required.", def.label()));
            }
            continue;
        };
        let v = match def.kind {
            SettingKind::Bool => match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Value::Bool(true),
                "false" | "no" | "0" => Value::Bool(false),
                _ => {
                    problems.push(format!("{} must be true or false.", def.label()));
                    continue;
                }
            },
            SettingKind::Guid if !guid_pattern().is_match(&value) => {
                problems.push(format!("{} must be a GUID.", def.label()));
                continue;
            }
            SettingKind::Choice if !def.choices.contains(&value) => {
                problems.push(format!("{} must be one of {}.", def.label(), def.choices.join(", ")));
                continue;
            }
            SettingKind::Folder | SettingKind::File if !Path::new(&value).is_absolute() => {
                problems.push(format!("{} must be a full path.", def.label()));
                continue;
            }
            _ => Value::String(value),
        };
        out.insert(def.key.clone(), v);
    }
    if problems.is_empty() { Ok(out) } else { Err(problems) }
}

/// The work root from resolved settings.
pub fn work_root(settings: &Map<String, Value>) -> Option<PathBuf> {
    settings.get(WORK_ROOT_KEY).and_then(Value::as_str).map(PathBuf::from)
}

// ---------------------------------------------------------------- helpers

/// `child` is `parent` or inside it (case-insensitive, after resolving links).
pub fn is_within(child: &Path, parent: &Path) -> bool {
    let c = crate::normalize_path(child);
    let p = crate::normalize_path(parent);
    c == p || c.strip_prefix(&p).is_some_and(|rest| rest.starts_with('\\') || p.ends_with('\\'))
}

pub fn sha256_bytes(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `<work_root>`-relative paths (forward slashes) under `dir` matching any of
/// `globs` (`*` within a name, `**` across folders). Studio scratch is skipped.
pub fn glob_files(dir: &Path, globs: &[String]) -> Vec<String> {
    let patterns: Vec<Regex> = globs.iter().filter_map(|g| Regex::new(&glob_regex(g)).ok()).collect();
    let mut out = Vec::new();
    walk_files(dir, dir, &mut |rel, _| {
        if !rel.starts_with(STUDIO_DIR) && patterns.iter().any(|p| p.is_match(rel)) {
            out.push(rel.to_string());
        }
    });
    out
}

fn glob_regex(glob: &str) -> String {
    let glob = glob.replace('\\', "/");
    let mut re = String::from("(?i)^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
}

/// Starts a work item in another plugin from files of this one, copying
/// `files` (files or folders, relative to `from`) into its `intake` folder.
#[allow(clippy::too_many_arguments)]
pub fn hand_off(target: &Manifest, target_root: &Path, from: &Path, files: &[String], title: &str, reference: &str, origin: Origin, now: SystemTime) -> Result<WorkItem> {
    for f in files {
        if !safe_relative(f) || !from.join(f).exists() {
            bail!("{f} is missing from {}.", from.display());
        }
    }
    let item = WorkItem::create(target_root, &target.page.folder, title, reference, origin, now)?;
    let intake = item.dir.join("intake");
    for f in files {
        let src = from.join(f);
        let dest = intake.join(f);
        if src.is_dir() {
            walk_files(&src, &src, &mut |rel, path| {
                let to = dest.join(rel);
                if let Some(parent) = to.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::copy(path, to);
            });
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dest).with_context(|| format!("copying {f}"))?;
        }
    }
    Ok(item)
}

/// `20261113-201500` (UTC) plus milliseconds, for names that sort by time.
fn file_stamp(t: SystemTime) -> String {
    let s = crate::intake::utc_timestamp(t);
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    let ms = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_millis()).unwrap_or(0);
    format!("{}-{}{ms:03}", &digits[..8], &digits[8..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::time::Duration;

    const VERSION: &str = "0.6.0";

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("plugins").join("sample")
    }

    fn fixture_manifest() -> String {
        std::fs::read_to_string(fixture().join(MANIFEST)).unwrap().replace("\r\n", "\n")
    }

    /// Zips `dir` with an optional top folder, letting a test swap the manifest.
    fn zip_dir(dir: &Path, top: Option<&str>, manifest: Option<&str>, extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        let prefix = top.map(|t| format!("{t}/")).unwrap_or_default();
        walk_files(dir, dir, &mut |rel, path| {
            let data = match manifest {
                Some(m) if rel == MANIFEST => m.as_bytes().to_vec(),
                _ => std::fs::read(path).unwrap(),
            };
            w.start_file(format!("{prefix}{rel}"), opts).unwrap();
            w.write_all(&data).unwrap();
        });
        for (name, data) in extra {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn sample_package() -> Package {
        Package::from_bytes(&zip_dir(&fixture(), None, None, &[])).unwrap()
    }

    fn invalid(yaml: &str) -> String {
        let names = ["scripts/Echo.ps1", "agent/INSTRUCTIONS.md", "agent/tasks/triage.md"];
        let m = Manifest::parse(yaml).unwrap();
        format!("{:#}", m.validate(&|rel| names.contains(&rel)).unwrap_err())
    }

    #[test]
    fn sample_manifest_is_valid() {
        let m = Manifest::parse(&fixture_manifest()).unwrap();
        m.validate(&|rel| fixture().join(rel).is_file()).unwrap();
        assert_eq!(m.glyph(), "\u{E8F1}");
        assert_eq!(m.page.kind, PageKind::Workitems);
        assert_eq!(m.step("approve").unwrap().gate, Gate::Hash { file: "change.json".into(), summary: Some("change.md".into()) });
        assert_eq!(m.step("echo").unwrap().gate, Gate::None);
        assert!(m.incompatibility(VERSION).is_none());
        assert!(m.incompatibility("0.5.2").unwrap().contains("0.6.0"));
        let order: Vec<&str> = m.pipeline().iter().map(|s| s.id()).collect();
        assert_eq!(order, ["triage", "echo", "confirmed", "approve"]);
        let explicit = Manifest::parse(&fixture_manifest().replace("  intake: [text, files]", "  intake: [text, files]\n  pipeline: [confirmed, triage]")).unwrap();
        assert_eq!(explicit.pipeline().iter().map(|s| s.id()).collect::<Vec<_>>(), ["confirmed", "triage"]);
        assert!(invalid(&fixture_manifest().replace("  intake: [text, files]", "  intake: [text, files]\n  pipeline: [nope]")).contains("pipeline lists 'nope'"));
    }

    #[test]
    fn manifest_problems_are_reported() {
        let base = fixture_manifest();
        let cases = [
            ("script: scripts/Echo.ps1", "script: ../Echo.ps1", "relative path"),
            ("script: scripts/Echo.ps1", r"script: C:\x\Echo.ps1", "relative path"),
            ("script: scripts/Echo.ps1", "script: scripts/Echo.cmd", ".ps1"),
            ("script: scripts/Echo.ps1", "script: scripts/Missing.ps1", "not in the package"),
            ("after: [triage]", "after: [nothing]", "unknown 'nothing'"),
            ("after: [triage]", "after: [approve]", "depends on itself"),
            ("id: sample-plugin", "id: Sample", "id 'Sample'"),
            ("icon: E8F1", "icon: '0041'", "code point"),
            ("type: workitems", "type: gallery", "gallery"),
            ("key: workRoot", "key: root", "'workRoot' is required"),
            ("risk: tenant-write\n    gate: { kind: hash, file: change.json, summary: change.md }", "risk: tenant-write\n    gate: confirm", "must have a hash gate"),
            ("plugin: other-plugin", "plugin: stranger", "not in requires.plugins"),
        ];
        for (from, to, expect) in cases {
            assert!(base.contains(from), "fixture lacks {from}");
            let yaml = base.replacen(from, to, 1);
            let err = match Manifest::parse(&yaml) {
                Ok(m) => format!("{:#}", m.validate(&|rel| ["scripts/Echo.ps1", "agent/INSTRUCTIONS.md", "agent/tasks/triage.md"].contains(&rel)).unwrap_err()),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(expect), "{to}: {err}");
        }
        assert!(invalid(&base.replace("id: echo\n", "id: triage\n")).contains("duplicate"));
        let newer = Manifest::parse(&base.replace("hostApi: 1", "hostApi: 2")).unwrap();
        assert!(newer.incompatibility(VERSION).unwrap().contains("plugin API 2"));
    }

    #[test]
    fn relative_paths() {
        for ok in ["a.ps1", "scripts/a.ps1", r"scripts\a.ps1", "intune-upload/live-change-set.json"] {
            assert!(safe_relative(ok), "{ok}");
        }
        for bad in ["", "../a", r"..\a", "/a", r"\a", "C:/a", r"\\server\share\a", "a/../../b", "a//b"] {
            assert!(!safe_relative(bad), "{bad}");
        }
    }

    #[test]
    fn installs_updates_and_detects_tampering() {
        let d = tempfile::tempdir().unwrap();
        // A top-level folder in the zip is accepted.
        let pkg = Package::from_bytes(&zip_dir(&fixture(), Some("sample-plugin"), None, &[])).unwrap();
        assert_eq!(pkg.manifest.id, "sample-plugin");
        let packed = super::zip_dir(&fixture()).unwrap();
        assert_eq!(packed, super::zip_dir(&fixture()).unwrap(), "the same folder packs to the same bytes");
        assert_eq!(Package::from_bytes(&packed).unwrap().manifest.id, "sample-plugin");
        let inst = install(&pkg, d.path(), VERSION, "test.zip", false).unwrap();
        assert_eq!(inst.health, Health::Ok);
        assert!(inst.root.join("scripts/Echo.ps1").is_file());

        let reg = Registry::discover(d.path(), VERSION);
        assert_eq!(reg.plugins.len(), 1);
        assert!(reg.broken.is_empty());
        assert_eq!(reg.missing_requirements(&reg.plugins[0]), vec!["needs the other-plugin plugin"]);

        // An older package is refused; a newer one keeps the old copy aside.
        let older = fixture_manifest().replace("version: 1.0.0", "version: 0.9.0");
        let pkg_old = Package::from_bytes(&zip_dir(&fixture(), None, Some(&older), &[])).unwrap();
        assert!(install(&pkg_old, d.path(), VERSION, "x", false).unwrap_err().to_string().contains("older"));
        let newer = fixture_manifest().replace("version: 1.0.0", "version: 1.1.0");
        let pkg_new = Package::from_bytes(&zip_dir(&fixture(), None, Some(&newer), &[])).unwrap();
        assert_eq!(install(&pkg_new, d.path(), VERSION, "x", false).unwrap().manifest.version, "1.1.0");
        assert!(d.path().join(PREVIOUS).join("sample-plugin").join(MANIFEST).is_file());
        assert!(!d.path().join(STAGING).read_dir().unwrap().any(|_| true), "staging is cleaned up");

        // Edits after install are noticed and stop the steps.
        std::fs::write(inst.root.join("scripts/Echo.ps1"), "Write-Host 'changed'").unwrap();
        std::fs::write(inst.root.join("scripts/Extra.ps1"), "").unwrap();
        let tampered = Installed::load(&inst.root, VERSION).unwrap();
        let Health::Modified(files) = &tampered.health else { panic!("{:?}", tampered.health) };
        assert!(files.contains(&"scripts/Echo.ps1 changed".to_string()) && files.contains(&"scripts/Extra.ps1 added".to_string()), "{files:?}");
        assert!(tampered.ensure_runnable().is_err());
        // Reinstalling the same version repairs it.
        assert_eq!(install(&pkg_new, d.path(), VERSION, "x", false).unwrap().health, Health::Ok);

        uninstall(d.path(), "sample-plugin").unwrap();
        assert!(Registry::discover(d.path(), VERSION).plugins.is_empty());
        assert!(uninstall(d.path(), "..").is_err());
    }

    #[test]
    fn unsafe_packages_are_refused() {
        let slip = zip_dir(&fixture(), None, None, &[("../evil.ps1", b"x")]);
        assert!(Package::from_bytes(&slip).unwrap_err().to_string().contains("unsafe path"));
        let exe = zip_dir(&fixture(), None, None, &[("tool.exe", b"MZ")]);
        assert!(Package::from_bytes(&exe).unwrap_err().to_string().contains("tool.exe"));
        let bytes = zip_dir(&fixture(), None, None, &[]);
        assert!(Package::read(&bytes, Limits { entries: 2, bytes: LIMITS.bytes }).unwrap_err().to_string().contains("entries"));
        assert!(Package::read(&bytes, Limits { entries: 100, bytes: 64 }).unwrap_err().to_string().contains("MB"));
        assert!(Package::from_bytes(b"not a zip").is_err());
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("readme.md", zip::write::SimpleFileOptions::default()).unwrap();
        assert!(Package::from_bytes(&w.finish().unwrap().into_inner()).unwrap_err().to_string().contains("no plugin.yaml"));
        let incompatible = fixture_manifest().replace("hostApi: 1", "hostApi: 9");
        let pkg = Package::from_bytes(&zip_dir(&fixture(), None, Some(&incompatible), &[])).unwrap();
        assert!(install(&pkg, tempfile::tempdir().unwrap().path(), VERSION, "x", false).is_err());
    }

    #[test]
    fn work_items_are_named_listed_and_locked() {
        let d = tempfile::tempdir().unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_600_000);
        assert_eq!(folder_name("{date}-{ref}-{slug}", "2026-11-13", "TASK1430821", "Mplus 8.11 / Combo!"), "2026-11-13-TASK1430821-mplus-8-11-combo");
        assert_eq!(folder_name("{date}-{ref}-{slug}", "2026-11-13", "", "Zoom"), "2026-11-13-zoom");
        let item = WorkItem::create(d.path(), "{date}-{ref}-{slug}", "Zoom Workplace", "", Origin { kind: "text".into(), ..Default::default() }, t).unwrap();
        assert!(item.name().ends_with("-zoom-workplace"));
        assert!(WorkItem::create(d.path(), "{date}-{ref}-{slug}", "Zoom Workplace", "", Origin::default(), t).is_err());
        // A folder from before the Studio shows up too.
        std::fs::create_dir_all(d.path().join("2026-10-01-old").join("recipes")).unwrap();
        std::fs::create_dir_all(d.path().join("not-a-ticket")).unwrap();
        let list = WorkItem::list(d.path());
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].meta.origin.kind, "existing");

        let mut item = WorkItem::open(&item.dir).unwrap();
        item.meta.steps.insert("echo".into(), StepRecord { state: StepState::Warn, ..Default::default() });
        item.save().unwrap();
        let item = WorkItem::open(&item.dir).unwrap();
        assert!(item.satisfied(&["echo".into()]));
        assert!(!item.satisfied(&["echo".into(), "other".into()]));

        let lock = Lock::acquire(&item.dir, "Echo").unwrap();
        assert!(Lock::acquire(&item.dir, "Echo").unwrap_err().to_string().contains("in use"));
        assert_eq!(Lock::holder(&item.dir).unwrap().purpose, "Echo");
        drop(lock);
        assert!(Lock::acquire(&item.dir, "again").is_ok());
    }

    #[test]
    fn settings_resolve_with_defaults_and_types() {
        let m = Manifest::parse(&fixture_manifest()).unwrap();
        let mut values = BTreeMap::new();
        assert_eq!(resolve_settings(&m, &values, None).unwrap_err(), vec!["Work folder is required.".to_string()]);
        values.insert("workRoot".into(), r"C:\Work".into());
        values.insert("tenantId".into(), "nope".into());
        assert!(resolve_settings(&m, &values, None).unwrap_err()[0].contains("GUID"));
        values.insert("tenantId".into(), "22222222-2222-2222-2222-222222222222".into());
        let s = resolve_settings(&m, &values, Some(Path::new(r"C:\Intune\YardstickDev"))).unwrap();
        assert_eq!(s["yardstickDevPath"], r"C:\Intune\YardstickDev");
        assert_eq!(s["verbose"], true);
        assert!(!resolve_settings(&m, &values, None).unwrap().contains_key("yardstickDevPath"));
        assert_eq!(work_root(&s), Some(PathBuf::from(r"C:\Work")));
    }

    fn setup_item() -> (tempfile::TempDir, Installed, WorkItem, Map<String, Value>) {
        let d = tempfile::tempdir().unwrap();
        let inst = install(&sample_package(), &d.path().join("plugins"), VERSION, "test", false).unwrap();
        let root = d.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let item = WorkItem::create(&root, &inst.manifest.page.folder, "Echo test", "T1", Origin::default(), SystemTime::now()).unwrap();
        let mut settings = Map::new();
        settings.insert("workRoot".into(), Value::String(root.to_string_lossy().into()));
        (d, inst, item, settings)
    }

    fn request<'a>(inst: &'a Installed, step: &str, dir: &'a Path, root: &'a Path, settings: &'a Map<String, Value>) -> StepRequest<'a> {
        StepRequest { plugin: inst, step: inst.manifest.step(step).unwrap(), dir, work_root: root, settings, params: Map::new(), confirmed: false }
    }

    #[test]
    fn steps_get_paths_only_and_refuse_unsafe_folders() {
        let (d, inst, item, settings) = setup_item();
        let root = d.path().join("work");
        let studio = StudioContext { version: VERSION.into(), production_paths: vec![d.path().join("prod")], ..Default::default() };
        let req = |dir| request(&inst, "echo", dir, &root, &settings);
        let p = prepare_step(req(&item.dir), &studio, Path::new("pwsh.exe"), SystemTime::now()).unwrap();
        assert_eq!(p.spec.args[..5], ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]);
        assert!(p.spec.args[5].ends_with("Echo.ps1"));
        assert_eq!(p.spec.args[6], "-StudioInput");
        assert_eq!(p.spec.args.len(), 10, "only paths on the command line");
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&p.input).unwrap()).unwrap();
        assert_eq!(input["params"]["message"], "hello");
        assert_eq!(input["settings"]["workRoot"], root.to_string_lossy().as_ref());

        let outside = d.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        assert!(prepare_step(req(&outside), &studio, Path::new("pwsh"), SystemTime::now()).unwrap_err().to_string().contains("outside"));
        let prod = StudioContext { production_paths: vec![root.clone()], ..studio.clone() };
        assert!(prepare_step(req(&item.dir), &prod, Path::new("pwsh"), SystemTime::now()).unwrap_err().to_string().contains("production"));
        let r = request(&inst, "confirmed", &item.dir, &root, &settings);
        assert!(prepare_step(r, &studio, Path::new("pwsh"), SystemTime::now()).unwrap_err().to_string().contains("confirmation"));
    }

    #[test]
    fn approvals_bind_to_the_file_and_are_single_use() {
        let (d, inst, item, settings) = setup_item();
        let root = d.path().join("work");
        let studio = StudioContext::default();
        let step = inst.manifest.step("approve").unwrap();
        let req = || StepRequest { confirmed: true, ..request(&inst, "approve", &item.dir, &root, &settings) };
        assert!(prepare_step(req(), &studio, Path::new("pwsh"), SystemTime::now()).unwrap_err().to_string().contains("approval"));

        std::fs::write(item.dir.join("change.json"), r#"{"apps":["a"]}"#).unwrap();
        let shown = sha256_file(&item.dir.join("change.json")).unwrap();
        assert!(grant(&item.dir, &inst, step, &"0".repeat(64), SystemTime::now()).unwrap_err().to_string().contains("changed while"));
        let (_, a) = grant(&item.dir, &inst, step, &shown, SystemTime::now()).unwrap();
        assert_eq!(a.package_sha256, inst.record.as_ref().unwrap().package_sha256);

        // Changing the file after approval voids it.
        std::fs::write(item.dir.join("change.json"), r#"{"apps":["a","b"]}"#).unwrap();
        assert!(prepare_step(req(), &studio, Path::new("pwsh"), SystemTime::now()).unwrap_err().to_string().contains("changed after"));

        std::fs::write(item.dir.join("change.json"), r#"{"apps":["a"]}"#).unwrap();
        let p = prepare_step(req(), &studio, Path::new("pwsh"), SystemTime::now()).unwrap();
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&p.input).unwrap()).unwrap();
        assert_eq!(input["approval"]["sha256"], shown.as_str());
        assert!(open_grant(&item.dir, "approve").is_none(), "used up");
        assert!(prepare_step(req(), &studio, Path::new("pwsh"), SystemTime::now()).is_err());
    }

    #[test]
    fn results_are_read_strictly() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("result.json");
        assert_eq!(read_result(&path, Some(0)).status, ResultStatus::Fail);
        assert!(read_result(&path, Some(3)).summary.contains("exit code 3"));
        std::fs::write(&path, "\u{feff}{\"status\":\"warn\",\"summary\":\"Two warnings\",\"rows\":[{\"name\":\"a\"}]}").unwrap();
        let r = read_result(&path, Some(0));
        assert_eq!((r.status, r.summary.as_str(), r.rows.len()), (ResultStatus::Warn, "Two warnings", 1));
        assert_eq!(read_result(&path, Some(1)).status, ResultStatus::Fail);
        std::fs::write(&path, "{nope").unwrap();
        assert_eq!(read_result(&path, Some(0)).status, ResultStatus::Fail);
    }

    #[test]
    fn echo_step_runs_end_to_end() {
        let (d, inst, item, settings) = setup_item();
        let root = d.path().join("work");
        let req = request(&inst, "echo", &item.dir, &root, &settings);
        let pwsh = crate::harness::powershell_exe(None);
        let p = prepare_step(req, &StudioContext::default(), &pwsh, SystemTime::now()).unwrap();
        let mut proc = crate::process::RunningProcess::spawn(p.spec, None).unwrap();
        while proc.is_running() {
            std::thread::sleep(Duration::from_millis(20));
            proc.poll();
        }
        let r = read_result(&p.result, proc.exit.flatten());
        assert_eq!(r.status, ResultStatus::Ok, "{r:?} {:?}", proc.lines);
        assert_eq!(r.summary, "Echoed hello");
        assert_eq!(std::fs::read_to_string(item.dir.join("echo.txt")).unwrap().trim(), "hello");
    }

    #[test]
    fn plugin_releases_are_picked_by_prefix() {
        let src = Source { github: "o/r".into(), tag_prefix: "plugin-sample-plugin-v".into() };
        let json = r#"[
            {"tag_name":"v0.6.0","assets":[{"name":"YardstickDevelopmentStudio-0.6.0-x64.msi","browser_download_url":"u"}]},
            {"tag_name":"plugin-sample-plugin-v1.0.0","assets":[{"name":"sample-plugin-1.0.0.zip","browser_download_url":"https://x/1.zip","size":5,
              "digest":"sha256:0754B845C19D10AA20C7322A9CE68D4CB01BC30E7962D43C389B16AA007DF5BB"}]},
            {"tag_name":"plugin-sample-plugin-v1.2.0","assets":[{"name":"sample-plugin-1.2.0.zip","browser_download_url":"https://x/2.zip"}]},
            {"tag_name":"plugin-sample-plugin-v2.0.0","prerelease":true,"assets":[{"name":"sample-plugin-2.0.0.zip","browser_download_url":"https://x/3.zip"}]},
            {"tag_name":"plugin-sample-plugin-v1.5.0","assets":[]}
        ]"#;
        let r = parse_releases(json, "sample-plugin", &src).unwrap();
        assert_eq!(r.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["1.2.0", "1.0.0"]);
        assert_eq!(r[1].sha256.as_deref(), Some("0754b845c19d10aa20c7322a9ce68d4cb01bc30e7962d43c389b16aa007df5bb"));
        assert_eq!(releases_api(&src), "https://api.github.com/repos/o/r/releases?per_page=50");
    }

    #[test]
    fn repo_plugins_validate() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("plugins");
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten().filter(|e| e.path().join(MANIFEST).is_file()) {
            let root = e.path();
            let m = Manifest::load(&root).unwrap();
            m.validate(&|rel| root.join(rel).is_file()).unwrap();
            assert_eq!(m.id, e.file_name().to_string_lossy(), "folder name matches the id");
            assert!(m.incompatibility(env!("CARGO_PKG_VERSION")).is_none(), "{} needs a newer Studio than this one", m.id);
        }
    }

    #[test]
    fn globs_and_handoffs() {
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("from");
        for f in ["export.md", "a.txt", "artifacts/run1/summary.md", "source/setup.msi", ".studio/runs/x/input.json"] {
            std::fs::create_dir_all(from.join(f).parent().unwrap()).unwrap();
            std::fs::write(from.join(f), f).unwrap();
        }
        let globs = |g: &[&str]| glob_files(&from, &g.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(globs(&["*.md"]), ["export.md"]);
        assert_eq!(globs(&["artifacts/**/summary.md", "*.TXT"]), ["a.txt", "artifacts/run1/summary.md"]);
        assert!(globs(&["**/*.json"]).is_empty(), "Studio scratch is hidden");

        let target = Manifest::parse(&fixture_manifest()).unwrap();
        let origin = Origin { kind: "handoff".into(), from_plugin: "sccm-migration".into(), key: "ScopeId_1/Application_2".into() };
        let item = hand_off(&target, &d.path().join("work"), &from, &["export.md".into(), "source".into()], "Spectragryph 1.2", "", origin, SystemTime::now()).unwrap();
        assert!(item.dir.join("intake/export.md").is_file() && item.dir.join("intake/source/setup.msi").is_file());
        assert_eq!(WorkItem::open(&item.dir).unwrap().meta.origin.from_plugin, "sccm-migration");
        assert!(hand_off(&target, &d.path().join("work"), &from, &["missing.md".into()], "x", "", Origin::default(), SystemTime::now()).is_err());
    }

    #[test]
    fn within_compares_whole_components() {
        assert!(is_within(Path::new(r"C:\Work\Item"), Path::new(r"c:\work")));
        assert!(is_within(Path::new(r"C:\Work"), Path::new(r"C:\Work\")));
        assert!(!is_within(Path::new(r"C:\Workshop"), Path::new(r"C:\Work")));
        assert!(!is_within(Path::new(r"C:\Work"), Path::new(r"C:\Work\Item")));
    }
}
