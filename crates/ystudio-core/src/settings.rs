//! Studio settings, stored as JSON in `%APPDATA%\YardstickStudio\settings.json`.

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
}
