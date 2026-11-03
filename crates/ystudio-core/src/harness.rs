//! Builds the PowerShell invocations for Yardstick's own tooling: the recipe
//! harness (schema, lab, install test), Publish-Yardstick, and the
//! development-only installer. Every builder enforces the Studio's safety rules
//! before it hands back a [`ProcessSpec`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::process::{ProcessSpec, which};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabStage {
    Download,
    Package,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallTestOptions {
    /// Dry run: everything except the install and uninstall commands.
    pub what_if: bool,
    /// Skip system-context recipes rather than prompting for elevation.
    pub no_elevate: bool,
    pub discard_installer: bool,
    pub include_reinstall: bool,
    pub skip_uninstall: bool,
    /// Continue when the app is already detected. Only with explicit consent.
    pub force: bool,
    pub install_timeout_minutes: u32,
    pub detection_settle_seconds: u32,
    pub residue_settle_seconds: u32,
}

impl InstallTestOptions {
    /// The skill's recommended safe dry run.
    pub fn dry_run() -> Self {
        Self {
            what_if: true,
            no_elevate: true,
            discard_installer: true,
            include_reinstall: false,
            skip_uninstall: false,
            force: false,
            install_timeout_minutes: 30,
            detection_settle_seconds: 120,
            residue_settle_seconds: 30,
        }
    }

    /// The skill's recommended live run.
    pub fn live() -> Self {
        Self {
            what_if: false,
            no_elevate: false,
            install_timeout_minutes: 90,
            detection_settle_seconds: 180,
            residue_settle_seconds: 60,
            ..Self::dry_run()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HarnessAction {
    Schema { recipe: PathBuf },
    Lab { recipe: PathBuf, stage: LabStage },
    InstallTest { recipes: Vec<PathBuf>, options: InstallTestOptions },
}

impl HarnessAction {
    pub fn title(&self) -> String {
        let ids = |paths: &[PathBuf]| paths.iter().map(|p| recipe_id(p)).collect::<Vec<_>>().join(", ");
        match self {
            HarnessAction::Schema { recipe } => format!("Schema · {}", recipe_id(recipe)),
            HarnessAction::Lab { recipe, stage } => format!("Lab {:?} · {}", stage, recipe_id(recipe)),
            HarnessAction::InstallTest { recipes, options } if options.what_if => format!("Dry run · {}", ids(recipes)),
            HarnessAction::InstallTest { recipes, .. } => format!("LIVE install test · {}", ids(recipes)),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, HarnessAction::InstallTest { options, .. } if !options.what_if)
    }
}

/// Explicit, per-run consent for actions that change the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    None,
    /// The user confirmed a dialog describing exactly what will be installed.
    Confirmed,
}

pub fn recipe_id(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Single-quoted PowerShell literal.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn ps_array(items: &[PathBuf]) -> String {
    items.iter().map(|p| ps_quote(&p.to_string_lossy())).collect::<Vec<_>>().join(", ")
}

/// pwsh 7 when available, otherwise Windows PowerShell.
pub fn powershell_exe(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path.filter(|p| !p.as_os_str().is_empty()) {
        return p.to_path_buf();
    }
    which("pwsh").or_else(|| which("powershell")).unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

/// Wraps a script body so that output is plain text and a thrown error becomes exit 1.
pub fn wrap_script(root: &Path, body: &str) -> String {
    format!(
        "$ProgressPreference = 'SilentlyContinue'; \
         if ($PSStyle) {{ $PSStyle.OutputRendering = 'PlainText' }}; \
         Set-Location -LiteralPath {root}; \
         try {{ {body}; if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 0 }} }} \
         catch {{ Write-Host \"ERROR: $($_.Exception.Message)\"; exit 1 }}",
        root = ps_quote(&root.to_string_lossy()),
    )
}

fn pwsh_spec(label: String, ws: &Workspace, pwsh: &Path, body: &str) -> ProcessSpec {
    ProcessSpec::new(label, pwsh, &ws.root)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(wrap_script(&ws.root, body))
        .env("NO_COLOR", "1")
}

pub fn build(ws: &Workspace, action: &HarnessAction, consent: Consent, pwsh: &Path) -> Result<ProcessSpec> {
    // Even schema runs sync shared icons/overrides into the folder, so every
    // harness action needs a writable (non-production) workspace.
    ws.ensure_writable("run the recipe harness")?;
    // Recipes\Development is git-ignored, so a fresh clone lacks it and the
    // harness's Resolve-Path on it fails.
    std::fs::create_dir_all(&ws.layout.development_recipes)
        .with_context(|| format!("creating {}", ws.layout.development_recipes.display()))?;
    let harness = &ws.layout.harness;
    let script = |name: &str| ps_quote(&harness.join(name).to_string_lossy());

    let body = match action {
        HarnessAction::Schema { recipe } => {
            ensure_inside(ws, recipe)?;
            format!("& {} -RecipePath {} | Format-List | Out-String -Width 200", script("Test-RecipeSchema.ps1"), ps_quote(&recipe.to_string_lossy()))
        }
        HarnessAction::Lab { recipe, stage } => {
            ensure_inside(ws, recipe)?;
            let stage = match stage {
                LabStage::Download => "Download",
                LabStage::Package => "Package",
            };
            format!(
                "& {} -RecipePath {} -Stage {stage} | Format-List | Out-String -Width 200",
                script("Invoke-RecipeLab.ps1"),
                ps_quote(&recipe.to_string_lossy())
            )
        }
        HarnessAction::InstallTest { recipes, options } => {
            if recipes.is_empty() {
                bail!("Select at least one recipe.");
            }
            for r in recipes {
                ensure_inside(ws, r)?;
            }
            if !options.what_if && consent != Consent::Confirmed {
                bail!("A live install test installs and uninstalls real software on this machine and needs explicit confirmation.");
            }
            if options.force && consent != Consent::Confirmed {
                bail!("-Force touches software the harness did not install and needs explicit confirmation.");
            }
            let mut b = format!("& {} -RecipePath {}", script("Invoke-RecipeInstallTest.ps1"), ps_array(recipes));
            let mut flag = |on: bool, f: &str| {
                if on {
                    b.push(' ');
                    b.push_str(f);
                }
            };
            flag(options.what_if, "-WhatIf");
            flag(options.no_elevate, "-NoElevate");
            flag(options.discard_installer, "-DiscardInstaller");
            flag(options.include_reinstall, "-IncludeReinstall");
            flag(options.skip_uninstall, "-SkipUninstall");
            flag(options.force, "-Force");
            if !options.what_if {
                b.push_str(&format!(
                    " -InstallTimeoutMinutes {} -DetectionSettleSeconds {} -ResidueSettleSeconds {}",
                    options.install_timeout_minutes.clamp(1, 240),
                    options.detection_settle_seconds,
                    options.residue_settle_seconds
                ));
            }
            b
        }
    };
    Ok(pwsh_spec(action.title(), ws, pwsh, &body))
}

/// `Publish-Yardstick.ps1 -RecipeId ... -ProductionPath ...` run from the
/// development workspace. A real publish needs consent; previews never do.
pub fn publish(ws: &Workspace, recipe_ids: &[String], production: &Path, what_if: bool, consent: Consent, pwsh: &Path) -> Result<ProcessSpec> {
    ws.ensure_writable("publish from")?;
    if recipe_ids.is_empty() {
        bail!("Select at least one recipe to promote.");
    }
    if crate::same_path(&ws.root, production) {
        bail!("The production path is this workspace.");
    }
    if !crate::workspace::is_yardstick_root(production) {
        bail!("'{}' is not a Yardstick folder.", production.display());
    }
    if let Some(bad) = recipe_ids.iter().find(|id| !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))) {
        bail!("Invalid recipe id '{bad}'.");
    }
    if !what_if && consent != Consent::Confirmed {
        bail!("Publishing to production needs explicit confirmation.");
    }
    let ids = recipe_ids.iter().map(|i| ps_quote(i)).collect::<Vec<_>>().join(", ");
    let mut body = format!(
        "& {} -RecipeId {ids} -ProductionPath {}",
        ps_quote(&ws.root.join("Publish-Yardstick.ps1").to_string_lossy()),
        ps_quote(&production.to_string_lossy())
    );
    body.push_str(if what_if { " -WhatIf" } else { " -Confirm:$false" });
    let label = format!("{} · {}", if what_if { "Publish preview" } else { "PUBLISH" }, recipe_ids.join(", "));
    Ok(pwsh_spec(label, ws, pwsh, &body))
}

/// Opens `Install-Yardstick.ps1 -DevelopmentOnly` in its own console; it is interactive.
pub fn install_development_only(ws: &Workspace, pwsh: &Path) -> Result<ProcessSpec> {
    ws.ensure_writable("configure")?;
    let mut spec = ProcessSpec::new("Install-Yardstick -DevelopmentOnly", pwsh, &ws.root).args([
        "-NoProfile",
        "-NoExit",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &ws.root.join("Install-Yardstick.ps1").to_string_lossy(),
        "-DevelopmentOnly",
    ]);
    spec.new_console = true;
    Ok(spec)
}

/// `git clone <url> <dest>` for a fresh development workspace.
pub fn git_clone(url: &str, dest: &Path) -> Result<ProcessSpec> {
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
        bail!("'{}' already exists and is not empty.", dest.display());
    }
    let git = which("git").ok_or_else(|| anyhow::anyhow!("git was not found on PATH."))?;
    let parent = dest.parent().filter(|p| p.is_dir()).ok_or_else(|| anyhow::anyhow!("The parent folder of '{}' does not exist.", dest.display()))?;
    Ok(ProcessSpec::new("git clone", git, parent).args(["clone", "--progress", url, &dest.to_string_lossy()]))
}

/// Copies icons that the destination lacks (icons are not in Git). Reads the
/// source only; never overwrites. Returns the number copied.
pub fn copy_missing_icons(from: &Path, to: &Path) -> Result<usize> {
    std::fs::create_dir_all(to)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(from)?.flatten() {
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let dst = to.join(entry.file_name());
        if !dst.exists() {
            std::fs::copy(&src, &dst)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn ensure_inside(ws: &Workspace, recipe: &Path) -> Result<()> {
    let r = crate::normalize_path(recipe);
    let root = crate::normalize_path(&ws.root);
    if !r.starts_with(&root) {
        bail!("'{}' is outside the workspace '{}'.", recipe.display(), ws.root.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::fake_workspace;

    fn dev_ws() -> (tempfile::TempDir, Workspace) {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        (d, ws)
    }

    fn body(spec: &ProcessSpec) -> &str {
        spec.args.last().unwrap()
    }

    #[test]
    fn quoting() {
        assert_eq!(ps_quote("it's"), "'it''s'");
    }

    #[test]
    fn schema_command() {
        let (_d, ws) = dev_ws();
        let recipe = ws.development_recipe_path("brave");
        let spec = build(&ws, &HarnessAction::Schema { recipe }, Consent::None, Path::new("pwsh")).unwrap();
        assert!(body(&spec).contains("Test-RecipeSchema.ps1"));
        assert!(body(&spec).contains("brave.yaml"));
        assert!(spec.args.contains(&"-NonInteractive".to_string()));
    }

    #[test]
    fn dry_run_needs_no_consent_but_live_does() {
        let (_d, ws) = dev_ws();
        let recipes = vec![ws.development_recipe_path("brave")];
        let dry = HarnessAction::InstallTest { recipes: recipes.clone(), options: InstallTestOptions::dry_run() };
        let spec = build(&ws, &dry, Consent::None, Path::new("pwsh")).unwrap();
        let b = body(&spec);
        assert!(b.contains("-WhatIf") && b.contains("-NoElevate") && b.contains("-DiscardInstaller"));

        let live = HarnessAction::InstallTest { recipes, options: InstallTestOptions::live() };
        assert!(live.is_live());
        assert!(build(&ws, &live, Consent::None, Path::new("pwsh")).is_err());
        let spec = build(&ws, &live, Consent::Confirmed, Path::new("pwsh")).unwrap();
        assert!(!body(&spec).contains("-WhatIf"));
        assert!(body(&spec).contains("-InstallTimeoutMinutes 90"));
    }

    #[test]
    fn production_is_refused() {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        let recipe = ws.development_recipe_path("brave");
        let err = build(&ws, &HarnessAction::Schema { recipe }, Consent::Confirmed, Path::new("pwsh")).unwrap_err();
        assert!(err.to_string().contains("production"));
    }

    #[test]
    fn recipe_outside_workspace_is_refused() {
        let (_d, ws) = dev_ws();
        let action = HarnessAction::Schema { recipe: PathBuf::from(r"C:\Windows\evil.yaml") };
        assert!(build(&ws, &action, Consent::None, Path::new("pwsh")).is_err());
    }

    #[test]
    fn publish_rules() {
        let (_d, ws) = dev_ws();
        let prod = fake_workspace(Some("x: 1\n"));
        let ids = vec!["brave".to_string()];
        let preview = publish(&ws, &ids, prod.path(), true, Consent::None, Path::new("pwsh")).unwrap();
        assert!(body(&preview).contains("-WhatIf"));
        assert!(publish(&ws, &ids, prod.path(), false, Consent::None, Path::new("pwsh")).is_err());
        let real = publish(&ws, &ids, prod.path(), false, Consent::Confirmed, Path::new("pwsh")).unwrap();
        assert!(body(&real).contains("-Confirm:$false"));
        assert!(publish(&ws, &ids, &ws.root, true, Consent::None, Path::new("pwsh")).is_err());
        assert!(publish(&ws, &["a;rm".to_string()], prod.path(), true, Consent::None, Path::new("pwsh")).is_err());
    }

    #[test]
    fn icon_copy_only_fills_gaps() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.png"), b"new").unwrap();
        std::fs::write(a.path().join("y.png"), b"new").unwrap();
        std::fs::write(b.path().join("x.png"), b"keep").unwrap();
        assert_eq!(copy_missing_icons(a.path(), b.path()).unwrap(), 1);
        assert_eq!(std::fs::read(b.path().join("x.png")).unwrap(), b"keep");
    }
}
