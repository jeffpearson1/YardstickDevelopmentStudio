//! Development-only setup through `Install-Yardstick.ps1`, driven by the
//! Studio's setup wizard: `-Status` reads what is in place without changing
//! anything, and a `-DevelopmentOnly -NonInteractive` run applies the
//! wizard's answers. Both write JSON to `-ResultPath` (schemaVersion 1).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::process::ProcessSpec;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub schema_version: u32,
    pub power_shell_version: String,
    pub powershell_yaml: Installed,
    pub optional_modules: Vec<OptionalModule>,
    pub execution_policy: ExecutionPolicy,
    pub preferences: Preferences,
    pub unwritable_paths: Unwritable,
    pub intune_win_app_util: Tool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Installed {
    pub installed: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OptionalModule {
    pub name: String,
    pub purpose: String,
    pub used_by_recipes: bool,
    pub minimum_version: Option<String>,
    pub installed_version: Option<String>,
    pub is_current: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutionPolicy {
    /// What a normal PowerShell window gets (the Process scope is ignored).
    pub policy: String,
    pub scope: String,
    /// Set by Group Policy, so `-SetExecutionPolicy` cannot change it.
    pub group_policy: bool,
    pub allows_scripts: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub exists: bool,
    pub path: String,
    pub read_error: Option<String>,
    pub development_only: bool,
    pub shared_icons: String,
    pub shared_recipe_overrides: String,
    pub sync_shared_content: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Unwritable {
    pub count: u32,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Tool {
    pub present: bool,
    pub path: String,
}

/// The `-ResultPath` file of a setup run.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupResult {
    pub schema_version: u32,
    pub succeeded: bool,
    pub development_only: bool,
    pub steps: Vec<StepResult>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StepResult {
    pub step: String,
    pub result: String,
    /// `OK`, `Skip`, `Fail` or `Warn`; use it rather than `result`.
    pub level: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Message {
    pub step: String,
    /// `OK`, `Warn`, `Fail`, `Info` or `Skip`.
    pub level: String,
    pub message: String,
}

/// What the wizard asks the installer to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answers {
    /// Optional modules to install or update (`Pester`, `Selenium`, …).
    pub modules: Vec<String>,
    /// Set RemoteSigned for the current user when the policy blocks scripts.
    pub set_execution_policy: bool,
    /// `None` keeps the stored value; `Some("")` clears it.
    pub shared_icons: Option<String>,
    pub shared_recipe_overrides: Option<String>,
}

/// Whether this clone's installer understands the wizard's parameters
/// (`-Status`, `-ResultPath`, `-InstallOptionalModule`); older clones need a pull.
pub fn supported(ws: &Workspace) -> bool {
    crate::read_text(&ws.root.join("Install-Yardstick.ps1"))
        .is_ok_and(|t| t.contains("$InstallOptionalModule") && t.contains("[switch]$Status") && t.contains("$ResultPath"))
}

/// PowerShell 7, which the installer requires: the configured PowerShell when
/// it is pwsh, else pwsh on PATH or in its default install folder.
pub fn pwsh7(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("pwsh")) && p.is_file()) {
        return Some(p.to_path_buf());
    }
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    crate::process::find_program("pwsh", &[program_files.join(r"PowerShell\7")])
}

fn spec(label: &str, ws: &Workspace, pwsh: &Path, args: &[&str]) -> ProcessSpec {
    let script = ws.root.join("Install-Yardstick.ps1");
    ProcessSpec::new(label, pwsh, &ws.root)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &script.to_string_lossy()])
        .args(args.iter().copied())
}

/// `Install-Yardstick.ps1 -Status -ResultPath <result>`: changes nothing.
pub fn status_spec(ws: &Workspace, pwsh: &Path, result: &Path) -> Result<ProcessSpec> {
    ws.ensure_writable("set up")?;
    Ok(spec("Setup status", ws, pwsh, &["-Status", "-ResultPath", &result.to_string_lossy()]))
}

/// A development-only, non-interactive setup run with the wizard's answers.
pub fn setup_spec(ws: &Workspace, pwsh: &Path, answers: &Answers, result: &Path) -> Result<ProcessSpec> {
    ws.ensure_writable("set up")?;
    if let Some(bad) = answers.modules.iter().find(|m| m.is_empty() || !m.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')) {
        bail!("Invalid module name '{bad}'.");
    }
    let mut args: Vec<String> = vec!["-DevelopmentOnly".into(), "-NonInteractive".into()];
    if let Some(icons) = &answers.shared_icons {
        args.extend(["-SharedIcons".into(), icons.trim().to_string()]);
    }
    if let Some(overrides) = &answers.shared_recipe_overrides {
        args.extend(["-SharedRecipeOverrides".into(), overrides.trim().to_string()]);
    }
    if !answers.modules.is_empty() {
        // Under `pwsh -File`, a list must be one comma-separated argument.
        args.extend(["-InstallOptionalModule".into(), answers.modules.join(",")]);
    }
    if answers.set_execution_policy {
        args.push("-SetExecutionPolicy".into());
    }
    args.extend(["-ResultPath".into(), result.to_string_lossy().into_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(spec("Set up Yardstick (development-only)", ws, pwsh, &args))
}

pub fn read_status(path: &Path) -> Result<Status> {
    let text = crate::read_text(path).with_context(|| format!("the installer did not write {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

pub fn read_result(path: &Path) -> Result<SetupResult> {
    let text = crate::read_text(path).with_context(|| format!("the installer did not write {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::fake_workspace;

    const STATUS: &str = r#"{
      "schemaVersion": 1,
      "projectRoot": "C:\\Intune\\YardstickDev",
      "powerShellVersion": "7.6.9",
      "powershellYaml": { "installed": true, "version": "0.4.12" },
      "optionalModules": [
        { "name": "Selenium", "purpose": "recipes that drive a browser", "usedByRecipes": true,
          "minimumVersion": "4.0.0-preview3", "installedVersion": null, "isCurrent": false },
        { "name": "Pester", "purpose": "running the unit tests in Tests\\Unit", "usedByRecipes": false,
          "minimumVersion": "5.0.0", "installedVersion": "6.2.0", "isCurrent": true }
      ],
      "executionPolicy": { "policy": "Restricted", "scope": "CurrentUser", "groupPolicy": false, "allowsScripts": false },
      "preferences": { "exists": false, "path": "C:\\Intune\\YardstickDev\\Local\\preferences.yaml", "readError": null,
        "developmentOnly": false, "sharedIcons": "", "sharedRecipeOverrides": "", "syncSharedContent": false },
      "unwritablePaths": { "count": 0, "examples": [] },
      "intuneWinAppUtil": { "present": false, "path": "C:\\Intune\\YardstickDev\\Tools\\IntuneWinAppUtil.exe" }
    }"#;

    const RESULT: &str = r#"{
      "schemaVersion": 1, "succeeded": true, "developmentOnly": true,
      "steps": [
        { "step": "Prerequisites", "result": "Done", "level": "OK", "detail": "" },
        { "step": "AppRegistration", "result": "Skipped", "level": "Skip", "detail": "development-only instance" },
        { "step": "Verify", "result": "Problems found", "level": "Warn", "detail": "" }
      ],
      "messages": [ { "step": "Verify", "level": "Warn", "message": "225 recipe icon(s) missing (...)" } ]
    }"#;

    #[test]
    fn reads_the_installer_json() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        // The installer writes UTF-8 without a BOM; a BOM is tolerated anyway.
        std::fs::write(&p, format!("\u{feff}{STATUS}")).unwrap();
        let s = read_status(&p).unwrap();
        assert_eq!(s.optional_modules[0].installed_version, None);
        assert!(!s.optional_modules[0].is_current && s.optional_modules[1].is_current);
        assert!(!s.execution_policy.allows_scripts && !s.execution_policy.group_policy);
        assert!(!s.preferences.exists && !s.intune_win_app_util.present);

        std::fs::write(&p, RESULT).unwrap();
        let r = read_result(&p).unwrap();
        assert!(r.succeeded && r.development_only);
        assert_eq!(r.steps[2].level, "Warn");
        assert_eq!(r.messages[0].step, "Verify");
        assert!(read_result(&d.path().join("missing.json")).unwrap_err().to_string().contains("did not write"));
    }

    #[test]
    fn setup_arguments_follow_the_installer_contract() {
        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let answers = Answers {
            modules: vec!["Pester".into(), "Selenium".into()],
            set_execution_policy: true,
            shared_icons: Some(String::new()),
            shared_recipe_overrides: None,
        };
        let s = setup_spec(&ws, Path::new("pwsh"), &answers, Path::new(r"C:\t\r.json")).unwrap();
        let tail: Vec<&str> = s.args.iter().skip(5).map(String::as_str).collect();
        assert_eq!(tail, ["-DevelopmentOnly", "-NonInteractive", "-SharedIcons", "", "-InstallOptionalModule", "Pester,Selenium", "-SetExecutionPolicy", "-ResultPath", r"C:\t\r.json"]);
        assert_eq!(&s.args[..4], ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        assert!(!s.new_console);

        let bad = Answers { modules: vec!["Pester; rm".into()], ..Default::default() };
        assert!(setup_spec(&ws, Path::new("pwsh"), &bad, Path::new("r.json")).is_err());
        let status = status_spec(&ws, Path::new("pwsh"), Path::new("s.json")).unwrap();
        assert_eq!(&status.args[5..], ["-Status", "-ResultPath", "s.json"]);

        let prod = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(status_spec(&prod, Path::new("pwsh"), Path::new("s.json")).is_err());
    }

    #[test]
    fn detects_installers_that_support_the_wizard() {
        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert!(!supported(&ws));
        std::fs::write(d.path().join("Install-Yardstick.ps1"), "param([string[]]$InstallOptionalModule, [switch]$Status, [string]$ResultPath)").unwrap();
        assert!(supported(&ws));
    }
}
