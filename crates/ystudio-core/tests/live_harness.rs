//! End-to-end checks against a real Yardstick development clone.
//!
//! Ignored by default because they need PowerShell, the Yardstick modules and
//! network access. Run with:
//!   $env:YSTUDIO_TEST_CLONE = 'C:\path\to\YardstickDev'
//!   $env:YSTUDIO_TEST_PRODUCTION = 'C:\Intune\Yardstick'   # optional
//!   .\build.ps1 -Task test -CargoArgs '--', '--ignored', '--nocapture'

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ystudio_core::harness::{self, Consent, HarnessAction, InstallTestOptions};
use ystudio_core::process::{ProcessSpec, RunningProcess};
use ystudio_core::results::{self, Verdict};
use ystudio_core::workspace::Workspace;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn run(spec: ProcessSpec, timeout: Duration) -> RunningProcess {
    println!("> {}", spec.display());
    let mut proc = RunningProcess::spawn(spec, None).expect("spawn");
    let start = Instant::now();
    while proc.is_running() {
        proc.poll();
        assert!(start.elapsed() < timeout, "timed out:\n{}", proc.text());
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("{}", proc.text());
    proc
}

#[test]
#[ignore]
fn schema_and_dry_run_against_development_clone() {
    let Some(root) = env_path("YSTUDIO_TEST_CLONE") else {
        eprintln!("YSTUDIO_TEST_CLONE not set - skipping");
        return;
    };
    let production: Vec<PathBuf> = env_path("YSTUDIO_TEST_PRODUCTION").into_iter().collect();
    let ws = Workspace::open(&root, &production, std::slice::from_ref(&root)).expect("open clone");
    assert!(ws.is_writable(), "trusted clone must be writable");
    // Icons are not in Git; mirror the Studio's clone flow, which copies them (read-only) from production.
    if let Some(prod) = production.first() {
        let copied = harness::copy_missing_icons(&prod.join("Icons"), &ws.layout.icons).expect("copy icons");
        println!("copied {copied} icons from production");
    }
    let pwsh = harness::powershell_exe(None);
    let recipe = ws.find_recipe("7zip").expect("7zip recipe");

    let schema = harness::build(&ws, &HarnessAction::Schema { recipe: recipe.clone() }, Consent::None, &pwsh).unwrap();
    assert!(run(schema, Duration::from_secs(300)).succeeded(), "schema validation failed");

    let started = std::time::SystemTime::now();
    let dry = HarnessAction::InstallTest { recipes: vec![recipe], options: InstallTestOptions::dry_run() };
    run(harness::build(&ws, &dry, Consent::None, &pwsh).unwrap(), Duration::from_secs(900));
    let run = results::newest_run_since(&ws.layout.harness_artifacts, started).expect("summary.json for the dry run");
    println!("{run:#?}");
    let summary = run.summary.as_ref().unwrap_or_else(|| panic!("unreadable summary: {:?}", run.error));
    let result = summary.results.iter().find(|r| r.id.eq_ignore_ascii_case("7zip")).expect("7zip result");
    assert_eq!(result.verdict(), Verdict::DryRunPassed);
}

#[test]
#[ignore]
fn production_folder_is_refused() {
    let Some(prod) = env_path("YSTUDIO_TEST_PRODUCTION") else {
        eprintln!("YSTUDIO_TEST_PRODUCTION not set - skipping");
        return;
    };
    let ws = Workspace::open(&prod, std::slice::from_ref(&prod), &[]).expect("open production");
    assert!(!ws.is_writable());
    let recipe = ws.find_recipe("7zip").expect("7zip recipe");
    let pwsh = harness::powershell_exe(None);
    assert!(harness::build(&ws, &HarnessAction::Schema { recipe: recipe.clone() }, Consent::None, &pwsh).is_err());
    let live = HarnessAction::InstallTest { recipes: vec![recipe], options: InstallTestOptions::live() };
    assert!(harness::build(&ws, &live, Consent::Confirmed, &pwsh).is_err());
    assert!(ystudio_core::promote::plan(&ws, "7zip").is_err());
}

#[test]
#[ignore]
fn setup_status_reads_the_clone_without_changing_it() {
    let Some(root) = env_path("YSTUDIO_TEST_CLONE") else {
        eprintln!("YSTUDIO_TEST_CLONE not set - skipping");
        return;
    };
    let ws = Workspace::open(&root, &[], &[]).expect("open clone");
    if !ystudio_core::setup::supported(&ws) {
        eprintln!("this clone's Install-Yardstick.ps1 predates -Status - skipping");
        return;
    }
    let pwsh = ystudio_core::setup::pwsh7(None).expect("PowerShell 7");
    let out = tempfile::tempdir().unwrap();
    let result = out.path().join("status.json");
    let proc = run(ystudio_core::setup::status_spec(&ws, &pwsh, &result).unwrap(), Duration::from_secs(120));
    assert!(proc.succeeded(), "status failed");
    let status = ystudio_core::setup::read_status(&result).unwrap();
    assert_eq!(status.schema_version, 1);
    assert!(status.powershell_yaml.installed);
    assert!(status.optional_modules.iter().any(|m| m.name == "Pester"));
    assert!(!status.execution_policy.policy.is_empty());
}

/// Changes the clone (preferences, Tools), so it needs a clone made for it.
#[test]
#[ignore]
fn development_only_setup_on_a_scratch_clone() {
    let Some(root) = env_path("YSTUDIO_TEST_SCRATCH_CLONE") else {
        eprintln!("YSTUDIO_TEST_SCRATCH_CLONE not set - skipping");
        return;
    };
    let ws = Workspace::open(&root, &[], &[]).expect("open clone");
    assert!(ystudio_core::setup::supported(&ws), "Install-Yardstick.ps1 predates the wizard");
    let pwsh = ystudio_core::setup::pwsh7(None).expect("PowerShell 7");
    let out = tempfile::tempdir().unwrap();
    let result = out.path().join("result.json");
    let answers = ystudio_core::setup::Answers { shared_icons: Some(String::new()), ..Default::default() };
    let proc = run(ystudio_core::setup::setup_spec(&ws, &pwsh, &answers, &result).unwrap(), Duration::from_secs(600));
    let r = ystudio_core::setup::read_result(&result).unwrap();
    assert_eq!(r.succeeded, proc.succeeded());
    assert!(r.development_only, "{r:#?}");
    assert_eq!(r.steps.len(), 7);
    assert!(r.steps.iter().filter(|s| ["AppRegistration", "IntuneDefaults", "Email", "ScheduledTask"].contains(&s.step.as_str())).all(|s| s.level == "Skip"));
    let reopened = Workspace::open(&root, &[], &[]).unwrap();
    assert!(reopened.development_only);
}

/// Publishes a recipe from a throwaway clone (to a local bare "origin" made
/// from YSTUDIO_TEST_CLONE, never GitHub) and runs Yardstick's own
/// `Test-RecipeEvidence.ps1` on the branch the Studio pushed.
#[test]
#[ignore]
fn published_evidence_passes_yardsticks_recipe_evidence_check() {
    use ystudio_core::promote::{self, Evidence, Repo};
    use ystudio_core::results::{Phase, RecipeResult};
    use ystudio_core::validation::{Completion, Record, recipe_hash};
    let Some(src) = env_path("YSTUDIO_TEST_CLONE") else {
        eprintln!("YSTUDIO_TEST_CLONE not set - skipping");
        return;
    };
    let git_exe = ystudio_core::git::git_exe().unwrap();
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new(&git_exe).args(args).current_dir(cwd).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let tmp = tempfile::tempdir().unwrap();
    let (bare, work) = (tmp.path().join("origin.git"), tmp.path().join("work"));
    git(tmp.path(), &["clone", "-q", "--bare", &src.to_string_lossy(), &bare.to_string_lossy()]);
    git(tmp.path(), &["clone", "-q", &bare.to_string_lossy(), &work.to_string_lossy()]);
    git(&work, &["config", "user.name", "Studio Test"]);
    git(&work, &["config", "user.email", "studio@example.com"]);
    std::fs::create_dir_all(work.join("Local")).unwrap();
    std::fs::write(work.join(r"Local\preferences.yaml"), "developmentOnly: true\n").unwrap();
    let dev = work.join(r"Recipes\Development\7zip.yaml");
    std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
    let recipe = std::fs::read_to_string(work.join(r"Recipes\7zip.yaml")).unwrap();
    std::fs::write(&dev, format!("{}\n# Published by the Studio's live evidence test.\n", recipe.trim_end())).unwrap();

    let ws = Workspace::open(&work, &[], &[]).unwrap();
    let plan = promote::plan(&ws, "7zip").unwrap();
    let repo = Repo { remote_url: bare.to_string_lossy().into_owned(), web_url: "https://github.com/o/r".into(), base: "main".into() };
    let phases = ["Schema", "Download", "PreDetect", "Install", "PostInstallDetect", "Uninstall", "PostUninstallDetect", "Residue"]
        .map(|n| Phase { name: Some(n.into()), status: "Pass".into(), ..Default::default() })
        .to_vec();
    let result = RecipeResult { id: "7zip".into(), phases, ..Default::default() };
    let record = Record { sha256: recipe_hash(&dev).unwrap(), warnings: false, tested: "now".into(), summary_dir: None };
    let check = |label: &str| {
        git(&work, &["fetch", "-q", "origin"]);
        git(&work, &["checkout", "-q", "-B", label, "origin/studio/7zip"]);
        let script = work.join(r"Tests\RecipeHarness\Test-RecipeEvidence.ps1");
        let summary = tmp.path().join(format!("{label}.md"));
        let pwsh = ystudio_core::setup::pwsh7(None).expect("PowerShell 7");
        let spec = ProcessSpec::new("Test-RecipeEvidence", pwsh, &work).args([
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &script.to_string_lossy(),
            "-BaseRef", "origin/main", "-SummaryPath", &summary.to_string_lossy(),
        ]);
        let proc = run(spec, Duration::from_secs(300));
        println!("{}", std::fs::read_to_string(&summary).unwrap_or_default());
        git(&work, &["checkout", "-q", "main"]);
        proc.succeeded()
    };

    let validated = Evidence::new("7zip", &Completion::Complete(record), Some(&result), Some(std::time::SystemTime::now()));
    promote::open_pull_request(&ws, &plan, &repo, None, &validated, &mut |l| println!("{l}")).unwrap();
    assert!(check("validated"), "the check rejected validated evidence");

    let untested = Evidence::new("7zip", &Completion::NotValidated, None, None);
    promote::open_pull_request(&ws, &plan, &repo, None, &untested, &mut |l| println!("{l}")).unwrap();
    assert!(!check("untested"), "the check accepted unvalidated evidence");
}
