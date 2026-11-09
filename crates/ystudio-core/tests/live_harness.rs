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
