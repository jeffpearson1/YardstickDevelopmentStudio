//! Reading the harness's evidence: `Artifacts\RecipeHarness\_install-tests\<ts>\summary.json`
//! and per-recipe `result.json`, plus lab run folders.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Phase {
    pub name: Option<String>,
    pub phase: Option<String>,
    pub status: String,
    pub detail: Option<String>,
    #[serde(rename = "DurationSec")]
    pub duration_sec: Option<f64>,
}

impl Phase {
    pub fn label(&self) -> &str {
        self.name.as_deref().or(self.phase.as_deref()).unwrap_or("?")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RecipeResult {
    pub id: String,
    pub recipe_path: Option<String>,
    pub version: Option<String>,
    pub file_name: Option<String>,
    #[serde(rename = "SHA256")]
    pub sha256: Option<String>,
    pub signature_status: Option<String>,
    pub signer: Option<String>,
    pub product_code: Option<String>,
    pub install_experience: Option<String>,
    pub install_command: Option<String>,
    pub uninstall_command: Option<String>,
    pub overall: String,
    #[serde(deserialize_with = "one_or_many")]
    pub phases: Vec<Phase>,
    pub output_directory: Option<String>,
    pub context: Option<String>,
    pub residue: Option<Value>,
}

/// The phases a live run must pass for the skill to call a recipe validated.
pub const VALIDATION_PHASES: &[&str] = &["Schema", "Download", "PreDetect", "Install", "PostDetect", "Uninstall", "PostUninstall", "Residue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every validation phase passed in one live run.
    Validated,
    /// Passed with warnings (e.g. residue, wrapper signature) - report them.
    ValidatedWithWarnings,
    /// A dry run (-WhatIf) passed; still needs a live run.
    DryRunPassed,
    Failed,
    Incomplete,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Validated => "Validated",
            Verdict::ValidatedWithWarnings => "Validated with warnings",
            Verdict::DryRunPassed => "Dry run passed - live test still required",
            Verdict::Failed => "Failed",
            Verdict::Incomplete => "Incomplete",
        }
    }
}

fn status_ok(s: &str) -> bool {
    s.eq_ignore_ascii_case("pass") || s.eq_ignore_ascii_case("warn")
}

impl RecipeResult {
    pub fn phase(&self, name: &str) -> Option<&Phase> {
        let canonical = |name: &str| match name.to_ascii_lowercase().as_str() {
            "postdetect" | "postinstalldetect" => "postinstalldetect".to_string(),
            "postuninstall" | "postuninstalldetect" => "postuninstalldetect".to_string(),
            _ => name.to_ascii_lowercase(),
        };
        let name = canonical(name);
        self.phases.iter().find(|p| canonical(p.label()) == name)
    }

    pub fn verdict(&self) -> Verdict {
        if self.overall.to_ascii_lowercase().starts_with("fail") || self.phases.iter().any(|p| p.status.eq_ignore_ascii_case("fail")) {
            return Verdict::Failed;
        }
        let all_live = VALIDATION_PHASES.iter().all(|n| self.phase(n).is_some_and(|p| status_ok(&p.status)));
        if all_live {
            let warned = VALIDATION_PHASES.iter().any(|n| self.phase(n).is_some_and(|p| p.status.eq_ignore_ascii_case("warn")));
            return if warned { Verdict::ValidatedWithWarnings } else { Verdict::Validated };
        }
        let dry = ["Schema", "Download", "PreDetect"].iter().all(|n| self.phase(n).is_some_and(|p| status_ok(&p.status)));
        if dry && self.phase("Install").is_none_or(|p| p.status.eq_ignore_ascii_case("skip") || p.status.eq_ignore_ascii_case("whatif")) {
            return Verdict::DryRunPassed;
        }
        Verdict::Incomplete
    }

    /// One line for status messages: the overall result and the phases that
    /// did not pass, with their details. Skipped phases are listed only when
    /// nothing worse is.
    pub fn problems(&self) -> String {
        let status = |p: &&Phase| p.status.to_ascii_lowercase();
        let mut bad: Vec<&Phase> = self.phases.iter().filter(|p| !matches!(status(p).as_str(), "pass" | "whatif" | "skip")).collect();
        if bad.is_empty() {
            bad = self.phases.iter().filter(|p| status(p) == "skip").collect();
        }
        let parts: Vec<String> = bad
            .iter()
            .map(|p| match p.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(detail) => format!("{} {} ({detail})", p.label(), p.status),
                None => format!("{} {}", p.label(), p.status),
            })
            .collect();
        let overall = if self.overall.is_empty() { "unknown" } else { self.overall.as_str() };
        if parts.is_empty() { format!("overall {overall}") } else { format!("overall {overall}: {}", parts.join("; ")) }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Summary {
    pub run_root: Option<String>,
    pub started_at: Option<String>,
    pub elevated: Option<bool>,
    pub machine: Option<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub results: Vec<RecipeResult>,
}

#[derive(Debug, Clone)]
pub struct InstallRun {
    pub dir: PathBuf,
    pub timestamp: String,
    pub summary: Option<Summary>,
    pub error: Option<String>,
}

impl InstallRun {
    pub fn summary_md(&self) -> Option<String> {
        crate::read_text(&self.dir.join("summary.md")).ok()
    }

    pub fn transcript(&self) -> Option<String> {
        crate::read_text(&self.dir.join("harness.transcript.log")).ok()
    }

    pub fn ids(&self) -> Vec<String> {
        self.summary.as_ref().map(|s| s.results.iter().map(|r| r.id.clone()).collect()).unwrap_or_default()
    }
}

/// Accepts `x`, `[x, ...]`, `{"value": [...], "Count": n}` (PowerShell 5.1) or null.
fn one_or_many<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    let v = match v {
        Value::Object(ref m) if m.contains_key("value") && m.contains_key("Count") => m["value"].clone(),
        other => other,
    };
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.into_iter().map(|i| serde_json::from_value(i).map_err(serde::de::Error::custom)).collect(),
        single => Ok(vec![serde_json::from_value(single).map_err(serde::de::Error::custom)?]),
    }
}

pub fn parse_summary(text: &str) -> Result<Summary, serde_json::Error> {
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
}

pub fn parse_result(text: &str) -> Result<RecipeResult, serde_json::Error> {
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
}

fn load_run(dir: &Path) -> InstallRun {
    let timestamp = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let summary_path = dir.join("summary.json");
    let (summary, error) = if summary_path.is_file() {
        match crate::read_text(&summary_path).map_err(|e| e.to_string()).and_then(|t| parse_summary(&t).map_err(|e| e.to_string())) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("summary.json unreadable: {e}"))),
        }
    } else {
        // In progress, or the elevated child has not finished: fall back to per-recipe results.
        let mut results = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let rp = entry.path().join("result.json");
            if let Ok(r) = crate::read_text(&rp).map_err(|e| e.to_string()).and_then(|t| parse_result(&t).map_err(|e| e.to_string())) {
                results.push(r);
            }
        }
        if results.is_empty() {
            (None, Some("No summary.json yet - the run may still be in progress or was interrupted.".into()))
        } else {
            (Some(Summary { results, ..Default::default() }), Some("Partial: summary.json not written yet.".into()))
        }
    };
    InstallRun { dir: dir.to_path_buf(), timestamp, summary, error }
}

/// Install-test runs, newest first.
pub fn list_install_runs(harness_artifacts: &Path) -> Vec<InstallRun> {
    let root = harness_artifacts.join("_install-tests");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.reverse();
    dirs.iter().map(|d| load_run(d)).collect()
}

/// The newest install-test run directory created at or after `since`.
pub fn newest_run_since(harness_artifacts: &Path, since: std::time::SystemTime) -> Option<InstallRun> {
    let root = harness_artifacts.join("_install-tests");
    std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.metadata().and_then(|m| m.created().or_else(|_| m.modified())).is_ok_and(|t| t >= since))
        .map(|e| e.path())
        .max()
        .map(|d| load_run(&d))
}

#[derive(Debug, Clone)]
pub struct LabRun {
    pub id: String,
    pub dir: PathBuf,
    pub timestamp: String,
    pub files: Vec<String>,
}

/// Lab (download/package) runs for one recipe, newest first.
pub fn list_lab_runs(harness_artifacts: &Path, id: &str) -> Vec<LabRun> {
    let root = harness_artifacts.join(id);
    let mut runs: Vec<LabRun> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let dir = e.path();
            let mut files: Vec<String> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|f| f.file_name().to_string_lossy().into_owned()).collect();
            files.sort();
            LabRun { id: id.to_string(), timestamp: e.file_name().to_string_lossy().into_owned(), dir, files }
        })
        .collect();
    runs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(list: &[(&str, &str)]) -> String {
        let items: Vec<String> = list
            .iter()
            .map(|(n, s)| format!(r#"{{"Name":"{n}","Phase":"{n}","Status":"{s}","Detail":"d","DurationSec":1.5}}"#))
            .collect();
        format!("[{}]", items.join(","))
    }

    fn result(id: &str, overall: &str, ph: &[(&str, &str)]) -> String {
        format!(r#"{{"Id":"{id}","Version":"1.0","Overall":"{overall}","Phases":{}}}"#, phases(ph))
    }

    const LIVE_PASS: &[(&str, &str)] = &[
        ("Schema", "Pass"),
        ("Download", "Pass"),
        ("PreDetect", "Pass"),
        ("Install", "Pass"),
        ("PostDetect", "Pass"),
        ("Uninstall", "Pass"),
        ("PostUninstall", "Pass"),
        ("Residue", "Pass"),
    ];

    #[test]
    fn parses_summary_with_bom_and_single_result() {
        let text = format!("\u{feff}{{\"RunRoot\":\"x\",\"Machine\":\"M\",\"Elevated\":false,\"Results\":{}}}", result("calibre", "Pass", LIVE_PASS));
        let s = parse_summary(&text).unwrap();
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.results[0].phases.len(), 8);
        assert_eq!(s.results[0].verdict(), Verdict::Validated);
    }

    #[test]
    fn parses_ps51_wrapped_list() {
        let text = format!(r#"{{"Results":{{"value":[{}],"Count":1}}}}"#, result("a", "Pass", LIVE_PASS));
        assert_eq!(parse_summary(&text).unwrap().results.len(), 1);
    }

    #[test]
    fn verdicts() {
        let mut warn = LIVE_PASS.to_vec();
        warn[7] = ("Residue", "Warn");
        let r = parse_result(&result("a", "Pass", &warn)).unwrap();
        assert_eq!(r.verdict(), Verdict::ValidatedWithWarnings);

        let dry = parse_result(&result("a", "Pass", &[("Schema", "Pass"), ("Download", "Warn"), ("PreDetect", "Pass"), ("Install", "Skip")])).unwrap();
        assert_eq!(dry.verdict(), Verdict::DryRunPassed);

        let fail = parse_result(&result("a", "Fail", &[("Schema", "Pass"), ("Download", "Fail")])).unwrap();
        assert_eq!(fail.verdict(), Verdict::Failed);

        let partial = parse_result(&result("a", "Pass", &[("Schema", "Pass")])).unwrap();
        assert_eq!(partial.verdict(), Verdict::Incomplete);
    }

    #[test]
    fn problems_name_the_phases_that_did_not_pass() {
        let phase = |name: &str, status: &str, detail: &str| Phase {
            name: Some(name.into()), status: status.into(), detail: Some(detail.into()), ..Default::default()
        };
        let blocked = RecipeResult {
            overall: "Blocked".into(),
            phases: vec![phase("Schema", "Pass", ""), phase("PreDetect", "Blocked", "App is already installed"), phase("Install", "Skip", "WhatIf")],
            ..Default::default()
        };
        assert_eq!(blocked.problems(), "overall Blocked: PreDetect Blocked (App is already installed)");
        let skipped = RecipeResult { overall: "Pass".into(), phases: vec![phase("Install", "Skip", "")], ..Default::default() };
        assert_eq!(skipped.problems(), "overall Pass: Install Skip");
        assert_eq!(RecipeResult::default().problems(), "overall unknown");
    }

    #[test]
    fn recognizes_current_harness_detection_phase_names() {
        let phases: Vec<_> = LIVE_PASS
            .iter()
            .map(|&(name, status)| {
                let name = match name {
                    "PostDetect" => "PostInstallDetect",
                    "PostUninstall" => "PostUninstallDetect",
                    _ => name,
                };
                (name, status)
            })
            .collect();
        let r = parse_result(&result("githubcopilotcli", "Pass", &phases)).unwrap();
        assert_eq!(r.verdict(), Verdict::Validated);

        let mut warned = phases.clone();
        warned[7].1 = "Warn";
        let r = parse_result(&result("githubcopilotcli", "Pass with warnings", &warned)).unwrap();
        assert_eq!(r.verdict(), Verdict::ValidatedWithWarnings);

        let mut skipped = phases[..5].to_vec();
        skipped.push(("Uninstall", "Skip"));
        let r = parse_result(&result("githubcopilotcli", "Pass", &skipped)).unwrap();
        assert_eq!(r.verdict(), Verdict::Incomplete);

        let mut failed = phases;
        failed[4].1 = "Fail";
        let r = parse_result(&result("githubcopilotcli", "Fail", &failed)).unwrap();
        assert_eq!(r.verdict(), Verdict::Failed);
    }

    #[test]
    fn lists_runs_newest_first_with_partial_fallback() {
        let d = tempfile::tempdir().unwrap();
        let tests = d.path().join("_install-tests");
        let old = tests.join("20260101-000000");
        let new = tests.join("20260202-000000");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(new.join("brave")).unwrap();
        std::fs::write(old.join("summary.json"), format!(r#"{{"Results":[{}]}}"#, result("a", "Pass", LIVE_PASS))).unwrap();
        std::fs::write(new.join(r"brave\result.json"), result("brave", "Fail", &[("Download", "Fail")])).unwrap();
        let runs = list_install_runs(d.path());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].timestamp, "20260202-000000");
        assert_eq!(runs[0].ids(), vec!["brave"]);
        assert!(runs[0].error.as_deref().unwrap().contains("Partial"));
        assert!(runs[1].error.is_none());
    }
}
