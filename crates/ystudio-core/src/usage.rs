//! What agent runs cost. Copilot CLI writes usage statistics when a run ends
//! (`--usage-output-file`): AI credits per model and per agent (the main agent
//! and each subagent). The Studio records one line per run in
//! `%APPDATA%\YardstickStudio\usage.jsonl`, so it can show credits per run
//! and, per model, the average a run and an app (recipe) cost.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A run whose usage file is read when it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Where the CLI writes its usage JSON.
    pub file: PathBuf,
    /// `create`, `fix`, `review`, `update`, `freeform` or `extract`.
    pub kind: String,
    /// The recipe the run worked on, when there is one.
    pub recipe: Option<String>,
    pub backend: String,
    pub multi_agent: bool,
}

impl Pending {
    /// A fresh usage file path in the temp folder.
    pub fn new(kind: &str, recipe: Option<String>, backend: &str, multi_agent: bool) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let file = std::env::temp_dir().join(format!("ystudio-usage-{}-{nanos}.json", std::process::id()));
        Pending { file, kind: kind.to_string(), recipe, backend: backend.to_string(), multi_agent }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageRecord {
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub at: String,
    pub backend: String,
    pub kind: String,
    pub recipe: Option<String>,
    /// The main agent's model.
    pub model: String,
    /// AI credits for the whole run, subagents included.
    pub credits: f64,
    /// Credits by model, when subagents used other models.
    pub models: BTreeMap<String, f64>,
    pub subagents: u32,
    pub multi_agent: bool,
}

impl Default for UsageRecord {
    fn default() -> Self {
        UsageRecord {
            at: String::new(),
            backend: String::new(),
            kind: String::new(),
            recipe: None,
            model: String::new(),
            credits: 0.0,
            models: BTreeMap::new(),
            subagents: 0,
            multi_agent: false,
        }
    }
}

/// Reads and removes the usage file of a finished run.
pub fn read(pending: &Pending) -> Result<UsageRecord> {
    let text = crate::read_text(&pending.file).with_context(|| format!("the agent did not write {}", pending.file.display()))?;
    let _ = std::fs::remove_file(&pending.file);
    parse(&text, pending)
}

const NANO: f64 = 1_000_000_000.0;

fn parse(json: &str, pending: &Pending) -> Result<UsageRecord> {
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Metrics {
        total_nano_aiu: f64,
    }
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Usage {
        total_nano_aiu: f64,
        current_model: String,
        model_metrics: BTreeMap<String, Metrics>,
        agent_metrics: BTreeMap<String, serde_json::Value>,
    }
    let u: Usage = serde_json::from_str(json).context("reading the agent's usage statistics")?;
    let models: BTreeMap<String, f64> = u.model_metrics.into_iter().map(|(m, x)| (m, x.total_nano_aiu / NANO)).collect();
    // The main agent's model; the most expensive one if the CLI did not say.
    let model = if u.current_model.is_empty() {
        models.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(m, _)| m.clone()).unwrap_or_default()
    } else {
        u.current_model
    };
    Ok(UsageRecord {
        at: crate::intake::timestamp(),
        backend: pending.backend.clone(),
        kind: pending.kind.clone(),
        recipe: pending.recipe.clone(),
        model,
        credits: u.total_nano_aiu / NANO,
        models,
        subagents: u.agent_metrics.keys().filter(|k| k.as_str() != "main").count() as u32,
        multi_agent: pending.multi_agent,
    })
}

/// `usage.jsonl`: one record per line, appended as runs end.
pub struct UsageLog {
    path: PathBuf,
}

impl UsageLog {
    pub fn default_path() -> PathBuf {
        crate::settings::Settings::default_path().parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir).join("usage.jsonl")
    }

    pub fn at(path: PathBuf) -> Self {
        UsageLog { path }
    }

    pub fn append(&self, record: &UsageRecord) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(record)?)?;
        Ok(())
    }

    /// Every readable record; a damaged line is skipped, not fatal.
    pub fn load(&self) -> Vec<UsageRecord> {
        crate::read_text(&self.path)
            .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default()
    }
}

/// Credits for one model across the recorded runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub runs: usize,
    pub credits: f64,
    /// Distinct recipes worked on with this model.
    pub apps: usize,
    /// Credits of the runs that worked on a recipe.
    pub app_credits: f64,
}

impl ModelUsage {
    pub fn per_run(&self) -> f64 {
        if self.runs == 0 { 0.0 } else { self.credits / self.runs as f64 }
    }

    /// What getting one app (recipe) done costs on average: every run on it
    /// (writing, fixing, repairing) counted together.
    pub fn per_app(&self) -> Option<f64> {
        (self.apps > 0).then(|| self.app_credits / self.apps as f64)
    }
}

/// Usage grouped by the main agent's model, sorted by model name.
pub fn by_model(records: &[UsageRecord]) -> Vec<ModelUsage> {
    let mut groups: BTreeMap<&str, (usize, f64, BTreeSet<String>, f64)> = BTreeMap::new();
    for r in records {
        let g = groups.entry(if r.model.is_empty() { "unknown" } else { r.model.as_str() }).or_default();
        g.0 += 1;
        g.1 += r.credits;
        if let Some(id) = &r.recipe {
            g.2.insert(id.to_lowercase());
            g.3 += r.credits;
        }
    }
    groups
        .into_iter()
        .map(|(model, (runs, credits, apps, app_credits))| ModelUsage { model: model.to_string(), runs, credits, apps: apps.len(), app_credits })
        .collect()
}

/// Credits spent on a recipe across all its runs.
pub fn recipe_credits(records: &[UsageRecord], id: &str) -> f64 {
    records.iter().filter(|r| r.recipe.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(id))).map(|r| r.credits).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `copilot --fleet --usage-output-file` run.
    const FLEET: &str = r#"{
      "totalPremiumRequestCost": 0.33, "totalNanoAiu": 9191115000, "aiCreditsStatus": "complete",
      "modelMetrics": { "claude-haiku-4.5": { "requests": { "count": 9, "cost": 0.33 }, "totalNanoAiu": 9191115000 } },
      "agentMetrics": {
        "main": { "totalNanoAiu": 4213540000 },
        "a93dc57a-4a80-4012-8d6d-3879630479f9": { "agentName": "general-purpose", "agentDisplayName": "Alpha Responder", "totalNanoAiu": 2523350000 },
        "e6296b87-b227-4923-8015-304c7f0969c3": { "agentName": "general-purpose", "agentDisplayName": "Beta Responder", "totalNanoAiu": 2454225000 }
      },
      "currentModel": "claude-haiku-4.5"
    }"#;

    fn pending(recipe: Option<&str>) -> Pending {
        Pending { file: PathBuf::new(), kind: "create".into(), recipe: recipe.map(String::from), backend: "GitHub Copilot CLI".into(), multi_agent: true }
    }

    #[test]
    fn reads_copilot_usage() {
        let r = parse(FLEET, &pending(Some("obsidian"))).unwrap();
        assert!((r.credits - 9.191115).abs() < 1e-9);
        assert_eq!(r.model, "claude-haiku-4.5");
        assert_eq!(r.subagents, 2);
        assert!((r.models["claude-haiku-4.5"] - 9.191115).abs() < 1e-9);
        assert!(r.multi_agent && r.recipe.as_deref() == Some("obsidian"));
        assert!(parse("not json", &pending(None)).is_err());
    }

    #[test]
    fn averages_per_run_and_per_app_by_model() {
        let rec = |model: &str, recipe: Option<&str>, credits: f64| UsageRecord { model: model.into(), recipe: recipe.map(String::from), credits, ..Default::default() };
        let records = vec![
            rec("gpt-5.4", Some("7zip"), 4.0),
            rec("gpt-5.4", Some("7zip"), 2.0), // a repair of the same app
            rec("gpt-5.4", Some("brave"), 3.0),
            rec("gpt-5.4", None, 1.0), // request extraction: a run, not an app
            rec("claude-opus-5.5", Some("Brave"), 10.0),
        ];
        let m = by_model(&records);
        assert_eq!(m.iter().map(|x| x.model.as_str()).collect::<Vec<_>>(), ["claude-opus-5.5", "gpt-5.4"]);
        let gpt = &m[1];
        assert_eq!((gpt.runs, gpt.apps), (4, 2));
        assert!((gpt.per_run() - 2.5).abs() < 1e-9);
        assert!((gpt.per_app().unwrap() - 4.5).abs() < 1e-9);
        assert_eq!(recipe_credits(&records, "brave"), 13.0);
        assert!(by_model(&[rec("x", None, 1.0)])[0].per_app().is_none());
    }

    #[test]
    fn log_appends_and_skips_damaged_lines() {
        let d = tempfile::tempdir().unwrap();
        let log = UsageLog::at(d.path().join("usage.jsonl"));
        log.append(&parse(FLEET, &pending(Some("a"))).unwrap()).unwrap();
        std::fs::write(d.path().join("usage.jsonl"), std::fs::read_to_string(d.path().join("usage.jsonl")).unwrap() + "garbage\n").unwrap();
        log.append(&parse(FLEET, &pending(None)).unwrap()).unwrap();
        let all = log.load();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].recipe.as_deref(), Some("a"));
    }
}
