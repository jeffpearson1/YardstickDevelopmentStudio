//! Optional validation of a recipe on the user's own Intune tenant, before it is
//! published.
//!
//! Everything here is opt-in (`TenantSettings::enabled`, off by default) and is
//! the only part of the Studio allowed near a tenant, so every builder starts
//! with [`ensure_ready`]: the clone must be a trusted development clone that has
//! been made a full Yardstick instance, never production and never
//! development-only, and the user must have switched the feature on and named
//! the tenant it may talk to.
//!
//! This module currently holds the read-only half: the connection preflight and
//! the command that makes a clone a full instance. Nothing in it writes to the
//! tenant.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::harness::ps_quote;
use crate::process::ProcessSpec;
use crate::settings::TenantSettings;
use crate::workspace::Workspace;

/// The preflight script, compiled in so the file Studio runs is the file it shipped.
pub const PREFLIGHT_SCRIPT: &str = include_str!("tenant/preflight.ps1");

const PREFLIGHT_FILE: &str = "Studio-TenantPreflight.ps1";

/// Where the Studio keeps the scripts and results of tenant runs.
pub fn work_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("YardstickStudio").join("tenant")
}

/// An Entra tenant id: a GUID.
pub fn is_tenant_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Why a group name cannot be used, if it cannot. Names go to Graph inside a
/// filter, so control characters and absurd lengths are refused up front.
pub fn group_problem(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        Some("A test group name is empty.")
    } else if name.chars().any(char::is_control) {
        Some("A test group name contains a control character.")
    } else if name.chars().count() > 256 {
        Some("A test group name is longer than 256 characters.")
    } else {
        None
    }
}

/// The gate every tenant action passes first. It errors, with a user-facing
/// explanation, unless this workspace and these settings allow talking to a tenant.
pub fn ensure_ready(ws: &Workspace, tenant: &TenantSettings, action: &str) -> Result<()> {
    ws.ensure_writable(action)?;
    if !tenant.enabled {
        bail!("Tenant validation is switched off. Turn it on under Settings → Tenant validation to {action}.");
    }
    if ws.development_only {
        bail!(
            "This clone is development-only, so Yardstick.ps1 refuses to run and it never signs in to Intune. \
             Use Settings → Tenant validation → Connect this clone to Intune first."
        );
    }
    if !ws.tenant_connected() {
        bail!(
            "'{}' is not a trusted development clone that has been set up as a full Yardstick instance, so the Studio will not {action} there.",
            ws.root.display()
        );
    }
    if !is_tenant_id(tenant.tenant_id.trim()) {
        bail!("Enter the tenant id (a GUID) this clone is allowed to use under Settings → Tenant validation.");
    }
    if let Some(problem) = tenant.test_groups.iter().find_map(|g| group_problem(g)) {
        bail!("{problem}");
    }
    Ok(())
}

/// A prepared preflight run: the process and the file it writes its report to.
pub struct PreflightRun {
    pub spec: ProcessSpec,
    pub result: PathBuf,
}

/// Builds the read-only connection check. `dir` receives the script, its input
/// and its result (see [`work_dir`]).
pub fn preflight_spec(ws: &Workspace, tenant: &TenantSettings, pwsh: &Path, dir: &Path) -> Result<PreflightRun> {
    ensure_ready(ws, tenant, "check the tenant connection")?;
    let dir = dir.join("preflight");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let script = dir.join(PREFLIGHT_FILE);
    let input = dir.join("input.json");
    let result = dir.join("result.json");
    let _ = std::fs::remove_file(&result);
    std::fs::write(&script, PREFLIGHT_SCRIPT).with_context(|| format!("writing {}", script.display()))?;
    let groups: Vec<&str> = tenant.test_groups.iter().map(|g| g.trim()).collect();
    let payload = serde_json::json!({
        "root": ws.root.to_string_lossy(),
        "tenantId": tenant.tenant_id.trim(),
        "tenantName": tenant.tenant_name.trim(),
        "groups": groups,
    });
    std::fs::write(&input, serde_json::to_string_pretty(&payload)?).with_context(|| format!("writing {}", input.display()))?;
    let spec = ProcessSpec::new("Tenant connection check", pwsh, &ws.root)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script.to_string_lossy())
        .args(["-InputPath"])
        .arg(input.to_string_lossy())
        .args(["-ResultPath"])
        .arg(result.to_string_lossy())
        .env("NO_COLOR", "1");
    Ok(PreflightRun { spec, result })
}

/// Opens `Install-Yardstick.ps1 -DevelopmentOnly:$false` in its own console, the
/// interactive setup that registers the app, stores the credential and turns a
/// development-only clone into a full instance. Production is refused.
///
/// The caller must have added the clone to the trusted development folders
/// first, or the Studio will classify it as production once it is connected.
pub fn connect_spec(ws: &Workspace, pwsh: &Path) -> Result<ProcessSpec> {
    ws.ensure_writable("connect to Intune")?;
    let script = ws.root.join("Install-Yardstick.ps1");
    if !script.is_file() {
        bail!("Install-Yardstick.ps1 was not found in '{}'.", ws.root.display());
    }
    let mut spec = ProcessSpec::new("Install-Yardstick (connect to Intune)", pwsh, &ws.root).args([
        "-NoProfile",
        "-NoExit",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
    ]);
    spec = spec.arg(format!("& {} -DevelopmentOnly:$false", ps_quote(&script.to_string_lossy())));
    spec.new_console = true;
    Ok(spec)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum CheckLevel {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub id: String,
    pub level: CheckLevel,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// A configured test group and what it resolved to.
#[derive(Debug, Clone, Deserialize)]
pub struct GroupInfo {
    pub input: String,
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreflightReport {
    pub ok: bool,
    pub tenant: Option<TenantInfo>,
    pub groups: Vec<GroupInfo>,
    pub checks: Vec<Check>,
}

impl PreflightReport {
    /// Reads the report the preflight script wrote.
    pub fn read(path: &Path) -> Result<Self> {
        let text = crate::read_text(path).with_context(|| format!("reading {}", path.display()))?;
        let report: Self = serde_json::from_str(&text).context("the tenant check wrote an unreadable report")?;
        Ok(report)
    }

    /// A tenant run may go ahead only when nothing failed and the tenant was reached.
    pub fn passed(&self) -> bool {
        self.ok && self.tenant.is_some() && !self.checks.iter().any(|c| c.level == CheckLevel::Fail)
    }

    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.level == CheckLevel::Fail)
    }

    /// One line for a status message: the first failure, or the tenant reached.
    pub fn summary(&self) -> String {
        if let Some(f) = self.failures().next() {
            return f.detail.clone();
        }
        match &self.tenant {
            Some(t) if !t.name.is_empty() => format!("Connected to {}.", t.name),
            Some(_) => "Connected to the tenant.".into(),
            None => "The tenant could not be reached.".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    const TENANT: &str = "11111111-2222-3333-4444-555555555555";

    fn clone_with(prefs: Option<&str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("Yardstick.ps1"), "").unwrap();
        std::fs::write(root.join("Install-Yardstick.ps1"), "").unwrap();
        std::fs::write(root.join("Yardstick.Project.psd1"), "@{\n}\n").unwrap();
        std::fs::create_dir_all(root.join("Local")).unwrap();
        if let Some(p) = prefs {
            std::fs::write(root.join(r"Local\preferences.yaml"), p).unwrap();
        }
        d
    }

    fn enabled() -> TenantSettings {
        TenantSettings { enabled: true, tenant_id: TENANT.into(), test_groups: vec!["Intune App Testers".into()], ..Default::default() }
    }

    /// A full instance the user trusts: the only shape allowed near a tenant.
    fn connected() -> (tempfile::TempDir, Workspace) {
        let d = clone_with(Some("defaultScopeTags: []\n"));
        let ws = Workspace::open(d.path(), &[], &[d.path().to_path_buf()]).unwrap();
        (d, ws)
    }

    fn refusal(ws: &Workspace, t: &TenantSettings) -> String {
        format!("{:#}", ensure_ready(ws, t, "run a recipe").unwrap_err())
    }

    #[test]
    fn a_trusted_full_instance_passes_the_gate() {
        let (_d, ws) = connected();
        assert!(ws.tenant_connected());
        ensure_ready(&ws, &enabled(), "run a recipe").unwrap();
    }

    #[test]
    fn the_gate_refuses_everything_that_is_not_a_connected_dev_clone() {
        // Production: a full install nobody has trusted.
        let d = clone_with(Some("defaultScopeTags: []\n"));
        let prod = Workspace::open(d.path(), &[], &[]).unwrap();
        assert!(refusal(&prod, &enabled()).contains("production"));
        // A listed production folder wins even over trust.
        let listed = Workspace::open(d.path(), &[d.path().to_path_buf()], &[d.path().to_path_buf()]).unwrap();
        assert!(refusal(&listed, &enabled()).contains("production"));
        // Development-only never signs in to Intune.
        let dev = clone_with(Some("developmentOnly: true\n"));
        let dev_ws = Workspace::open(dev.path(), &[], &[]).unwrap();
        assert!(refusal(&dev_ws, &enabled()).contains("development-only"));
        // A clone that has not been set up has no credential to use.
        let fresh = clone_with(None);
        let fresh_ws = Workspace::open(fresh.path(), &[], &[]).unwrap();
        assert!(refusal(&fresh_ws, &enabled()).contains("not a trusted development clone"));
    }

    #[test]
    fn the_gate_needs_the_switch_the_tenant_and_valid_groups() {
        let (_d, ws) = connected();
        assert!(refusal(&ws, &TenantSettings { enabled: false, ..enabled() }).contains("switched off"));
        assert!(refusal(&ws, &TenantSettings { tenant_id: String::new(), ..enabled() }).contains("tenant id"));
        assert!(refusal(&ws, &TenantSettings { tenant_id: "not-a-guid".into(), ..enabled() }).contains("tenant id"));
        assert!(refusal(&ws, &TenantSettings { test_groups: vec!["  ".into()], ..enabled() }).contains("empty"));
        assert!(refusal(&ws, &TenantSettings { test_groups: vec!["a\nb".into()], ..enabled() }).contains("control"));
        // No test group is allowed: the app is simply uploaded unassigned.
        ensure_ready(&ws, &TenantSettings { test_groups: vec![], ..enabled() }, "run a recipe").unwrap();
    }

    #[test]
    fn recognises_tenant_ids() {
        assert!(is_tenant_id(TENANT));
        assert!(is_tenant_id("0AFB8F4C-1B2C-4D3E-8F9A-0123456789AB"));
        assert!(!is_tenant_id(""));
        assert!(!is_tenant_id("11111111-2222-3333-4444-55555555555"));
        assert!(!is_tenant_id("11111111-2222-3333-4444-55555555555g"));
        assert!(!is_tenant_id("contoso.onmicrosoft.com"));
    }

    #[test]
    fn preflight_command_runs_the_script_file_with_data_in_json_only() {
        let (_d, ws) = connected();
        let work = tempfile::tempdir().unwrap();
        let tenant = TenantSettings { tenant_name: " Contoso ".into(), test_groups: vec![" It's a group ".into()], ..enabled() };
        let run = preflight_spec(&ws, &tenant, Path::new("pwsh.exe"), work.path()).unwrap();
        let args = run.spec.args.join(" ");
        assert!(args.contains("-File") && !args.contains("-Command"));
        // The command line carries paths only: no tenant, group or recipe text.
        assert!(!args.contains("Contoso") && !args.contains(TENANT) && !args.contains("group"));
        assert!(!args.contains("Yardstick.ps1"));
        let dir = work.path().join("preflight");
        assert_eq!(std::fs::read_to_string(dir.join(PREFLIGHT_FILE)).unwrap(), PREFLIGHT_SCRIPT);
        let input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("input.json")).unwrap()).unwrap();
        assert_eq!(input["tenantId"], TENANT);
        assert_eq!(input["tenantName"], "Contoso");
        assert_eq!(input["groups"][0], "It's a group");
        assert_eq!(input["root"], ws.root.to_string_lossy().as_ref());
        assert_eq!(run.result, dir.join("result.json"));
    }

    #[test]
    fn preflight_refuses_where_the_gate_refuses() {
        let dev = clone_with(Some("developmentOnly: true\n"));
        let ws = Workspace::open(dev.path(), &[], &[]).unwrap();
        let work = tempfile::tempdir().unwrap();
        assert!(preflight_spec(&ws, &enabled(), Path::new("pwsh.exe"), work.path()).is_err());
        assert!(!work.path().join("preflight").exists(), "nothing is written when the gate refuses");
    }

    #[test]
    fn preflight_script_only_reads() {
        let script = PREFLIGHT_SCRIPT.to_ascii_lowercase();
        // Every Graph call is a GET.
        for method in ["post", "put", "patch", "delete"] {
            assert!(!script.contains(&format!("-method {method}")), "preflight must not use {method}");
        }
        assert_eq!(script.matches("-method get").count(), script.matches("invoke-yardstickgraphrequest").count());
        // No cmdlet that changes anything, and no Yardstick run.
        for verb in ["remove-", "set-content", "new-item", "out-file", "add-content", "register-", "unregister-", "start-process", "invoke-yardstick ", "yardstick.ps1"] {
            assert!(!script.contains(verb), "preflight must not contain {verb}");
        }
        // The only file it writes is the result.
        assert_eq!(script.matches("writealltext").count(), 1);
    }

    #[test]
    fn preflight_script_parses_in_powershell() {
        let Some(pwsh) = crate::process::which("pwsh") else { return };
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join(PREFLIGHT_FILE);
        std::fs::write(&path, PREFLIGHT_SCRIPT).unwrap();
        let probe = format!(
            "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$null, [ref]$e); \
             if ($e.Count) {{ $e | ForEach-Object {{ $_.Message }}; exit 1 }}",
            ps_quote(&path.to_string_lossy())
        );
        let out = std::process::Command::new(pwsh).args(["-NoProfile", "-NonInteractive", "-Command", &probe]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }

    #[test]
    fn connect_opens_the_interactive_installer_and_never_in_production() {
        let d = clone_with(None);
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let spec = connect_spec(&ws, Path::new("pwsh.exe")).unwrap();
        assert!(spec.new_console);
        let last = spec.args.last().unwrap();
        assert!(last.contains("Install-Yardstick.ps1") && last.ends_with("-DevelopmentOnly:$false"));
        let prod = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(connect_spec(&prod, Path::new("pwsh.exe")).is_err());
    }

    #[test]
    fn reads_a_report_and_judges_it() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("r.json");
        let good = r#"{"ok": true, "tenant": {"id": "t", "name": "Contoso"},
            "groups": [{"input": "Testers", "id": "g1", "name": "Testers"}],
            "checks": [{"id": "tenant", "level": "Pass", "detail": "Connected to 'Contoso'."},
                       {"id": "group:Testers", "level": "Warn", "detail": "dynamic"}]}"#;
        std::fs::write(&path, format!("\u{feff}{good}")).unwrap();
        let r = PreflightReport::read(&path).unwrap();
        assert!(r.passed());
        assert_eq!(r.summary(), "Connected to Contoso.");
        assert_eq!(r.groups[0].id, "g1");

        let bad = r#"{"ok": false, "tenant": null, "groups": [],
            "checks": [{"id": "shared-sync", "level": "Fail", "detail": "syncSharedContent is on."}]}"#;
        std::fs::write(&path, bad).unwrap();
        let r = PreflightReport::read(&path).unwrap();
        assert!(!r.passed());
        assert_eq!(r.summary(), "syncSharedContent is on.");

        // ok without a tenant is not a pass; a missing or garbled file is an error.
        std::fs::write(&path, r#"{"ok": true, "tenant": null, "checks": []}"#).unwrap();
        assert!(!PreflightReport::read(&path).unwrap().passed());
        std::fs::write(&path, "not json").unwrap();
        assert!(PreflightReport::read(&path).is_err());
        assert!(PreflightReport::read(&d.path().join("missing.json")).is_err());
    }
}
