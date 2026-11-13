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
    /// Reported by installers that take `-SoftwareDropbox`.
    pub software_dropbox: String,
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
    /// Where licensed media for manualDownload recipes is staged; only for
    /// installers that take `-SoftwareDropbox` ([`supports_dropbox`]).
    pub software_dropbox: Option<String>,
}

/// Whether this clone's installer understands the wizard's parameters
/// (`-Status`, `-ResultPath`, `-InstallOptionalModule`); older clones need a pull.
pub fn supported(ws: &Workspace) -> bool {
    crate::read_text(&ws.root.join("Install-Yardstick.ps1"))
        .is_ok_and(|t| t.contains("$InstallOptionalModule") && t.contains("[switch]$Status") && t.contains("$ResultPath"))
}

/// Whether this clone's installer takes `-SoftwareDropbox`, which
/// development-only setups otherwise never ask for.
pub fn supports_dropbox(ws: &Workspace) -> bool {
    crate::read_text(&ws.root.join("Install-Yardstick.ps1")).is_ok_and(|t| t.contains("$SoftwareDropbox"))
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
        args.extend(["-SharedIcons".into(), network_path(icons)]);
    }
    if let Some(overrides) = &answers.shared_recipe_overrides {
        args.extend(["-SharedRecipeOverrides".into(), overrides_file(&network_path(overrides))]);
    }
    if let Some(dropbox) = &answers.software_dropbox {
        args.extend(["-SoftwareDropbox".into(), network_path(dropbox)]);
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

/// The installer takes the overrides *file*, and only recognises a folder
/// that is reachable right now: a folder not created yet, or offline, would
/// be stored as the file and every sync would then fail. So name the file.
pub fn overrides_file(value: &str) -> String {
    let v = value.trim().trim_matches(['"', '\'']).trim();
    let lower = v.to_ascii_lowercase();
    if v.is_empty() || lower.ends_with(".yaml") || lower.ends_with(".yml") {
        v.to_string()
    } else {
        format!(r"{}\recipeOverrides.yaml", v.trim_end_matches(['\\', '/']))
    }
}

/// `Z:\Intune\Icons` as `\\server\share\Intune\Icons` when Z: is a mapped
/// network drive: elevated processes and scheduled tasks do not see the
/// user's drive mappings. Anything else comes back trimmed and unchanged.
pub fn network_path(value: &str) -> String {
    let v = value.trim().trim_matches(['"', '\'']).trim();
    let bytes = v.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
        && let Some(remote) = mapped_drive(&v[..2])
    {
        return format!("{}{}", remote.trim_end_matches('\\'), &v[2..]);
    }
    v.to_string()
}

/// The network path behind drive `letter` (`Z:`), if it is a mapped drive.
#[cfg(windows)]
fn mapped_drive(letter: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    let local: Vec<u16> = letter.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `local` is NUL-terminated and `len` is the buffer's length in characters.
    let rc = unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) };
    if rc != NO_ERROR {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end])).filter(|s| s.starts_with(r"\\"))
}

#[cfg(not(windows))]
fn mapped_drive(_: &str) -> Option<String> {
    None
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
            software_dropbox: None,
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
    fn shared_overrides_always_name_the_file() {
        assert_eq!(overrides_file(r"\\server\share\Yardstick"), r"\\server\share\Yardstick\recipeOverrides.yaml");
        assert_eq!(overrides_file(r"  \\server\share\Yardstick\ "), r"\\server\share\Yardstick\recipeOverrides.yaml");
        assert_eq!(overrides_file(r#""\\server\my share\Team.YML""#), r"\\server\my share\Team.YML");
        assert_eq!(overrides_file(""), "");
        assert_eq!(network_path(r#" "\\server\share\Icons" "#), r"\\server\share\Icons");
        // Not a mapped drive (or not a drive at all): unchanged.
        assert_eq!(network_path(r"C:\Intune\Icons"), r"C:\Intune\Icons");
        assert_eq!(network_path("Icons"), "Icons");

        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let answers = Answers { shared_icons: Some(r"\\s\x\Icons".into()), shared_recipe_overrides: Some(r"\\s\x\Not Yet Created".into()), ..Default::default() };
        let s = setup_spec(&ws, Path::new("pwsh"), &answers, Path::new("r.json")).unwrap();
        let tail: Vec<&str> = s.args.iter().skip(7).take(4).map(String::as_str).collect();
        assert_eq!(tail, ["-SharedIcons", r"\\s\x\Icons", "-SharedRecipeOverrides", r"\\s\x\Not Yet Created\recipeOverrides.yaml"]);
    }

    #[test]
    fn detects_installers_that_support_the_wizard() {
        let d = fake_workspace(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert!(!supported(&ws));
        std::fs::write(d.path().join("Install-Yardstick.ps1"), "param([string[]]$InstallOptionalModule, [switch]$Status, [string]$ResultPath)").unwrap();
        assert!(supported(&ws) && !supports_dropbox(&ws));
        std::fs::write(d.path().join("Install-Yardstick.ps1"), "param([string[]]$InstallOptionalModule, [switch]$Status, [string]$ResultPath, [string]$SoftwareDropbox)").unwrap();
        assert!(supports_dropbox(&ws));
        let answers = Answers { software_dropbox: Some(" Q:/x ".into()), ..Default::default() };
        let s = setup_spec(&ws, Path::new("pwsh"), &answers, Path::new("r.json")).unwrap();
        assert!(s.args.windows(2).any(|w| w == ["-SoftwareDropbox", "Q:/x"]));
    }
}
