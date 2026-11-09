//! Which recipe contents passed a live install test.
//!
//! The harness's summaries record a verdict but not what the recipe said when
//! it was tested, so an edit after a passing run would still look validated.
//! The Studio keeps its own ledger beside the harness evidence: a recipe is
//! complete only while its file still hashes to what was tested.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::results::Verdict;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// [`recipe_hash`] of the file that was tested.
    pub sha256: String,
    pub warnings: bool,
    /// Harness run timestamp.
    pub tested: String,
    pub summary_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// No live install test has passed for this recipe.
    NotValidated,
    /// The recipe is exactly what passed its live install test.
    Complete(Record),
    /// A live install test passed, but the recipe has changed since.
    Changed(Record),
}

impl Completion {
    pub fn is_complete(&self) -> bool {
        matches!(self, Completion::Complete(_))
    }
}

#[derive(Debug, Default)]
pub struct Ledger {
    path: PathBuf,
    /// Keyed by lower-case recipe id.
    records: BTreeMap<String, Record>,
}

impl Ledger {
    pub fn path(harness_artifacts: &Path) -> PathBuf {
        harness_artifacts.join(r"_studio\validated.json")
    }

    /// An unreadable ledger is treated as empty: it only ever grants status, so
    /// losing it means re-testing, never publishing something untested.
    pub fn load(harness_artifacts: &Path) -> Self {
        let path = Self::path(harness_artifacts);
        let records = crate::read_text(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ledger { path, records }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.records)?).with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&Record> {
        self.records.get(&id.to_lowercase())
    }

    /// Records a passing live run of the recipe at `recipe`, unless the file
    /// changed after `started` (then the run tested something else). Returns
    /// whether a record was written.
    pub fn record(&mut self, id: &str, recipe: &Path, started: SystemTime, verdict: Verdict, tested: &str, summary_dir: Option<PathBuf>) -> Result<bool> {
        if !matches!(verdict, Verdict::Validated | Verdict::ValidatedWithWarnings) {
            return Ok(false);
        }
        let modified = std::fs::metadata(recipe).and_then(|m| m.modified()).with_context(|| format!("reading {}", recipe.display()))?;
        if modified > started {
            return Ok(false);
        }
        let record = Record {
            sha256: recipe_hash(recipe)?,
            warnings: verdict == Verdict::ValidatedWithWarnings,
            tested: tested.to_string(),
            summary_dir,
        };
        self.records.insert(id.to_lowercase(), record);
        Ok(true)
    }

    pub fn completion(&self, id: &str, recipe: &Path) -> Completion {
        let Some(record) = self.get(id) else { return Completion::NotValidated };
        match recipe_hash(recipe) {
            Ok(h) if h == record.sha256 => Completion::Complete(record.clone()),
            _ => Completion::Changed(record.clone()),
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.records.keys().map(String::as_str)
    }
}

/// SHA-256 of a recipe, ignoring a UTF-8 BOM and CRLF/LF differences so a git
/// checkout's line endings do not change it.
pub fn recipe_hash(path: &Path) -> Result<String> {
    let text = crate::read_text(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hash_text(&text))
}

pub fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.replace("\r\n", "\n").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn line_endings_and_bom_do_not_change_the_hash() {
        assert_eq!(hash_text("a: 1\r\nb: 2\r\n"), hash_text("a: 1\nb: 2\n"));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.yaml");
        std::fs::write(&p, b"\xEF\xBB\xBFa: 1\r\n").unwrap();
        assert_eq!(recipe_hash(&p).unwrap(), hash_text("a: 1\n"));
    }

    #[test]
    fn complete_until_the_recipe_changes() {
        let d = tempfile::tempdir().unwrap();
        let recipe = d.path().join("obsidian.yaml");
        std::fs::write(&recipe, "id: obsidian\n").unwrap();
        let started = SystemTime::now() + Duration::from_secs(1);
        let mut ledger = Ledger::load(d.path());
        assert_eq!(ledger.completion("obsidian", &recipe), Completion::NotValidated);
        assert!(!ledger.record("obsidian", &recipe, started, Verdict::DryRunPassed, "t", None).unwrap());
        assert!(!ledger.record("obsidian", &recipe, started, Verdict::Failed, "t", None).unwrap());
        assert!(ledger.record("Obsidian", &recipe, started, Verdict::Validated, "2026-11-09 10:00:00", None).unwrap());
        ledger.save().unwrap();

        let ledger = Ledger::load(d.path());
        assert!(ledger.completion("obsidian", &recipe).is_complete());
        std::fs::write(&recipe, "id: obsidian\nversion: 2\n").unwrap();
        assert!(matches!(ledger.completion("obsidian", &recipe), Completion::Changed(_)));
    }

    #[test]
    fn edits_during_the_run_are_not_recorded() {
        let d = tempfile::tempdir().unwrap();
        let recipe = d.path().join("x.yaml");
        std::fs::write(&recipe, "id: x\n").unwrap();
        let mut ledger = Ledger::load(d.path());
        let started = SystemTime::now() - Duration::from_secs(60);
        assert!(!ledger.record("x", &recipe, started, Verdict::Validated, "t", None).unwrap());
        assert!(ledger.get("x").is_none());
    }

    #[test]
    fn unreadable_ledger_is_empty() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("_studio")).unwrap();
        std::fs::write(Ledger::path(d.path()), "not json").unwrap();
        assert_eq!(Ledger::load(d.path()).ids().count(), 0);
    }
}
