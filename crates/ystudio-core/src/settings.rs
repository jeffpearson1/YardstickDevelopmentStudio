//! Studio settings, stored as JSON in `%APPDATA%\YardstickStudio\settings.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent::AgentBackend;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// The Yardstick folder currently open.
    pub workspace: Option<PathBuf>,
    pub recent_workspaces: Vec<PathBuf>,
    /// Folders that run against the tenant. Always read-only in the Studio and
    /// the default target for promotion.
    pub production_paths: Vec<PathBuf>,
    /// Full installs the user has confirmed are scratch, overriding the heuristic.
    pub trusted_dev_paths: Vec<PathBuf>,
    pub repo_url: String,
    pub agent_backend: AgentBackend,
    /// Optional model override passed to the agent CLI.
    pub agent_model: String,
    /// Override for the PowerShell executable (pwsh 7 is preferred).
    pub powershell_path: Option<PathBuf>,
    pub dark_mode: bool,
    pub editor_font_size: f32,
    /// Line numbers in the YAML editor and the Designer's script fields.
    pub line_numbers: bool,
    /// `git fetch` when a workspace opens and on every recipe Refresh.
    pub auto_git_fetch: bool,
    /// Run Yardstick's shared icon/recipe-override sync on open and Refresh
    /// (only for workspaces with `syncSharedContent: true`).
    pub auto_sync_shared: bool,
    /// Check GitHub Releases for a newer Studio at startup.
    pub auto_check_updates: bool,
    /// A version the user chose to skip; startup checks stay quiet about it.
    pub skipped_update: String,
    /// The first-run setup wizard was finished or skipped.
    pub setup_done: bool,
    /// The user approves agents downloading installers and inspecting them
    /// with tools outside the workspace (see `agent::Approvals`).
    pub agent_downloads: bool,
    /// Agents run as a coordinator with parallel subagents.
    pub agent_multi: bool,
    /// In Multi-agent mode, how many apps Recipe Builder researches at once.
    pub agent_parallel: u32,
    /// Publish puts up to this many new recipes in one pull request.
    pub publish_per_pr: u32,
    /// Keep full job output, the Studio's messages and the agent CLIs' own
    /// logs in `%APPDATA%\YardstickStudio\logs` (see `diagnostics`).
    pub diagnostic_logging: bool,
    /// Per installed plugin, by id. Kept when a plugin is uninstalled.
    pub plugins: BTreeMap<String, PluginPrefs>,
    /// Prompts started from the Agent tab, newest first.
    pub agent_prompt_history: Vec<PromptRecord>,
    /// The optional "run on my tenant" validation (see `tenant`). Off by default.
    pub tenant: TenantSettings,
}

/// What happens to the app a tenant validation run creates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum TenantAfterRun {
    /// Assign it to the test group(s) and leave it there.
    #[default]
    LeaveInTestGroup,
    /// Upload it with no assignments and leave it.
    LeaveUnassigned,
    /// Upload it with no assignments, then remove it.
    Remove,
}

impl TenantAfterRun {
    pub const ALL: [TenantAfterRun; 3] = [Self::LeaveInTestGroup, Self::LeaveUnassigned, Self::Remove];

    pub fn title(self) -> &'static str {
        match self {
            Self::LeaveInTestGroup => "Deploy to the test group and leave it",
            Self::LeaveUnassigned => "Upload without assignments and leave it",
            Self::Remove => "Remove it after the run",
        }
    }

    /// Whether the app is assigned to the test group(s) during the run.
    pub fn assigns(self) -> bool {
        matches!(self, Self::LeaveInTestGroup)
    }
}

/// Settings for validating a recipe on the user's own Intune tenant before it
/// is published. Everything here is inert until `enabled` is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TenantSettings {
    /// Shows the Tenant step on a recipe's Test tab. Off by default.
    pub enabled: bool,
    /// The tenant the clone's Intune credential must belong to (an Entra tenant id).
    pub tenant_id: String,
    /// Optional: the organization's display name must match too.
    pub tenant_name: String,
    /// Entra groups (display name or object id) the test app is assigned to.
    pub test_groups: Vec<String>,
    pub after_run: TenantAfterRun,
    /// With `Remove`, ask before deleting instead of removing straight away.
    pub ask_before_remove: bool,
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tenant_id: String::new(),
            tenant_name: String::new(),
            test_groups: Vec::new(),
            after_run: TenantAfterRun::default(),
            ask_before_remove: true,
        }
    }
}

/// One prompt started from the Agent tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct PromptRecord {
    /// The recipe it was for.
    pub recipe: String,
    /// The task kind: Fix, Update or Freeform.
    pub kind: String,
    pub text: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PluginPrefs {
    /// Shows the plugin's tab.
    pub enabled: bool,
    /// Values for the manifest's settings, by key.
    pub values: BTreeMap<String, String>,
    /// The package SHA-256 the user agreed to on the trust prompt.
    pub trusted_sha256: String,
    /// Where it was installed from: a zip path or release URL.
    pub source: String,
}

impl Default for PluginPrefs {
    fn default() -> Self {
        Self { enabled: true, values: BTreeMap::new(), trusted_sha256: String::new(), source: String::new() }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            workspace: None,
            recent_workspaces: Vec::new(),
            production_paths: vec![PathBuf::from(crate::DEFAULT_PRODUCTION_PATH)],
            trusted_dev_paths: Vec::new(),
            repo_url: crate::DEFAULT_REPO_URL.to_string(),
            agent_backend: AgentBackend::CopilotCli,
            agent_model: String::new(),
            powershell_path: None,
            dark_mode: true,
            editor_font_size: 13.5,
            line_numbers: true,
            auto_git_fetch: true,
            auto_sync_shared: true,
            auto_check_updates: true,
            skipped_update: String::new(),
            setup_done: false,
            agent_downloads: false,
            agent_multi: false,
            agent_parallel: 3,
            publish_per_pr: 10,
            diagnostic_logging: false,
            plugins: BTreeMap::new(),
            agent_prompt_history: Vec::new(),
            tenant: TenantSettings::default(),
        }
    }
}

impl Settings {
    pub fn default_path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("YardstickStudio").join("settings.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        crate::read_text(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn remember_workspace(&mut self, path: PathBuf) {
        self.recent_workspaces.retain(|p| !crate::same_path(p, &path));
        self.recent_workspaces.insert(0, path.clone());
        self.recent_workspaces.truncate(8);
        self.workspace = Some(path);
    }

    pub fn primary_production(&self) -> Option<&PathBuf> {
        self.production_paths.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("s.json");
        let mut s = Settings::default();
        assert_eq!(s.production_paths, vec![PathBuf::from(r"C:\Intune\Yardstick")]);
        s.remember_workspace(PathBuf::from(r"C:\a"));
        s.remember_workspace(PathBuf::from(r"C:\b"));
        s.remember_workspace(PathBuf::from(r"C:\a"));
        assert_eq!(s.recent_workspaces, vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]);
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
    }

    #[test]
    fn tolerates_partial_and_corrupt_files() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("s.json");
        std::fs::write(&path, r#"{"dark_mode": false}"#).unwrap();
        let s = Settings::load_from(&path);
        assert!(!s.dark_mode);
        assert_eq!(s.repo_url, crate::DEFAULT_REPO_URL);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn tenant_validation_is_off_unless_asked_for() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("s.json");
        // Settings saved before the feature existed carry no tenant block.
        std::fs::write(&path, r#"{"dark_mode": true}"#).unwrap();
        let t = Settings::load_from(&path).tenant;
        assert!(!t.enabled);
        assert_eq!(t.after_run, TenantAfterRun::LeaveInTestGroup);
        assert!(t.ask_before_remove && t.test_groups.is_empty());
        // A partial block keeps the safe defaults for what it leaves out.
        std::fs::write(&path, r#"{"tenant": {"tenant_id": "abc"}}"#).unwrap();
        let t = Settings::load_from(&path).tenant;
        assert_eq!(t.tenant_id, "abc");
        assert!(!t.enabled && t.ask_before_remove);
        assert!(TenantAfterRun::LeaveInTestGroup.assigns());
        assert!(!TenantAfterRun::LeaveUnassigned.assigns() && !TenantAfterRun::Remove.assigns());
    }

    #[test]
    fn plugin_prefs_default_to_enabled() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("s.json");
        std::fs::write(&path, r#"{"plugins": {"oneoff-packager": {"values": {"workRoot": "C:\\Work"}}}}"#).unwrap();
        let p = &Settings::load_from(&path).plugins["oneoff-packager"];
        assert!(p.enabled);
        assert_eq!(p.values["workRoot"], r"C:\Work");
    }
}
