//! Running one recipe through Yardstick on the tenant, and remembering the result.
//!
//! This is the part of the Studio that writes to Intune, so it is built in
//! layers, each enforced here rather than in the UI:
//!
//! 1. [`super::ensure_ready`]: a trusted full instance, the feature on, a tenant set.
//! 2. [`TenantConsent::Confirmed`], which only the per-run dialog produces.
//! 3. A fixed command line: `Yardstick.ps1 -ApplicationId <id> -NoEmail -NoDelete`
//!    (plus `-Force` only on request). Never `-All`, `-Group` or `-Repair`.
//! 4. A temporary entry for the recipe in `Local\recipeOverrides.yaml` that
//!    renames the app (`YDS TEST - <name>`), so Yardstick's version lookup,
//!    assignment migration and supersedence cannot reach a real app, forces the
//!    test groups over whatever the recipe says, and switches supersedence and
//!    auto-update off. The original file is backed up byte for byte and put back
//!    afterwards, and a marker lets the next Studio start put it back if this
//!    one died mid-run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

use super::{GroupInfo, TenantInfo, ensure_ready};
use crate::process::ProcessSpec;
use crate::recipe::{RecipeDoc, is_valid_id};
use crate::settings::TenantSettings;
use crate::workspace::Workspace;

/// The run script, compiled in so the file Studio runs is the file it shipped.
pub const RUN_SCRIPT: &str = include_str!("run.ps1");

const RUN_FILE: &str = "Studio-TenantRun.ps1";
const MARKER: &str = "restore.json";
const MARKER_DONE: &str = "restore.done.json";
const BACKUP: &str = "overrides.backup";

/// What the test app is called in Intune, ahead of the recipe's own name.
/// No brackets: PowerShell's `-like` reads `[...]` as a character class, and
/// Yardstick matches names with it.
pub const TEST_PREFIX: &str = "YDS TEST - ";

/// The name of the app a tenant run creates for a recipe called `display_name`.
pub fn test_display_name(display_name: &str) -> String {
    format!("{TEST_PREFIX}{}", display_name.trim())
}

/// The user confirmed a dialog describing exactly this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantConsent {
    None,
    Confirmed,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// `-Force`: upload even if Intune already has this version of the test app.
    pub force_reupload: bool,
}

/// A prepared run: the process to start and where it will write its result.
/// The overrides file has already been staged; whoever starts the process must
/// call [`restore_overrides`] on `dir` when it ends, or if it cannot be started.
pub struct PreparedRun {
    pub spec: ProcessSpec,
    pub result: PathBuf,
    pub dir: PathBuf,
    /// The name the test app will have in Intune.
    pub app_name: String,
}

/// `Local\recipeOverrides.yaml`, the site-specific overrides Yardstick reads.
pub fn overrides_path(ws: &Workspace) -> PathBuf {
    ws.root.join("Local").join("recipeOverrides.yaml")
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Builds the run: checks every gate, stages the overrides, writes the script
/// and its input, and returns the process. `base` is [`super::work_dir`].
#[allow(clippy::too_many_arguments)]
pub fn run_spec(
    ws: &Workspace,
    tenant: &TenantSettings,
    recipe_id: &str,
    options: RunOptions,
    resolved_groups: &[GroupInfo],
    consent: TenantConsent,
    pwsh: &Path,
    base: &Path,
) -> Result<PreparedRun> {
    ensure_ready(ws, tenant, "run a recipe on the tenant")?;
    if consent != TenantConsent::Confirmed {
        bail!("A tenant run uploads an app to Intune and needs explicit confirmation of this exact run.");
    }
    if !is_valid_id(recipe_id) {
        bail!("'{recipe_id}' is not a valid recipe id.");
    }
    let recipe = ws.development_recipe_path(recipe_id);
    if !recipe.is_file() {
        bail!("Only a recipe in Recipes\\Development can be run on the tenant, and '{recipe_id}' is not there.");
    }
    let doc = RecipeDoc::load(&recipe).with_context(|| format!("reading {}", recipe.display()))?;
    let Some(display_name) = doc.get_str("displayName").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        bail!("'{recipe_id}' has no displayName, so its test app cannot be named.");
    };
    let app_name = test_display_name(&display_name);
    let assign = tenant.after_run.assigns();
    if assign && tenant.test_groups.iter().all(|g| g.trim().is_empty()) {
        bail!("Name at least one test group under Settings → Tenant validation, or choose to upload without assignments.");
    }
    // Yardstick assigns `defaultDeploymentGroups` by id; Graph refuses a name.
    let groups = if assign { resolved_group_ids(tenant, resolved_groups)? } else { Vec::new() };

    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let dir = base.join("runs").join(format!("run-{stamp}-{recipe_id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let script = dir.join(RUN_FILE);
    let input = dir.join("input.json");
    let result = dir.join("result.json");
    std::fs::write(&script, RUN_SCRIPT).with_context(|| format!("writing {}", script.display()))?;
    let payload = serde_json::json!({
        "root": ws.root.to_string_lossy(),
        "tenantId": tenant.tenant_id.trim(),
        "tenantName": tenant.tenant_name.trim(),
        "recipeId": recipe_id,
        "testDisplayName": app_name,
        "testPrefix": TEST_PREFIX,
        "groups": groups,
        "assign": assign,
        "force": options.force_reupload,
        "pwsh": pwsh.to_string_lossy(),
    });
    std::fs::write(&input, serde_json::to_string_pretty(&payload)?).with_context(|| format!("writing {}", input.display()))?;

    // Last, so nothing above can fail with the overrides already changed.
    stage_overrides(ws, &dir, recipe_id, &app_name, &groups)?;

    let spec = ProcessSpec::new(format!("Tenant run: {recipe_id}"), pwsh, &ws.root)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script.to_string_lossy())
        .args(["-InputPath"])
        .arg(input.to_string_lossy())
        .args(["-ResultPath"])
        .arg(result.to_string_lossy())
        .env("NO_COLOR", "1");
    Ok(PreparedRun { spec, result, dir, app_name })
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The configured test groups as the ids the connection check resolved them to.
/// Yardstick passes `defaultDeploymentGroups` straight to Graph as group ids, so
/// a run is staged only with ids, and only when every configured group (a
/// display name or an id) resolved to exactly one group.
pub fn resolved_group_ids(tenant: &TenantSettings, resolved: &[GroupInfo]) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    for wanted in tenant.test_groups.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
        let Some(group) = resolved.iter().find(|g| g.input.trim().eq_ignore_ascii_case(wanted) || g.id.eq_ignore_ascii_case(wanted)) else {
            bail!("The test group '{wanted}' was not resolved to a group id, so nothing was uploaded. Check the tenant connection in Settings.");
        };
        if !is_guid(&group.id) {
            bail!("The test group '{wanted}' resolved to '{}', which is not a group id.", group.id);
        }
        if !ids.iter().any(|id| id.eq_ignore_ascii_case(&group.id)) {
            ids.push(group.id.clone());
        }
    }
    Ok(ids)
}

// ---- the temporary override ----------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    overrides_path: PathBuf,
    /// The overrides file existed before the run.
    existed: bool,
    original_sha: String,
    staged_sha: String,
}

/// Writes the recipe's temporary override entry, after saving the file as it was.
fn stage_overrides(ws: &Workspace, dir: &Path, recipe_id: &str, app_name: &str, groups: &[String]) -> Result<()> {
    let path = overrides_path(ws);
    let original = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut root = match &original {
        None => Mapping::new(),
        Some(bytes) => match serde_yaml::from_slice::<Value>(strip_bom(bytes)).with_context(|| format!("{} is not valid YAML, so a test override cannot be added", path.display()))? {
            Value::Null => Mapping::new(),
            Value::Mapping(m) => m,
            _ => bail!("{} is not a mapping of recipe ids, so a test override cannot be added.", path.display()),
        },
    };
    let existing_key = root.keys().find(|k| k.as_str().is_some_and(|s| s.eq_ignore_ascii_case(recipe_id))).cloned();
    let key = existing_key.unwrap_or_else(|| Value::String(recipe_id.to_string()));
    let mut entry = match root.get(&key) {
        None | Some(Value::Null) => Mapping::new(),
        Some(Value::Mapping(m)) => m.clone(),
        Some(_) => bail!("The override entry for '{recipe_id}' is not a mapping."),
    };
    if entry.contains_key("variantOf") {
        bail!("'{recipe_id}' is defined as a variant in the overrides file, which a tenant run does not support.");
    }
    entry.insert("displayName".into(), Value::String(app_name.to_string()));
    entry.insert("defaultDeploymentGroups".into(), Value::Sequence(groups.iter().cloned().map(Value::String).collect()));
    entry.insert("supersedence".into(), Value::Bool(false));
    entry.insert("autoUpdateOnAssignment".into(), Value::Bool(false));
    root.insert(key, Value::Mapping(entry));
    let staged = serde_yaml::to_string(&Value::Mapping(root))?;

    let backup = dir.join(BACKUP);
    if let Some(bytes) = &original {
        std::fs::write(&backup, bytes).with_context(|| format!("writing {}", backup.display()))?;
    }
    let marker = Marker {
        overrides_path: path.clone(),
        existed: original.is_some(),
        original_sha: original.as_deref().map(sha_hex).unwrap_or_default(),
        staged_sha: sha_hex(staged.as_bytes()),
    };
    // The marker goes first: if the write below is interrupted, restoring is still safe.
    std::fs::write(dir.join(MARKER), serde_json::to_string_pretty(&marker)?).with_context(|| format!("writing {}", dir.join(MARKER).display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, staged.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restored {
    /// There was nothing staged, or it was already put back.
    NothingToDo,
    /// The overrides file is exactly as it was before the run.
    Restored,
    /// The file was edited since it was staged, so it was left alone.
    LeftAlone(String),
}

/// Puts `Local\recipeOverrides.yaml` back as it was before the run in `dir`.
/// Safe to call more than once. It only writes when the file is still exactly
/// what the run staged, so it never overwrites an edit made since.
pub fn restore_overrides(dir: &Path) -> Result<Restored> {
    let marker_path = dir.join(MARKER);
    let Ok(text) = std::fs::read_to_string(&marker_path) else { return Ok(Restored::NothingToDo) };
    let marker: Marker = serde_json::from_str(&text).with_context(|| format!("reading {}", marker_path.display()))?;
    let current = match std::fs::read(&marker.overrides_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", marker.overrides_path.display())),
    };
    let current_sha = current.as_deref().map(sha_hex).unwrap_or_default();
    let outcome = if current_sha == marker.staged_sha {
        if marker.existed {
            let backup = dir.join(BACKUP);
            let bytes = std::fs::read(&backup).with_context(|| format!("reading {}", backup.display()))?;
            std::fs::write(&marker.overrides_path, bytes).with_context(|| format!("writing {}", marker.overrides_path.display()))?;
        } else {
            std::fs::remove_file(&marker.overrides_path).with_context(|| format!("removing {}", marker.overrides_path.display()))?;
        }
        Restored::Restored
    } else if marker.existed == current.is_some() && current_sha == marker.original_sha {
        Restored::NothingToDo
    } else {
        Restored::LeftAlone(format!(
            "{} was changed after the tenant run staged its temporary entry, so it was left as it is. The original is saved at {}.",
            marker.overrides_path.display(),
            dir.join(BACKUP).display()
        ))
    };
    let _ = std::fs::rename(&marker_path, dir.join(MARKER_DONE));
    Ok(outcome)
}

/// At startup: puts back the overrides of any run the Studio did not get to
/// finish (it was closed or killed mid-run). Returns a message for each run
/// that needed attention.
pub fn recover_interrupted(base: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    let Ok(runs) = std::fs::read_dir(base.join("runs")) else { return notes };
    for entry in runs.flatten() {
        let dir = entry.path();
        if !dir.join(MARKER).is_file() {
            continue;
        }
        match restore_overrides(&dir) {
            Ok(Restored::Restored) => notes.push("Restored Local\\recipeOverrides.yaml after a tenant run that did not finish.".to_string()),
            Ok(Restored::LeftAlone(why)) => notes.push(why),
            Ok(Restored::NothingToDo) => {}
            Err(e) => notes.push(format!("Could not restore Local\\recipeOverrides.yaml after a tenant run: {e:#}")),
        }
    }
    notes
}

// ---- the result ------------------------------------------------------------------

/// PowerShell writes `null` for anything it has not set yet; read that as the default.
fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Assignment {
    #[serde(deserialize_with = "null_default")]
    pub intent: String,
    #[serde(deserialize_with = "null_default")]
    pub target_type: String,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CreatedApp {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub display_name: String,
    #[serde(deserialize_with = "null_default")]
    pub display_version: String,
    #[serde(deserialize_with = "null_default")]
    pub publishing_state: String,
    #[serde(deserialize_with = "null_default")]
    pub created_date_time: String,
    #[serde(deserialize_with = "null_default")]
    pub assignments: Vec<Assignment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RunResult {
    /// `Passed`, `Failed` or `Refused` (a guard stopped the run before anything was uploaded).
    #[serde(deserialize_with = "null_default")]
    pub status: String,
    #[serde(deserialize_with = "null_default")]
    pub recipe_id: String,
    pub exit_code: Option<i64>,
    #[serde(deserialize_with = "null_default")]
    pub started_utc: String,
    pub tenant: Option<TenantInfo>,
    pub app: Option<CreatedApp>,
    #[serde(deserialize_with = "null_default")]
    pub groups: Vec<GroupInfo>,
    #[serde(deserialize_with = "null_default")]
    pub problems: Vec<String>,
}

impl RunResult {
    pub fn read(path: &Path) -> Result<Self> {
        let text = crate::read_text(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).context("the tenant run wrote an unreadable result")
    }

    pub fn passed(&self) -> bool {
        self.status == "Passed" && self.problems.is_empty() && self.app.as_ref().is_some_and(|a| !a.id.is_empty())
    }

    /// A guard stopped the run before anything was uploaded.
    pub fn refused(&self) -> bool {
        self.status == "Refused"
    }

    pub fn summary(&self) -> String {
        if let Some(p) = self.problems.first() {
            return p.clone();
        }
        match &self.app {
            Some(a) => format!("{} {} is in Intune.", a.display_name, a.display_version).trim().to_string(),
            None => "The run did not report an app.".into(),
        }
    }
}

// ---- removing a test app -------------------------------------------------------------

/// The removal script, compiled in like the run script.
pub const REMOVE_SCRIPT: &str = include_str!("remove.ps1");

const REMOVE_FILE: &str = "Studio-TenantRemove.ps1";

/// A prepared removal: the process to start and where it writes its result.
pub struct PreparedRemove {
    pub spec: ProcessSpec,
    pub result: PathBuf,
}

/// Builds the removal of the test app a run recorded. It can delete exactly one
/// app, named by an id that Studio's own result put in the ledger, and only a
/// test app (the name still carries [`TEST_PREFIX`]) in the tenant the settings
/// name. The script re-reads the app and refuses unless it is still that app.
pub fn remove_spec(ws: &Workspace, tenant: &TenantSettings, record: &TenantRecord, consent: TenantConsent, pwsh: &Path, base: &Path) -> Result<PreparedRemove> {
    ensure_ready(ws, tenant, "remove a test app from the tenant")?;
    if consent != TenantConsent::Confirmed {
        bail!("Removing an app from Intune needs explicit confirmation.");
    }
    if record.removed {
        bail!("That test app has already been removed.");
    }
    if !super::is_tenant_id(&record.app_id) {
        bail!("The run did not record an Intune app id, so there is nothing to remove.");
    }
    if !record.app_name.starts_with(TEST_PREFIX) {
        bail!("'{}' is not a test app (its name does not start with '{TEST_PREFIX}'), so the Studio will not remove it.", record.app_name);
    }
    if !record.tenant_id.eq_ignore_ascii_case(tenant.tenant_id.trim()) {
        bail!("That test app was created in a different tenant than the one set under Settings → Tenant validation.");
    }
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let dir = base.join("removals").join(format!("remove-{stamp}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let script = dir.join(REMOVE_FILE);
    let input = dir.join("input.json");
    let result = dir.join("result.json");
    std::fs::write(&script, REMOVE_SCRIPT).with_context(|| format!("writing {}", script.display()))?;
    let payload = serde_json::json!({
        "root": ws.root.to_string_lossy(),
        "tenantId": tenant.tenant_id.trim(),
        "tenantName": tenant.tenant_name.trim(),
        "appId": record.app_id,
        "appName": record.app_name,
        "createdUtc": record.created,
        "testPrefix": TEST_PREFIX,
    });
    std::fs::write(&input, serde_json::to_string_pretty(&payload)?).with_context(|| format!("writing {}", input.display()))?;
    let spec = ProcessSpec::new(format!("Remove test app: {}", record.app_name), pwsh, &ws.root)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script.to_string_lossy())
        .args(["-InputPath"])
        .arg(input.to_string_lossy())
        .args(["-ResultPath"])
        .arg(result.to_string_lossy())
        .env("NO_COLOR", "1");
    Ok(PreparedRemove { spec, result })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoveResult {
    /// `Removed`, `Failed` or `Refused` (a check stopped it before anything was deleted).
    #[serde(deserialize_with = "null_default")]
    pub status: String,
    #[serde(deserialize_with = "null_default")]
    pub app_id: String,
    #[serde(deserialize_with = "null_default")]
    pub app_name: String,
    #[serde(deserialize_with = "null_default")]
    pub problems: Vec<String>,
}

impl RemoveResult {
    pub fn read(path: &Path) -> Result<Self> {
        let text = crate::read_text(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).context("the removal wrote an unreadable result")
    }

    pub fn removed(&self) -> bool {
        self.status == "Removed" && self.problems.is_empty()
    }

    pub fn summary(&self) -> String {
        self.problems.first().cloned().unwrap_or_else(|| if self.removed() { format!("{} was removed from Intune.", self.app_name) } else { "The removal did not report a result.".into() })
    }
}

// ---- the ledger --------------------------------------------------------------------

/// What the last tenant run of a recipe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantRecord {
    /// [`crate::validation::recipe_hash`] of the recipe that was run.
    pub sha256: String,
    pub tenant_id: String,
    pub passed: bool,
    pub app_id: String,
    pub app_name: String,
    pub version: String,
    /// When Intune created the app, as the run read it back. Removal checks it.
    #[serde(default)]
    pub created: String,
    /// Display names of the groups the app was assigned to.
    pub groups: Vec<String>,
    pub tested: String,
    /// The test app has since been removed from Intune.
    pub removed: bool,
    /// The first problem, when the run failed.
    pub problem: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantStatus {
    /// Never run on the tenant.
    NotRun,
    /// The recipe is exactly what passed on the tenant.
    Passed(TenantRecord),
    /// The last run found a problem.
    Failed(TenantRecord),
    /// It ran on the tenant, but the recipe has changed since.
    Changed(TenantRecord),
    /// It passed, and the test app was removed afterwards.
    Removed(TenantRecord),
}

/// Informational only: nothing about publishing depends on it.
#[derive(Debug, Default)]
pub struct TenantLedger {
    path: PathBuf,
    records: BTreeMap<String, TenantRecord>,
}

impl TenantLedger {
    pub fn path(harness_artifacts: &Path) -> PathBuf {
        harness_artifacts.join("_studio").join("tenant-validated.json")
    }

    pub fn load(harness_artifacts: &Path) -> Self {
        let path = Self::path(harness_artifacts);
        let records = crate::read_text(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        TenantLedger { path, records }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.records)?).with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&TenantRecord> {
        self.records.get(&id.to_lowercase())
    }

    /// Records a finished run of the recipe at `recipe`, unless the recipe
    /// changed after `started` (then the run tested something else), or a guard
    /// refused the run (nothing was uploaded). Returns whether it was recorded.
    pub fn record_run(&mut self, id: &str, recipe: &Path, started: SystemTime, tenant_id: &str, result: &RunResult, tested: &str) -> Result<bool> {
        if result.refused() {
            return Ok(false);
        }
        let modified = std::fs::metadata(recipe).and_then(|m| m.modified()).with_context(|| format!("reading {}", recipe.display()))?;
        if modified > started {
            return Ok(false);
        }
        let app = result.app.clone().unwrap_or_default();
        let record = TenantRecord {
            sha256: crate::validation::recipe_hash(recipe)?,
            tenant_id: tenant_id.to_string(),
            passed: result.passed(),
            app_id: app.id,
            app_name: app.display_name,
            version: app.display_version,
            created: app.created_date_time,
            groups: app.assignments.iter().filter_map(|a| a.group_name.clone().or_else(|| a.group_id.clone())).collect(),
            tested: tested.to_string(),
            removed: false,
            problem: result.problems.first().cloned().unwrap_or_default(),
        };
        self.records.insert(id.to_lowercase(), record);
        Ok(true)
    }

    /// Notes that the test app of `id` is gone from Intune. Returns whether a record changed.
    pub fn mark_removed(&mut self, id: &str) -> bool {
        match self.records.get_mut(&id.to_lowercase()) {
            Some(r) if !r.removed => {
                r.removed = true;
                true
            }
            _ => false,
        }
    }

    pub fn status(&self, id: &str, recipe: &Path) -> TenantStatus {
        let Some(record) = self.get(id) else { return TenantStatus::NotRun };
        if record.removed {
            return TenantStatus::Removed(record.clone());
        }
        match crate::validation::recipe_hash(recipe) {
            Ok(h) if h == record.sha256 => {
                if record.passed {
                    TenantStatus::Passed(record.clone())
                } else {
                    TenantStatus::Failed(record.clone())
                }
            }
            _ => TenantStatus::Changed(record.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::settings::TenantAfterRun;
    use crate::workspace::Workspace;

    const TENANT: &str = "11111111-2222-3333-4444-555555555555";

    /// A trusted full instance with one development recipe.
    fn connected() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("Yardstick.ps1"), "").unwrap();
        std::fs::write(root.join("Install-Yardstick.ps1"), "").unwrap();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\n}\n").unwrap();
        std::fs::create_dir_all(root.join("Local")).unwrap();
        std::fs::write(root.join(r"Local\preferences.yaml"), "defaultScopeTags: []\n").unwrap();
        std::fs::create_dir_all(root.join(r"Recipes\Development")).unwrap();
        std::fs::write(root.join(r"Recipes\Development\app.yaml"), "id: app\ndisplayName: 7-Zip\nversion: '1.0'\n").unwrap();
        let ws = Workspace::open(root, &[], &[root.to_path_buf()]).unwrap();
        (d, ws)
    }

    fn enabled() -> TenantSettings {
        TenantSettings { enabled: true, tenant_id: TENANT.into(), test_groups: vec!["Intune App Testers".into()], ..Default::default() }
    }

    const GROUP_ID: &str = "0b0e2f1c-7b1a-4e33-9d43-6f0f6f1a2b3c";

    /// What the connection check reports for `enabled()`'s test group.
    fn resolved() -> Vec<GroupInfo> {
        vec![GroupInfo { input: "Intune App Testers".into(), id: GROUP_ID.into(), name: "Intune App Testers".into() }]
    }

    fn run(ws: &Workspace, t: &TenantSettings, base: &Path, consent: TenantConsent) -> Result<PreparedRun> {
        run_spec(ws, t, "app", RunOptions::default(), &resolved(), consent, Path::new("pwsh.exe"), base)
    }

    fn overrides(ws: &Workspace) -> String {
        std::fs::read_to_string(overrides_path(ws)).unwrap()
    }

    #[test]
    fn the_run_needs_the_gate_and_consent_and_a_development_recipe() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        let err = |r: Result<PreparedRun>| format!("{:#}", r.err().expect("should refuse"));
        assert!(err(run(&ws, &enabled(), base.path(), TenantConsent::None)).contains("explicit confirmation"));
        assert!(err(run(&ws, &TenantSettings { enabled: false, ..enabled() }, base.path(), TenantConsent::Confirmed)).contains("switched off"));
        assert!(err(run_spec(&ws, &enabled(), "Bad Id", RunOptions::default(), &resolved(), TenantConsent::Confirmed, Path::new("pwsh"), base.path())).contains("not a valid recipe id"));
        assert!(err(run_spec(&ws, &enabled(), "nope", RunOptions::default(), &resolved(), TenantConsent::Confirmed, Path::new("pwsh"), base.path())).contains("Recipes\\Development"));
        assert!(err(run(&ws, &TenantSettings { test_groups: vec![], ..enabled() }, base.path(), TenantConsent::Confirmed)).contains("test group"));
        // Refusals touch nothing.
        assert!(!overrides_path(&ws).exists());
        assert!(!base.path().join("runs").exists() || std::fs::read_dir(base.path().join("runs")).unwrap().all(|e| !e.unwrap().path().join(MARKER).exists()));
        // Production and development-only clones are refused by the gate.
        let dev = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dev.path().join("Local")).unwrap();
        for f in ["Yardstick.ps1", "Yardstick.Project.psd1"] {
            std::fs::write(dev.path().join(f), "").unwrap();
        }
        std::fs::write(dev.path().join(r"Local\preferences.yaml"), "developmentOnly: true\n").unwrap();
        let dev_ws = Workspace::open(dev.path(), &[], &[]).unwrap();
        assert!(err(run(&dev_ws, &enabled(), base.path(), TenantConsent::Confirmed)).contains("development-only"));
    }

    #[test]
    fn an_assigned_run_is_staged_only_with_resolved_group_ids() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        let err = |r: Result<PreparedRun>| format!("{:#}", r.err().expect("should refuse"));
        let with = |groups: &[GroupInfo]| run_spec(&ws, &enabled(), "app", RunOptions::default(), groups, TenantConsent::Confirmed, Path::new("pwsh"), base.path());
        // Nothing resolved, another group resolved, or something that is not an id.
        assert!(err(with(&[])).contains("not resolved"));
        assert!(err(with(&[GroupInfo { input: "Other".into(), id: GROUP_ID.into(), name: "Other".into() }])).contains("not resolved"));
        assert!(err(with(&[GroupInfo { input: "Intune App Testers".into(), id: "Intune App Testers".into(), name: String::new() }])).contains("not a group id"));
        assert!(!overrides_path(&ws).exists());
        // Names match case-insensitively, and a configured id matches itself.
        let ids = resolved_group_ids(
            &TenantSettings { test_groups: vec!["intune app testers".into(), GROUP_ID.to_uppercase()], ..enabled() },
            &[GroupInfo { input: "Intune App Testers".into(), id: GROUP_ID.into(), name: String::new() }],
        )
        .unwrap();
        assert_eq!(ids, vec![GROUP_ID.to_string()]);
        // An unassigned run needs no groups at all.
        let unassigned = TenantSettings { after_run: TenantAfterRun::LeaveUnassigned, ..enabled() };
        let u = run_spec(&ws, &unassigned, "app", RunOptions::default(), &[], TenantConsent::Confirmed, Path::new("pwsh"), base.path()).unwrap();
        restore_overrides(&u.dir).unwrap();
    }

    #[test]
    fn the_command_line_is_fixed_and_carries_no_recipe_or_tenant_text() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        let prepared = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        let args = prepared.spec.args.join(" ");
        assert!(args.contains("-File") && !args.contains("-Command"));
        assert!(!args.contains("7-Zip") && !args.contains(TENANT) && !args.contains("Testers"));
        let input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(prepared.dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(input["recipeId"], "app");
        assert_eq!(input["testDisplayName"], "YDS TEST - 7-Zip");
        assert_eq!(input["assign"], true);
        assert_eq!(input["force"], false);
        assert_eq!(input["groups"][0], GROUP_ID);
        assert_eq!(std::fs::read_to_string(prepared.dir.join(RUN_FILE)).unwrap(), RUN_SCRIPT);
        // -Force only when asked, and the unassigned modes send no groups.
        let forced = run_spec(&ws, &enabled(), "app", RunOptions { force_reupload: true }, &resolved(), TenantConsent::Confirmed, Path::new("pwsh"), base.path()).unwrap();
        let forced_input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(forced.dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(forced_input["force"], true);
        restore_overrides(&forced.dir).unwrap();
        restore_overrides(&prepared.dir).unwrap();
        let unassigned = TenantSettings { after_run: TenantAfterRun::LeaveUnassigned, ..enabled() };
        let u = run(&ws, &unassigned, base.path(), TenantConsent::Confirmed).unwrap();
        let u_input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(u.dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(u_input["assign"], false);
        assert_eq!(u_input["groups"].as_array().unwrap().len(), 0);
        assert!(overrides(&ws).contains("defaultDeploymentGroups: []"));
    }

    #[test]
    fn staging_isolates_the_app_and_restoring_is_byte_exact() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        // An existing file with comments, CRLF, a BOM and an entry for this recipe.
        let original = "\u{feff}# site overrides\r\napp:\r\n  scopeTags:\r\n    - Workstations\r\n  displayName: 7-Zip (site)\r\nother:\r\n  scopeTags: [Labs]\r\n";
        std::fs::write(overrides_path(&ws), original).unwrap();
        let prepared = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        let staged: Value = serde_yaml::from_str(&overrides(&ws)).unwrap();
        let app = &staged["app"];
        assert_eq!(app["displayName"], "YDS TEST - 7-Zip");
        // Yardstick hands these to Graph as group ids; a display name is refused there.
        assert_eq!(app["defaultDeploymentGroups"][0], GROUP_ID);
        assert_eq!(app["supersedence"], false);
        assert_eq!(app["autoUpdateOnAssignment"], false);
        // The user's own fields and other recipes survive the run.
        assert_eq!(app["scopeTags"][0], "Workstations");
        assert_eq!(staged["other"]["scopeTags"][0], "Labs");
        assert_eq!(restore_overrides(&prepared.dir).unwrap(), Restored::Restored);
        assert_eq!(std::fs::read(overrides_path(&ws)).unwrap(), original.as_bytes());
        // Restoring twice is harmless.
        assert_eq!(restore_overrides(&prepared.dir).unwrap(), Restored::NothingToDo);
    }

    #[test]
    fn a_missing_overrides_file_is_removed_again() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        assert!(!overrides_path(&ws).exists());
        let prepared = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        assert!(overrides(&ws).contains("YDS TEST - 7-Zip"));
        assert_eq!(restore_overrides(&prepared.dir).unwrap(), Restored::Restored);
        assert!(!overrides_path(&ws).exists());
    }

    #[test]
    fn restoring_never_overwrites_an_edit_made_since() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        std::fs::write(overrides_path(&ws), "other:\n  scopeTags: [Labs]\n").unwrap();
        let prepared = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        std::fs::write(overrides_path(&ws), "edited: by the user\n").unwrap();
        let outcome = restore_overrides(&prepared.dir).unwrap();
        assert!(matches!(&outcome, Restored::LeftAlone(why) if why.contains("overrides.backup")), "{outcome:?}");
        assert_eq!(overrides(&ws), "edited: by the user\n");
        assert_eq!(std::fs::read_to_string(prepared.dir.join(BACKUP)).unwrap(), "other:\n  scopeTags: [Labs]\n");
    }

    #[test]
    fn an_interrupted_run_is_put_back_at_the_next_start() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        std::fs::write(overrides_path(&ws), "other:\n  scopeTags: [Labs]\n").unwrap();
        let _prepared = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        // The Studio dies here: no restore_overrides call.
        assert!(overrides(&ws).contains("YDS TEST"));
        let notes = recover_interrupted(base.path());
        assert_eq!(notes.len(), 1);
        assert_eq!(overrides(&ws), "other:\n  scopeTags: [Labs]\n");
        assert!(recover_interrupted(base.path()).is_empty(), "recovery only happens once");
    }

    #[test]
    fn staging_refuses_files_it_cannot_round_trip_safely() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        let before = |text: &str| std::fs::write(overrides_path(&ws), text).unwrap();
        before("app:\n  variantOf: other\n");
        let err = format!("{:#}", run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).err().unwrap());
        assert!(err.contains("variant"), "{err}");
        before("{ not: [valid");
        assert!(run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).is_err());
        before("- just\n- a list\n");
        assert!(run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).is_err());
        // Whatever was refused, the file is untouched.
        assert_eq!(overrides(&ws), "- just\n- a list\n");
        // A comment-only file is fine.
        before("# nothing yet\n");
        let ok = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        restore_overrides(&ok.dir).unwrap();
        assert_eq!(overrides(&ws), "# nothing yet\n");
    }

    #[test]
    fn the_id_match_in_the_overrides_file_ignores_case() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        std::fs::write(overrides_path(&ws), "APP:\n  scopeTags: [Labs]\n").unwrap();
        let p = run(&ws, &enabled(), base.path(), TenantConsent::Confirmed).unwrap();
        let staged: Value = serde_yaml::from_str(&overrides(&ws)).unwrap();
        assert_eq!(staged.as_mapping().unwrap().len(), 1, "one entry, not two");
        assert_eq!(staged["APP"]["displayName"], "YDS TEST - 7-Zip");
        restore_overrides(&p.dir).unwrap();
    }

    #[test]
    fn run_script_runs_one_recipe_and_only_reads_the_tenant() {
        let script = RUN_SCRIPT.to_ascii_lowercase();
        for method in ["post", "put", "patch", "delete"] {
            assert!(!script.contains(&format!("-method {method}")), "the run must not use {method}");
        }
        assert_eq!(script.matches("-method get").count(), script.matches("invoke-yardstickgraphrequest").count());
        // The single Yardstick command, with its fixed flags.
        assert_eq!(script.matches("'-applicationid', $recipeid, '-noemail', '-nodelete'").count(), 1);
        assert_eq!(script.matches("& $pwshexe @arguments").count(), 1);
        let forbidden = regex::Regex::new(r"-(all|group|repair|skipuninstall)\b").unwrap();
        assert!(!forbidden.is_match(&script), "the run must never widen beyond one recipe");
        for verb in ["remove-", "set-content", "new-item", "out-file", "add-content", "register-", "unregister-", "invoke-yardstick "] {
            assert!(!script.contains(verb), "the run must not contain {verb}");
        }
        // Every guard runs before the one Yardstick command.
        let run_at = script.find("& $pwshexe @arguments").unwrap();
        for guard in ["developmentonly", "syncsharedcontent", "testprefix", "tenantid", "get-yardstickintunecredential"] {
            assert!(script.find(guard).unwrap() < run_at, "{guard} must be checked before the run");
        }
    }

    #[test]
    fn run_script_parses_in_powershell() {
        let Some(pwsh) = crate::process::which("pwsh") else { return };
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join(RUN_FILE);
        std::fs::write(&path, RUN_SCRIPT).unwrap();
        let probe = format!(
            "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$null, [ref]$e); \
             if ($e.Count) {{ $e | ForEach-Object {{ $_.Message }}; exit 1 }}",
            crate::harness::ps_quote(&path.to_string_lossy())
        );
        let out = std::process::Command::new(pwsh).args(["-NoProfile", "-NonInteractive", "-Command", &probe]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }

    const PASSED: &str = r##"{"status": "Passed", "recipeId": "app", "exitCode": 0, "startedUtc": "2026-11-23T10:00:00Z",
        "tenant": {"id": "t", "name": "Contoso"}, "groups": [{"id": "g1", "name": "Intune App Testers"}],
        "app": {"id": "a1", "displayName": "YDS TEST - 7-Zip", "displayVersion": "1.0", "publishingState": "published",
                "createdDateTime": "2026-11-23T10:05:00Z",
                "assignments": [{"intent": "available", "targetType": "#microsoft.graph.groupAssignmentTarget", "groupId": "g1", "groupName": "Intune App Testers"}]},
        "problems": []}"##;

    #[test]
    fn reads_results() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.json");
        std::fs::write(&p, format!("\u{feff}{PASSED}")).unwrap();
        let r = RunResult::read(&p).unwrap();
        assert!(r.passed() && !r.refused());
        assert_eq!(r.app.as_ref().unwrap().assignments[0].group_name.as_deref(), Some("Intune App Testers"));
        assert_eq!(r.summary(), "YDS TEST - 7-Zip 1.0 is in Intune.");

        std::fs::write(&p, r#"{"status": "Refused", "problems": ["syncSharedContent is on."]}"#).unwrap();
        let r = RunResult::read(&p).unwrap();
        assert!(r.refused() && !r.passed());
        assert_eq!(r.summary(), "syncSharedContent is on.");
        // What the script writes when a guard stops it before anything ran: nulls, not blanks.
        std::fs::write(&p, r#"{"status": "Refused", "recipeId": "app", "exitCode": null, "startedUtc": null,
            "tenant": null, "app": null, "groups": [], "problems": ["Modules\\YardstickCredential.psm1 is missing."]}"#).unwrap();
        let r = RunResult::read(&p).unwrap();
        assert!(r.refused() && r.started_utc.is_empty() && r.exit_code.is_none());
        assert_eq!(r.summary(), "Modules\\YardstickCredential.psm1 is missing.");
        // "Passed" with problems, or without an app, is not a pass.
        std::fs::write(&p, r#"{"status": "Passed", "problems": []}"#).unwrap();
        assert!(!RunResult::read(&p).unwrap().passed());
        std::fs::write(&p, "nope").unwrap();
        assert!(RunResult::read(&p).is_err());
    }

    #[test]
    fn the_ledger_tracks_runs_against_the_recipe_contents() {
        let d = tempfile::tempdir().unwrap();
        let recipe = d.path().join("app.yaml");
        std::fs::write(&recipe, "id: app\n").unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        let passed: RunResult = serde_json::from_str(PASSED).unwrap();
        let mut ledger = TenantLedger::load(d.path());
        assert_eq!(ledger.status("app", &recipe), TenantStatus::NotRun);
        assert!(ledger.record_run("App", &recipe, later, TENANT, &passed, "2026-11-23 10:05:00").unwrap());
        ledger.save().unwrap();

        let mut ledger = TenantLedger::load(d.path());
        match ledger.status("app", &recipe) {
            TenantStatus::Passed(r) => {
                assert_eq!((r.app_id.as_str(), r.version.as_str(), r.tenant_id.as_str()), ("a1", "1.0", TENANT));
                assert_eq!(r.groups, vec!["Intune App Testers".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        std::fs::write(&recipe, "id: app\nversion: 2\n").unwrap();
        assert!(matches!(ledger.status("app", &recipe), TenantStatus::Changed(_)));

        // A failed run is remembered as failed, with its first problem.
        std::fs::write(&recipe, "id: app\n").unwrap();
        let failed: RunResult = serde_json::from_str(r#"{"status": "Failed", "problems": ["Yardstick exited with code 40."]}"#).unwrap();
        assert!(ledger.record_run("app", &recipe, later, TENANT, &failed, "t").unwrap());
        assert!(matches!(ledger.status("app", &recipe), TenantStatus::Failed(r) if r.problem.contains("40")));

        // A refused run uploaded nothing, so it changes nothing; a recipe edited mid-run is not recorded.
        let refused: RunResult = serde_json::from_str(r#"{"status": "Refused", "problems": ["x"]}"#).unwrap();
        assert!(!ledger.record_run("app", &recipe, later, TENANT, &refused, "t").unwrap());
        let long_ago = SystemTime::now() - Duration::from_secs(600);
        assert!(!ledger.record_run("app", &recipe, long_ago, TENANT, &passed, "t").unwrap());
        assert!(matches!(ledger.status("app", &recipe), TenantStatus::Failed(_)));
    }

    #[test]
    fn an_unreadable_ledger_is_empty() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("_studio")).unwrap();
        std::fs::write(TenantLedger::path(d.path()), "not json").unwrap();
        assert!(TenantLedger::load(d.path()).get("app").is_none());
    }

    fn record() -> TenantRecord {
        TenantRecord {
            sha256: "x".into(),
            tenant_id: TENANT.into(),
            passed: true,
            app_id: "0f9c1c4e-1111-4222-8333-444455556666".into(),
            app_name: "YDS TEST - 7-Zip".into(),
            version: "1.0".into(),
            created: "2026-11-23T10:05:00Z".into(),
            groups: vec![],
            tested: "t".into(),
            removed: false,
            problem: String::new(),
        }
    }

    #[test]
    fn removal_deletes_only_a_recorded_test_app_in_the_configured_tenant() {
        let (_d, ws) = connected();
        let base = tempfile::tempdir().unwrap();
        let remove = |r: &TenantRecord, t: &TenantSettings, c: TenantConsent| remove_spec(&ws, t, r, c, Path::new("pwsh.exe"), base.path());
        let err = |r: Result<PreparedRemove>| format!("{:#}", r.err().expect("should refuse"));
        assert!(err(remove(&record(), &enabled(), TenantConsent::None)).contains("explicit confirmation"));
        assert!(err(remove(&record(), &TenantSettings { enabled: false, ..enabled() }, TenantConsent::Confirmed)).contains("switched off"));
        // A real app's name, an empty or malformed id, a removed record, another tenant: all refused.
        assert!(err(remove(&TenantRecord { app_name: "7-Zip".into(), ..record() }, &enabled(), TenantConsent::Confirmed)).contains("not a test app"));
        assert!(err(remove(&TenantRecord { app_name: "Something YDS TEST - ".into(), ..record() }, &enabled(), TenantConsent::Confirmed)).contains("not a test app"));
        assert!(err(remove(&TenantRecord { app_id: String::new(), ..record() }, &enabled(), TenantConsent::Confirmed)).contains("did not record an Intune app id"));
        assert!(err(remove(&TenantRecord { app_id: "7-Zip".into(), ..record() }, &enabled(), TenantConsent::Confirmed)).contains("app id"));
        assert!(err(remove(&TenantRecord { removed: true, ..record() }, &enabled(), TenantConsent::Confirmed)).contains("already been removed"));
        assert!(err(remove(&TenantRecord { tenant_id: "aaaaaaaa-2222-3333-4444-555555555555".into(), ..record() }, &enabled(), TenantConsent::Confirmed)).contains("different tenant"));
        assert!(!base.path().join("removals").exists(), "a refusal writes nothing");

        let ok = remove(&record(), &enabled(), TenantConsent::Confirmed).unwrap();
        let args = ok.spec.args.join(" ");
        assert!(args.contains("-File") && !args.contains("-Command"));
        assert!(!args.contains("0f9c1c4e") && !args.contains("7-Zip"), "ids and names travel in the JSON input, not on the command line");
        let dir = ok.result.parent().unwrap();
        let input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(input["appId"], "0f9c1c4e-1111-4222-8333-444455556666");
        assert_eq!(input["createdUtc"], "2026-11-23T10:05:00Z");
        assert_eq!(input["testPrefix"], TEST_PREFIX);
        assert_eq!(std::fs::read_to_string(dir.join(REMOVE_FILE)).unwrap(), REMOVE_SCRIPT);
        // Production and development-only clones are refused by the gate.
        let prod = Workspace::open(ws.root.as_path(), std::slice::from_ref(&ws.root), &[]).unwrap();
        assert!(remove_spec(&prod, &enabled(), &record(), TenantConsent::Confirmed, Path::new("pwsh"), base.path()).is_err());
    }

    #[test]
    fn remove_script_sends_one_delete_after_it_has_proved_what_it_is_deleting() {
        let script = REMOVE_SCRIPT.to_ascii_lowercase();
        assert_eq!(script.matches("-method delete").count(), 1, "exactly one delete");
        for method in ["post", "put", "patch"] {
            assert!(!script.contains(&format!("-method {method}")), "removal must not use {method}");
        }
        // The delete targets the recorded id only: no search, no wildcard, no name.
        let delete_line = script.lines().find(|l| l.contains("-method delete")).unwrap();
        assert!(delete_line.contains("$appid") && !delete_line.contains('?') && !delete_line.contains("filter"), "{delete_line}");
        // From the moment it re-reads the app, nothing may search or loop.
        let tail = &script[script.find("re-read the app").expect("the proof section is marked")..];
        assert!(!tail.contains("$filter") && !tail.contains("-like") && !tail.contains("foreach") && !tail.contains("where-object"), "no searching or looping near a delete");
        for verb in ["remove-", "set-content", "new-item", "out-file", "yardstick.ps1", "invoke-yardstick "] {
            assert!(!script.contains(verb), "removal must not contain {verb}");
        }
        // Every proof comes before the delete.
        let delete_at = script.find("-method delete").unwrap();
        for proof in ["is not a guid", "developmentonly", "get-yardstickintunecredential", "organization", "win32lobapp", "startswith($prefix)", "createdutc"] {
            let at = script.find(proof).unwrap_or_else(|| panic!("missing check: {proof}"));
            assert!(at < delete_at, "{proof} must be checked before the delete");
        }
    }

    #[test]
    fn remove_script_parses_in_powershell() {
        let Some(pwsh) = crate::process::which("pwsh") else { return };
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join(REMOVE_FILE);
        std::fs::write(&path, REMOVE_SCRIPT).unwrap();
        let probe = format!(
            "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$null, [ref]$e); \
             if ($e.Count) {{ $e | ForEach-Object {{ $_.Message }}; exit 1 }}",
            crate::harness::ps_quote(&path.to_string_lossy())
        );
        let out = std::process::Command::new(pwsh).args(["-NoProfile", "-NonInteractive", "-Command", &probe]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }

    #[test]
    fn reads_removal_results_and_marks_the_ledger() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.json");
        std::fs::write(&p, r#"{"status": "Removed", "appId": "a", "appName": "YDS TEST - 7-Zip", "problems": []}"#).unwrap();
        let r = RemoveResult::read(&p).unwrap();
        assert!(r.removed());
        assert_eq!(r.summary(), "YDS TEST - 7-Zip was removed from Intune.");
        std::fs::write(&p, r#"{"status": "Refused", "appId": "a", "appName": null, "problems": ["The app is now called 'x', which is not a test app, so it was not removed."]}"#).unwrap();
        let r = RemoveResult::read(&p).unwrap();
        assert!(!r.removed() && r.summary().contains("not a test app"));
        // "Removed" with a problem is not a removal.
        std::fs::write(&p, r#"{"status": "Removed", "problems": ["x"]}"#).unwrap();
        assert!(!RemoveResult::read(&p).unwrap().removed());

        let recipe = d.path().join("app.yaml");
        std::fs::write(&recipe, "id: app\n").unwrap();
        let passed: RunResult = serde_json::from_str(PASSED).unwrap();
        let mut ledger = TenantLedger::load(d.path());
        ledger.record_run("app", &recipe, SystemTime::now() + Duration::from_secs(5), TENANT, &passed, "t").unwrap();
        assert_eq!(ledger.get("app").unwrap().created, "2026-11-23T10:05:00Z");
        assert!(ledger.mark_removed("App"));
        assert!(!ledger.mark_removed("app"), "already marked");
        assert!(!ledger.mark_removed("other"));
        assert!(matches!(ledger.status("app", &recipe), TenantStatus::Removed(_)));
        // A record written before `created` existed still loads.
        ledger.save().unwrap();
        let text = std::fs::read_to_string(TenantLedger::path(d.path())).unwrap().replace("\"created\"", "\"createdX\"");
        std::fs::write(TenantLedger::path(d.path()), text).unwrap();
        assert_eq!(TenantLedger::load(d.path()).get("app").unwrap().created, "");
    }

    #[test]
    fn test_names_carry_the_prefix_and_no_wildcard_characters() {
        assert_eq!(test_display_name(" 7-Zip "), "YDS TEST - 7-Zip");
        assert!(!TEST_PREFIX.contains(['[', ']', '*', '?']));
    }
}
