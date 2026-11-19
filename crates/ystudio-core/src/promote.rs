//! Promotion by pull request. A development recipe is committed to a branch
//! cut from the upstream default branch, in a temporary worktree so the
//! user's checkout and `Recipes\Development` are untouched, then pushed and
//! offered as a pull request. Production pulls merged work itself; the Studio
//! never writes to a production folder.
//!
//! Icons are not kept in git: promotion copies a recipe's icon to the shared
//! icon folder (`SharedIcons` in preferences), which Yardstick syncs from.
//!
//! Each pull request carries the recipe's test evidence
//! (`Tests/RecipeEvidence/<id>.json`), which Yardstick's required `Recipe
//! evidence` check reads: it passes only when a live install test validated
//! exactly the recipe contents being merged.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::Value;

use crate::process::{Output, ProcessSpec, clean_line, one_line, run_bytes, run_output};
use crate::results::RecipeResult;
use crate::validation::{Completion, hash_bytes};
use crate::workspace::{RecipeLocation, Workspace};

/// Branches the Studio pushes live under this prefix.
pub const BRANCH_PREFIX: &str = "studio/";
/// Branches of pull requests that carry several recipes: `studio/batch-<stamp>-<n>`.
pub const COMBINED_PREFIX: &str = "studio/batch-";
/// Lists a combined pull request's recipes in its description, so the Studio
/// can track each recipe (status, merged-recipe cleanup) like a single one.
const RECIPES_MARKER: &str = "<!-- studio-recipes:";

/// Where Yardstick's `Recipe evidence` check looks for a recipe's evidence.
pub const EVIDENCE_DIR: &str = "Tests/RecipeEvidence";

/// What the Studio knows about a recipe's testing, committed with it as
/// `Tests/RecipeEvidence/<id>.json` (schemaVersion 1; see Yardstick's
/// `Docs/RecipeEvidence.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub schema_version: u32,
    pub recipe_id: String,
    /// Repository-relative, forward slashes; filled in from the commit.
    pub recipe_path: String,
    /// The hash of the recipe as committed; filled in from the commit.
    pub recipe_sha256: String,
    pub validated: bool,
    /// `Validated`, `ValidatedWithWarnings` or `NotValidated`.
    pub verdict: String,
    /// Why it is not validated; empty when it is.
    pub reason: String,
    /// When the install test ran, ISO 8601 UTC; empty when none is known.
    pub tested_at: String,
    pub studio_version: String,
    pub install_experience: String,
    pub installer: Installer,
    pub phases: Vec<EvidencePhase>,
    /// The hash the live test validated, to notice a clean filter rewriting the recipe.
    #[serde(skip)]
    tested_sha256: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installer {
    pub file_name: String,
    pub version: String,
    pub sha256: String,
    pub signer: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvidencePhase {
    pub name: String,
    pub status: String,
    pub detail: String,
}

/// `t` as ISO 8601 UTC, e.g. `2026-11-09T10:00:00Z`.
pub fn iso_utc(t: SystemTime) -> String {
    format!("{}Z", crate::intake::utc_timestamp(t).replace(' ', "T"))
}

impl Evidence {
    /// Evidence for `id`: whether it is complete, and the install test that
    /// validated it (or the latest one) with when that ran.
    pub fn new(id: &str, completion: &Completion, result: Option<&RecipeResult>, tested_at: Option<SystemTime>) -> Self {
        let tested_at = tested_at.map(iso_utc).unwrap_or_default();
        let (validated, verdict, reason, tested_sha256) = match completion {
            Completion::Complete(r) => {
                (true, if r.warnings { "ValidatedWithWarnings" } else { "Validated" }, String::new(), Some(r.sha256.clone()))
            }
            // Yardstick's check quotes the reason inside its own sentence, so no full stop.
            Completion::Changed(r) => (false, "NotValidated", format!("a live install test passed on {}, but the recipe has changed since", r.tested), None),
            Completion::NotValidated => {
                let reason = match result {
                    Some(r) => format!("no live install test of this recipe has passed; the latest install test was: {}", r.verdict().label()),
                    None => "no install test of this recipe has been recorded".to_string(),
                };
                (false, "NotValidated", reason, None)
            }
        };
        let text = |s: &Option<String>| s.clone().unwrap_or_default();
        Evidence {
            schema_version: 1,
            recipe_id: id.to_string(),
            recipe_path: String::new(),
            recipe_sha256: String::new(),
            validated,
            verdict: verdict.to_string(),
            reason,
            tested_at,
            studio_version: env!("CARGO_PKG_VERSION").to_string(),
            install_experience: result.map(|r| text(&r.install_experience)).unwrap_or_default(),
            installer: result
                .map(|r| Installer { file_name: text(&r.file_name), version: text(&r.version), sha256: text(&r.sha256), signer: text(&r.signer) })
                .unwrap_or_default(),
            phases: result
                .map(|r| r.phases.iter().map(|p| EvidencePhase { name: p.label().to_string(), status: p.status.clone(), detail: text(&p.detail) }).collect())
                .unwrap_or_default(),
            tested_sha256,
        }
    }

    /// A sentence for the commit and the pull request.
    pub fn summary(&self) -> String {
        let when = if self.tested_at.is_empty() { String::new() } else { format!(" on {}", self.tested_at) };
        if self.validated {
            let warnings = if self.verdict == "ValidatedWithWarnings" { " with warnings" } else { "" };
            let hash = if self.recipe_sha256.is_empty() { self.tested_sha256.clone().unwrap_or_default() } else { self.recipe_sha256.clone() };
            format!("Validated: a live install test passed{warnings}{when} with exactly this recipe (SHA-256 {hash}).")
        } else {
            format!("Not validated: {}.", self.reason)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub id: String,
    /// The development recipe being promoted.
    pub source: PathBuf,
    /// Where it goes, relative to the repository root, e.g. `Recipes\7zip.yaml`.
    pub dest: PathBuf,
    pub branch: String,
    /// The recipe's icon and where it is copied in the shared icon folder.
    pub icon: Option<(PathBuf, PathBuf)>,
    /// Why no icon is copied, when there is a reason worth reporting.
    pub icon_note: Option<String>,
}

/// What a promotion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `gh pr create` opened it, or one was already open for the branch.
    PullRequest { url: String, existing: bool },
    /// The branch is pushed; GitHub's compare page creates the pull request.
    Compare { url: String },
}

impl Outcome {
    pub fn url(&self) -> &str {
        match self {
            Outcome::PullRequest { url, .. } | Outcome::Compare { url } => url,
        }
    }
}

/// The upstream repository the workspace was cloned from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub remote_url: String,
    /// `https://github.com/owner/repo`.
    pub web_url: String,
    /// The upstream default branch, e.g. `main`.
    pub base: String,
}

/// One open pull request created by the Studio, including the GitHub state
/// needed by the Publish page. Strings intentionally preserve GitHub's
/// vocabulary so newer check and review states remain visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestStatus {
    pub number: u64,
    pub recipe_id: String,
    /// The head branch: `studio/<id>`, or a combined `studio/batch-…`.
    pub branch: String,
    pub title: String,
    pub url: String,
    /// `OPEN`, `MERGED`, or `CLOSED` from GitHub.
    pub state: String,
    pub merged_at: String,
    pub draft: bool,
    pub merge_state: String,
    pub review_decision: String,
    pub updated_at: String,
    pub checks: Vec<PullCheck>,
    pub activity: Vec<PullActivity>,
}

/// What happened when a merged pull request was reconciled with its local
/// Development recipe. A successful retirement is recoverable from `archive`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retirement {
    pub recipe_id: String,
    pub retired: bool,
    pub archive: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullCheck {
    pub name: String,
    pub workflow: String,
    pub state: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullActivity {
    pub author: String,
    pub kind: String,
    pub body: String,
    pub at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenPullRequest {
    number: u64,
    url: String,
}

fn git(git: &Path, cwd: &Path, args: &[&str]) -> ProcessSpec {
    // Never block on a credential prompt nobody can see.
    ProcessSpec::new("git", git, cwd).args(args.iter().copied()).env("GIT_TERMINAL_PROMPT", "0")
}

/// Runs git, logging the command; fails with git's own message.
fn run(git_exe: &Path, cwd: &Path, args: &[&str], log: &mut dyn FnMut(&str)) -> Result<String> {
    // One log row per command, even for a multi-line commit message.
    log(&format!("> git {}", one_line(&args.join(" "))));
    let out = run_output(&git(git_exe, cwd, args))?;
    // Progress meters ("Updating files: 7%\r…") keep only their final frame.
    for line in stderr_lines(&out.stderr) {
        log(&line);
    }
    if !out.success {
        bail!("git {} failed: {}", args.first().copied().unwrap_or_default(), last_line(&out));
    }
    Ok(out.stdout)
}

/// git's stderr as log lines: colours and '\r' progress frames removed, blanks skipped.
fn stderr_lines(stderr: &str) -> impl Iterator<Item = String> + '_ {
    stderr.lines().map(clean_line).filter(|l| !l.trim().is_empty())
}

/// The most telling line: the last one on stderr (git reports errors there), else on stdout.
fn last_line(out: &Output) -> String {
    out.stderr.lines().rev().chain(out.stdout.lines().rev()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("no output").to_string()
}

pub fn repo(ws: &Workspace) -> Result<Repo> {
    let git_exe = crate::git::git_exe()?;
    let get = |args: &[&str]| run_output(&git(&git_exe, &ws.root, args)).map(|o| o.success.then(|| o.stdout.trim().to_string()));
    let remote_url = get(&["remote", "get-url", "origin"])?.ok_or_else(|| anyhow!("This clone has no 'origin' remote."))?;
    let web_url = web_url(&remote_url).ok_or_else(|| anyhow!("'{remote_url}' is not a GitHub-style remote."))?;
    // origin/HEAD names the default branch; clones made by git have it.
    let base = get(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])?
        .and_then(|r| r.strip_prefix("origin/").map(str::to_string))
        .unwrap_or_else(|| "main".to_string());
    Ok(Repo { remote_url, web_url, base })
}

/// `https://host/owner/repo` for an https, `git@host:` or `ssh://` remote.
pub fn web_url(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches('/');
    let r = r.strip_suffix(".git").unwrap_or(r);
    let (host, path) = if let Some(rest) = r.strip_prefix("https://").or_else(|| r.strip_prefix("http://")) {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        rest.split_once('/')?
    } else if let Some(rest) = r.strip_prefix("ssh://") {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?, path)
    } else {
        let rest = r.rsplit_once('@').map_or(r, |(_, h)| h);
        rest.split_once(':')?
    };
    let path = path.trim_matches('/');
    (!host.is_empty() && path.split('/').count() == 2 && !path.split('/').any(str::is_empty)).then(|| format!("https://{host}/{path}"))
}

/// Where `id` goes and what else moves with it. Only recipes in
/// `Recipes\Development` are promoted; a shipped recipe of the same id keeps
/// its folder (e.g. `Recipes\Jetbrains`), as `Publish-Yardstick.ps1` does.
pub fn plan(ws: &Workspace, id: &str) -> Result<Plan> {
    ws.ensure_writable("open a pull request from")?;
    if !crate::recipe::is_valid_id(id) {
        bail!("Invalid recipe id '{id}'.");
    }
    let source = ws.development_recipe_path(id);
    if !source.is_file() {
        bail!("{} is not in Recipes\\Development; only development recipes are promoted.", id);
    }
    let recipes = ws.list_recipes();
    let disabled = |p: &Path| p.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Disabled"));
    let shipped: Vec<&PathBuf> = recipes
        .iter()
        .filter(|r| r.id.eq_ignore_ascii_case(id) && r.location != RecipeLocation::Development && !disabled(&r.path))
        .map(|r| &r.path)
        .collect();
    if shipped.len() > 1 {
        bail!("Recipe '{id}' exists in more than one folder: {}", shipped.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
    }
    let target = shipped.first().map(|p| p.to_path_buf()).unwrap_or_else(|| ws.layout.recipes.join(format!("{id}.yaml")));
    let dest = target.strip_prefix(&ws.root).map(Path::to_path_buf).map_err(|_| anyhow!("{} is outside the repository.", target.display()))?;

    let icon_file = recipes
        .iter()
        .find(|r| r.location == RecipeLocation::Development && r.id.eq_ignore_ascii_case(id))
        .map(|r| r.meta.icon_file.trim().to_string())
        .unwrap_or_default();
    let (icon, icon_note) = if icon_file.is_empty() {
        (None, Some("The recipe has no iconFile.".to_string()))
    } else if !ws.layout.icons.join(&icon_file).is_file() {
        (None, Some(format!("Icons\\{icon_file} does not exist, so no icon was copied.")))
    } else {
        match &ws.shared_icons {
            Some(share) => (Some((ws.layout.icons.join(&icon_file), share.join(&icon_file))), None),
            None => (None, Some(format!("No shared icon folder (SharedIcons) is set in preferences, so Icons\\{icon_file} was not copied anywhere."))),
        }
    };
    Ok(Plan { id: id.to_string(), source, dest, branch: format!("{BRANCH_PREFIX}{id}"), icon, icon_note })
}

/// Removes the temporary worktree and its local branch, however the promotion ends.
struct Worktree<'a> {
    git: &'a Path,
    repo: &'a Path,
    dir: PathBuf,
    branch: String,
}

impl Drop for Worktree<'_> {
    fn drop(&mut self) {
        let dir = self.dir.to_string_lossy().into_owned();
        let _ = run_output(&git(self.git, self.repo, &["worktree", "remove", "--force", &dir]));
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = run_output(&git(self.git, self.repo, &["worktree", "prune"]));
        let _ = run_output(&git(self.git, self.repo, &["branch", "-D", &self.branch]));
    }
}

fn repo_slug(repo: &Repo) -> &str {
    let slug = repo.web_url.trim_start_matches("https://");
    slug.strip_prefix("github.com/").unwrap_or(slug)
}

fn gh_output(repo_root: &Path, gh: &Path, args: &[&str]) -> Result<Output> {
    let spec = crate::prereqs::with_user_env(
        ProcessSpec::new("gh", gh, repo_root)
            .args(args.iter().copied())
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1"),
    );
    run_output(&spec)
}

fn find_open_pull_request(root: &Path, repo: &Repo, gh_exe: &Path, branch: &str) -> Result<Option<OpenPullRequest>> {
    let out = gh_output(
        root,
        gh_exe,
        &["pr", "list", "--repo", repo_slug(repo), "--state", "open", "--head", branch, "--limit", "1", "--json", "number,url"],
    )?;
    if !out.success {
        bail!("gh could not check for an existing pull request: {}", last_line(&out));
    }
    let rows: Value = serde_json::from_str(&out.stdout).context("reading GitHub's pull request list")?;
    Ok(rows.as_array().and_then(|a| a.first()).and_then(|v| {
        Some(OpenPullRequest { number: v.get("number")?.as_u64()?, url: v.get("url")?.as_str()?.to_string() })
    }))
}

/// Lists Studio pull requests with their checks, comments and reviews. Merged
/// requests are included so the caller can retire their Development copies.
/// The caller should run this off the UI thread because `gh` contacts GitHub.
pub fn pull_requests(root: &Path, repo: &Repo, gh_exe: &Path) -> Result<Vec<PullRequestStatus>> {
    let fields = "number,title,body,url,headRefName,state,mergedAt,isDraft,mergeStateStatus,reviewDecision,statusCheckRollup,comments,reviews,updatedAt";
    let out = gh_output(root, gh_exe, &["pr", "list", "--repo", repo_slug(repo), "--state", "all", "--limit", "100", "--json", fields])?;
    if !out.success {
        bail!("GitHub pull requests could not be refreshed: {}", last_line(&out));
    }
    parse_pull_requests(&out.stdout)
}

fn parse_pull_requests(json: &str) -> Result<Vec<PullRequestStatus>> {
    let rows: Value = serde_json::from_str(json).context("reading GitHub's pull request status")?;
    let mut prs = Vec::new();
    for row in rows.as_array().ok_or_else(|| anyhow!("GitHub returned an unexpected pull request list"))? {
        let branch = text_field(row, "headRefName");
        // A combined pull request stands for each recipe it lists.
        let recipe_ids: Vec<String> = if branch.starts_with(COMBINED_PREFIX) {
            marked_recipes(&text_field(row, "body"))
        } else {
            branch.strip_prefix(BRANCH_PREFIX).filter(|id| !id.is_empty()).map(|id| vec![id.to_string()]).unwrap_or_default()
        };
        if recipe_ids.is_empty() {
            continue;
        }
        let mut activity = Vec::new();
        for comment in array_field(row, "comments") {
            activity.push(PullActivity {
                author: nested_text(comment, "author", "login"),
                kind: "Comment".into(),
                body: text_field(comment, "body"),
                at: text_field(comment, "createdAt"),
                url: text_field(comment, "url"),
            });
        }
        for review in array_field(row, "reviews") {
            let state = text_field(review, "state");
            activity.push(PullActivity {
                author: nested_text(review, "author", "login"),
                kind: if state.is_empty() { "Review".into() } else { format!("Review · {state}") },
                body: text_field(review, "body"),
                at: text_field(review, "submittedAt"),
                url: text_field(review, "url"),
            });
        }
        activity.sort_by(|a, b| b.at.cmp(&a.at));
        let checks = array_field(row, "statusCheckRollup")
            .iter()
            .map(|check| {
                let typename = text_field(check, "__typename");
                let (name, state, url) = if typename == "StatusContext" {
                    (text_field(check, "context"), text_field(check, "state"), text_field(check, "targetUrl"))
                } else {
                    let conclusion = text_field(check, "conclusion");
                    let status = text_field(check, "status");
                    (text_field(check, "name"), if conclusion.is_empty() { status } else { conclusion }, text_field(check, "detailsUrl"))
                };
                PullCheck { name, workflow: text_field(check, "workflowName"), state, url }
            })
            .collect();
        let pr = PullRequestStatus {
            number: row.get("number").and_then(Value::as_u64).unwrap_or_default(),
            recipe_id: String::new(),
            branch: branch.clone(),
            title: text_field(row, "title"),
            url: text_field(row, "url"),
            state: text_field(row, "state"),
            merged_at: text_field(row, "mergedAt"),
            draft: row.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            merge_state: text_field(row, "mergeStateStatus"),
            review_decision: text_field(row, "reviewDecision"),
            updated_at: text_field(row, "updatedAt"),
            checks,
            activity,
        };
        prs.extend(recipe_ids.into_iter().map(|recipe_id| PullRequestStatus { recipe_id, ..pr.clone() }));
    }
    Ok(prs)
}

/// The recipe ids in a combined pull request's `<!-- studio-recipes: a, b -->` line.
fn marked_recipes(body: &str) -> Vec<String> {
    body.lines()
        .find_map(|l| l.trim().strip_prefix(RECIPES_MARKER))
        .map(|rest| {
            rest.trim_end_matches("-->")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| crate::recipe::is_valid_id(s))
                .collect()
        })
        .unwrap_or_default()
}

/// One pull request to open or update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub branch: String,
    pub ids: Vec<String>,
    /// The branch already has an open Studio pull request, which gets a new commit.
    pub existing: bool,
}

/// Splits the recipes being published into pull requests. A recipe that is
/// already in an open Studio pull request (its own or a combined one) keeps
/// building on it; the rest share new pull requests of at most `max` recipes.
/// A pull request of one recipe uses the classic `studio/<id>` branch.
pub fn group(ids: &[String], open: &[PullRequestStatus], max: usize, stamp: &str) -> Vec<Group> {
    let max = max.max(1);
    let mut groups: Vec<Group> = Vec::new();
    let mut fresh = Vec::new();
    for id in ids {
        let existing = open.iter().find(|pr| pr.state.eq_ignore_ascii_case("OPEN") && pr.recipe_id.eq_ignore_ascii_case(id));
        match existing {
            Some(pr) => match groups.iter_mut().find(|g| g.branch == pr.branch) {
                Some(g) => g.ids.push(id.clone()),
                None => groups.push(Group { branch: pr.branch.clone(), ids: vec![id.clone()], existing: true }),
            },
            None => fresh.push(id.clone()),
        }
    }
    for (n, chunk) in fresh.chunks(max).enumerate() {
        let branch = if chunk.len() == 1 { format!("{BRANCH_PREFIX}{}", chunk[0]) } else { format!("{COMBINED_PREFIX}{stamp}-{}", n + 1) };
        groups.push(Group { branch, ids: chunk.to_vec(), existing: false });
    }
    groups
}

/// Archives Development recipes whose pull requests are merged and whose
/// contents still exactly match the recipe on the remote default branch.
/// Changed local work is never moved. The caller should run this off the UI
/// thread because it fetches the remote default branch.
pub fn retire_merged_recipes(ws: &Workspace, repo: &Repo, prs: &[PullRequestStatus]) -> Vec<Retirement> {
    let merged: Vec<&PullRequestStatus> = prs
        .iter()
        .filter(|pr| pr.state.eq_ignore_ascii_case("MERGED") && ws.development_recipe_path(&pr.recipe_id).is_file())
        .collect();
    if merged.is_empty() {
        return Vec::new();
    }
    let git_exe = match crate::git::git_exe() {
        Ok(git) => git,
        Err(error) => {
            return merged
                .into_iter()
                .map(|pr| Retirement {
                    recipe_id: pr.recipe_id.clone(),
                    retired: false,
                    archive: None,
                    message: format!("Could not verify the merged recipe because git is unavailable: {error:#}"),
                })
                .collect();
        }
    };
    let fetch = run_output(&git(&git_exe, &ws.root, &["fetch", "--prune", "origin", &repo.base]));
    if let Err(error) = fetch.as_ref() {
        return merged
            .into_iter()
            .map(|pr| Retirement {
                recipe_id: pr.recipe_id.clone(),
                retired: false,
                archive: None,
                message: format!("Could not refresh origin/{} before cleanup: {error:#}", repo.base),
            })
            .collect();
    }
    if let Ok(output) = fetch
        && !output.success
    {
        let detail = last_line(&output);
        return merged
            .into_iter()
            .map(|pr| Retirement {
                recipe_id: pr.recipe_id.clone(),
                retired: false,
                archive: None,
                message: format!("Could not refresh origin/{} before cleanup: {detail}", repo.base),
            })
            .collect();
    }

    merged.into_iter().map(|pr| retire_merged_recipe(ws, repo, &git_exe, pr)).collect()
}

fn retire_merged_recipe(ws: &Workspace, repo: &Repo, git_exe: &Path, pr: &PullRequestStatus) -> Retirement {
    let unchanged = || Retirement { recipe_id: pr.recipe_id.clone(), retired: false, archive: None, message: String::new() };
    let plan = match plan(ws, &pr.recipe_id) {
        Ok(plan) => plan,
        Err(error) => return Retirement { message: format!("Could not prepare merged-recipe cleanup: {error:#}"), ..unchanged() },
    };
    let remote_path = plan.dest.iter().map(|part| part.to_string_lossy()).collect::<Vec<_>>().join("/");
    let object = format!("origin/{}:{remote_path}", repo.base);
    let remote = match run_output(&git(git_exe, &ws.root, &["rev-parse", &object])) {
        Ok(output) => output,
        Err(error) => return Retirement { message: format!("Could not identify the merged recipe at {object}: {error:#}"), ..unchanged() },
    };
    if !remote.success {
        return Retirement { message: format!("The pull request is merged, but {object} is not available: {}", last_line(&remote)), ..unchanged() };
    }
    // `--filters --path` applies the same clean filters used during promotion,
    // so secrets scrubbed by git do not look like later local edits.
    let source = plan.source.to_string_lossy().into_owned();
    let path_arg = format!("--path={remote_path}");
    let local = match run_output(&git(git_exe, &ws.root, &["hash-object", "--filters", &path_arg, &source])) {
        Ok(output) => output,
        Err(error) => return Retirement { message: format!("Could not hash the Development recipe through git filters: {error:#}"), ..unchanged() },
    };
    if !local.success {
        return Retirement { message: format!("Could not hash the Development recipe through git filters: {}", last_line(&local)), ..unchanged() };
    }
    if local.stdout.trim() != remote.stdout.trim() {
        return Retirement {
            message: "Merged on GitHub, but the Development recipe has changed since it was published; keeping the newer local work.".into(),
            ..unchanged()
        };
    }
    let archive_dir = ws.layout.artifacts.join("PublishedRecipes");
    if let Err(error) = std::fs::create_dir_all(&archive_dir) {
        return Retirement { message: format!("Could not create {}: {error}", archive_dir.display()), ..unchanged() };
    }
    let archive = archive_dir.join(format!("{}-pr-{}.yaml", pr.recipe_id, pr.number));
    if archive.exists() {
        return Retirement { message: format!("Archive {} already exists; keeping the Development copy.", archive.display()), ..unchanged() };
    }
    match std::fs::rename(&plan.source, &archive) {
        Ok(()) => Retirement {
            recipe_id: pr.recipe_id.clone(),
            retired: true,
            archive: Some(archive.clone()),
            message: format!("Merged PR #{}; moved the matching Development copy to {}.", pr.number, archive.display()),
        },
        Err(error) => Retirement { message: format!("Could not archive {}: {error}", plan.source.display()), ..unchanged() },
    }
}

fn text_field(value: &Value, field: &str) -> String {
    value.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn nested_text(value: &Value, object: &str, field: &str) -> String {
    value.get(object).map(|v| text_field(v, field)).unwrap_or_default()
}

fn array_field<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value.get(field).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()
}

/// Commits `plan` on its branch from `origin/<base>`, pushes it, copies the
/// icon to the shared folder, and opens a pull request with `gh` when it is
/// given (falling back to GitHub's compare page). `evidence` goes in the
/// commit and pull request bodies.
pub fn open_pull_request(ws: &Workspace, plan: &Plan, repo: &Repo, gh: Option<&Path>, evidence: &Evidence, log: &mut dyn FnMut(&str)) -> Result<Outcome> {
    open_group_pull_request(ws, &[(plan.clone(), evidence.clone())], &plan.branch, repo, gh, log)
}

/// Like [`open_pull_request`] for several recipes on one `branch`: each recipe
/// and its evidence file are committed together, and the description lists
/// them (with a marker the Studio reads back to track each recipe).
pub fn open_group_pull_request(ws: &Workspace, plans: &[(Plan, Evidence)], branch: &str, repo: &Repo, gh: Option<&Path>, log: &mut dyn FnMut(&str)) -> Result<Outcome> {
    ws.ensure_writable("open a pull request from")?;
    if plans.is_empty() {
        bail!("No recipes to publish.");
    }
    if !branch.starts_with(BRANCH_PREFIX) || !crate::git::is_valid_branch_name(branch) {
        bail!("'{branch}' is not a Studio branch.");
    }
    let git_exe = crate::git::git_exe()?;
    let root = &ws.root;
    let existing = match gh {
        Some(exe) => find_open_pull_request(root, repo, exe, branch)?,
        None => None,
    };
    run(&git_exe, root, &["fetch", "origin", &repo.base], log)?;
    if existing.is_some() {
        run(&git_exe, root, &["fetch", "origin", &format!("refs/heads/{branch}:refs/remotes/origin/{branch}")], log)?;
    }

    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let slug: String = branch.trim_start_matches(BRANCH_PREFIX).chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    let dir = std::env::temp_dir().join(format!("ystudio-pr-{slug}-{stamp}"));
    let dir_s = dir.to_string_lossy().into_owned();
    let base_ref = existing.as_ref().map_or_else(|| format!("origin/{}", repo.base), |_| format!("origin/{branch}"));
    run(&git_exe, root, &["worktree", "add", "--force", "-B", branch, &dir_s, &base_ref], log)?;
    let _cleanup = Worktree { git: &git_exe, repo: root, dir: dir.clone(), branch: branch.to_string() };

    let mut added = Vec::new();
    let mut lines = Vec::new();
    for (plan, evidence) in plans {
        let dest = dir.join(&plan.dest);
        added.push(!dest.exists());
        std::fs::create_dir_all(dest.parent().unwrap_or(&dir)).with_context(|| format!("creating the folder for {}", plan.dest.display()))?;
        std::fs::copy(&plan.source, &dest).with_context(|| format!("copying {} into the branch", plan.source.display()))?;
        let dest_s = plan.dest.to_string_lossy().replace('\\', "/");
        run(&git_exe, &dir, &["add", "--", &dest_s], log)?;
        // Hash what git commits: a clean filter (Yardstick's cleanSecrets) may
        // rewrite the recipe, and the check hashes the committed version.
        let blob = run_bytes(&git(&git_exe, &dir, &["cat-file", "blob", &format!(":{dest_s}")]))?;
        let mut evidence = evidence.clone();
        evidence.recipe_path = dest_s.clone();
        evidence.recipe_sha256 = hash_bytes(&blob);
        if evidence.tested_sha256.as_ref().is_some_and(|t| *t != evidence.recipe_sha256) {
            log(&format!("{}: git's clean filter rewrote the recipe as it was committed; the evidence records the committed version.", plan.id));
        }
        let evidence_rel = format!("{EVIDENCE_DIR}/{}.json", plan.id);
        let evidence_path = dir.join(&evidence_rel);
        std::fs::create_dir_all(evidence_path.parent().unwrap_or(&dir)).with_context(|| format!("creating {EVIDENCE_DIR}"))?;
        std::fs::write(&evidence_path, serde_json::to_string_pretty(&evidence)? + "\n").with_context(|| format!("writing {evidence_rel}"))?;
        run(&git_exe, &dir, &["add", "--", &evidence_rel], log)?;
        if !evidence.validated {
            log(&format!("{}: not validated, so Yardstick's Recipe evidence check will fail until it is tested or the pull request is labelled evidence-waived.", plan.id));
        }
        lines.push((plan.id.clone(), evidence_rel, evidence.summary()));
    }
    if run_output(&git(&git_exe, &dir, &["diff", "--cached", "--quiet"]))?.success {
        let target = if existing.is_some() { branch } else { repo.base.as_str() };
        let what = if plans.len() == 1 { plans[0].0.dest.display().to_string() } else { format!("All {} recipes", plans.len()) };
        bail!("{what} and their evidence are already identical on {target}; there is nothing to promote.");
    }

    let ids: Vec<&str> = plans.iter().map(|(p, _)| p.id.as_str()).collect();
    let (title, body) = if plans.len() == 1 {
        let (id, evidence_rel, summary) = &lines[0];
        (
            format!("{} recipe: {id}", if added[0] { "Add" } else { "Update" }),
            format!("{summary}\n\nEvidence: {evidence_rel}. Opened by Yardstick Development Studio from Recipes/Development/{id}.yaml."),
        )
    } else {
        let verb = match (added.iter().all(|a| *a), added.iter().any(|a| *a)) {
            (true, _) => "Add",
            (false, false) => "Update",
            _ => "Add and update",
        };
        let mut list = ids.iter().take(6).copied().collect::<Vec<_>>().join(", ");
        if ids.len() > 6 {
            list += &format!(" and {} more", ids.len() - 6);
        }
        let mut body = format!("{} recipes, opened by Yardstick Development Studio from Recipes/Development.\n\n", ids.len());
        for (id, evidence_rel, summary) in &lines {
            body += &format!("- **{id}**: {summary} Evidence: {evidence_rel}.\n");
        }
        body += &format!("\n{RECIPES_MARKER} {} -->", ids.join(", "));
        (format!("{verb} {} recipes: {list}", ids.len()), body)
    };
    run(&git_exe, &dir, &["commit", "-m", &title, "-m", &body], log)?;
    let destination = format!("HEAD:refs/heads/{branch}");
    if existing.is_some() {
        run(&git_exe, &dir, &["push", "origin", &destination], log)?;
    } else {
        run(&git_exe, &dir, &["push", "--force-with-lease", "origin", &destination], log)?;
    }

    for (plan, _) in plans {
        copy_icon(plan, log);
    }
    let plan_branch = branch;

    let compare = format!("{}/compare/{}...{}?expand=1", repo.web_url, repo.base, plan_branch);
    let Some(gh) = gh else {
        log("gh is not installed; the pull request is created from GitHub's compare page.");
        return Ok(Outcome::Compare { url: compare });
    };
    let slug = repo_slug(repo);
    if let Some(existing) = existing {
        log(&format!("A pull request for {} is already open; appending the new commit to PR #{}.", ids.join(", "), existing.number));
        let number = existing.number.to_string();
        match gh_output(root, gh, &["pr", "edit", &number, "--repo", slug, "--title", &title, "--body", &body]) {
            Ok(out) if !out.success => log(&format!("The commit was appended, but the pull request description could not be refreshed ({}).", last_line(&out))),
            Err(e) => log(&format!("The commit was appended, but the pull request description could not be refreshed ({e:#}).")),
            _ => {}
        }
        return Ok(Outcome::PullRequest { url: existing.url, existing: true });
    }
    log(&format!("> gh pr create --repo {slug} --base {} --head {plan_branch}", repo.base));
    let out = gh_output(root, gh, &["pr", "create", "--repo", slug, "--base", &repo.base, "--head", plan_branch, "--title", &title, "--body", &body])?;
    if out.success
        && let Some(url) = pull_url(&out.stdout)
    {
        return Ok(Outcome::PullRequest { url, existing: false });
    }
    // A pull request already open for the branch now shows the new commit.
    if out.stderr.contains("already exists")
        && let Some(url) = pull_url(&out.stderr)
    {
        log("A pull request for this branch was already open; it now has the new commit.");
        return Ok(Outcome::PullRequest { url, existing: true });
    }
    log(&format!("gh could not open the pull request ({}); use GitHub's compare page instead.", last_line(&out)));
    Ok(Outcome::Compare { url: compare })
}

fn copy_icon(plan: &Plan, log: &mut dyn FnMut(&str)) {
    let Some((src, dest)) = &plan.icon else {
        if let Some(note) = &plan.icon_note {
            log(note);
        }
        return;
    };
    let same = std::fs::read(src).ok().zip(std::fs::read(dest).ok()).is_some_and(|(a, b)| a == b);
    if same {
        log(&format!("The shared icon folder already has {}.", dest.display()));
        return;
    }
    match std::fs::copy(src, dest) {
        Ok(_) => log(&format!("Copied the icon to {}.", dest.display())),
        Err(e) => log(&format!("Could not copy the icon to {}: {e}. Copy it there by hand.", dest.display())),
    }
}

/// The first `https://…/pull/N` in `text`.
fn pull_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != ':' && c != '.' && c != '-'))
        .find(|w| w.starts_with("https://") && w.rsplit_once("/pull/").is_some_and(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::fake_workspace;

    #[test]
    fn git_progress_logs_only_its_final_frame() {
        let stderr = "Preparing worktree\nUpdating files:   6% (24/376)\rUpdating files: 100% (376/376), done.\n\n";
        assert_eq!(stderr_lines(stderr).collect::<Vec<_>>(), ["Preparing worktree", "Updating files: 100% (376/376), done."]);
    }

    #[test]
    fn web_urls_from_remotes() {
        assert_eq!(web_url("https://github.com/jeffpearson1/Yardstick").as_deref(), Some("https://github.com/jeffpearson1/Yardstick"));
        assert_eq!(web_url("https://user@github.com/o/r.git/").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("git@github.com:o/r.git").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("ssh://git@ghe.example.com:22/o/r.git").as_deref(), Some("https://ghe.example.com/o/r"));
        assert_eq!(web_url(r"C:\repos\bare.git"), None);
        assert_eq!(web_url("https://github.com/only-owner"), None);
    }

    fn open_pr(id: &str, branch: &str, state: &str) -> PullRequestStatus {
        PullRequestStatus {
            number: 7, recipe_id: id.into(), branch: branch.into(), title: String::new(), url: String::new(), state: state.into(),
            merged_at: String::new(), draft: false, merge_state: String::new(), review_decision: String::new(), updated_at: String::new(),
            checks: Vec::new(), activity: Vec::new(),
        }
    }

    #[test]
    fn groups_new_recipes_and_keeps_ones_with_open_pull_requests_on_them() {
        let ids: Vec<String> = ["a", "b", "c", "d", "e", "f"].iter().map(|s| s.to_string()).collect();
        let open = [
            open_pr("b", "studio/b", "OPEN"),
            open_pr("c", "studio/batch-1-1", "OPEN"),
            open_pr("d", "studio/batch-1-1", "OPEN"),
            open_pr("e", "studio/e", "MERGED"),
        ];
        let groups = group(&ids, &open, 2, "20261118");
        let shape: Vec<(&str, Vec<&str>, bool)> = groups.iter().map(|g| (g.branch.as_str(), g.ids.iter().map(String::as_str).collect(), g.existing)).collect();
        assert_eq!(shape, [
            ("studio/b", vec!["b"], true),
            ("studio/batch-1-1", vec!["c", "d"], true),
            ("studio/batch-20261118-1", vec!["a", "e"], false),
            ("studio/f", vec!["f"], false),
        ]);
        // A limit of one is the old one-recipe-per-pull-request behaviour.
        assert!(group(&ids, &[], 1, "x").iter().all(|g| g.branch == format!("studio/{}", g.ids[0])));
        assert_eq!(group(&ids, &[], 10, "x").len(), 1);
    }

    #[test]
    fn combined_pull_requests_are_tracked_per_recipe() {
        let json = r#"[{"number":9,"title":"Add 2 recipes: a, b","body":"2 recipes\n\n<!-- studio-recipes: a, b -->","url":"https://x/pull/9",
            "headRefName":"studio/batch-20261118-1","state":"OPEN"},
            {"number":10,"headRefName":"studio/batch-x-1","body":"no marker","state":"OPEN"},
            {"number":11,"headRefName":"feature/other","state":"OPEN"}]"#;
        let prs = parse_pull_requests(json).unwrap();
        let rows: Vec<_> = prs.iter().map(|p| (p.recipe_id.as_str(), p.number, p.branch.as_str())).collect();
        assert_eq!(rows, [("a", 9, "studio/batch-20261118-1"), ("b", 9, "studio/batch-20261118-1")]);
    }

    #[test]
    fn finds_pull_request_urls() {
        assert_eq!(pull_url("https://github.com/o/r/pull/12\n").as_deref(), Some("https://github.com/o/r/pull/12"));
        let err = "a pull request for branch \"studio/x\" into branch \"main\" already exists:\nhttps://github.com/o/r/pull/7";
        assert_eq!(pull_url(err).as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(pull_url("https://github.com/o/r/compare/main...x"), None);
    }

    #[test]
    fn parses_studio_pull_requests_with_checks_and_activity() {
        let json = r#"[
          {
            "number": 42,
            "title": "Update recipe: brave",
            "url": "https://github.com/o/r/pull/42",
            "headRefName": "studio/brave",
            "state": "OPEN",
            "mergedAt": null,
            "isDraft": false,
            "mergeStateStatus": "BLOCKED",
            "reviewDecision": "CHANGES_REQUESTED",
            "updatedAt": "2026-11-16T12:30:00Z",
            "statusCheckRollup": [
              {"__typename":"CheckRun","name":"test","workflowName":"CI","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://example/check"},
              {"__typename":"StatusContext","context":"policy","state":"PENDING","targetUrl":"https://example/policy"}
            ],
            "comments": [{"author":{"login":"copilot-pull-request-reviewer[bot]"},"body":"Consider an edge case.","createdAt":"2026-11-16T12:00:00Z","url":"https://example/comment"}],
            "reviews": [{"author":{"login":"octocat"},"body":"Please update the test.","state":"CHANGES_REQUESTED","submittedAt":"2026-11-16T12:10:00Z","url":"https://example/review"}]
          },
          {"number": 7, "headRefName": "feature/not-studio"}
        ]"#;
        let prs = parse_pull_requests(json).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!((prs[0].recipe_id.as_str(), prs[0].number), ("brave", 42));
        assert_eq!(prs[0].state, "OPEN");
        assert_eq!(prs[0].checks.iter().map(|c| c.state.as_str()).collect::<Vec<_>>(), ["SUCCESS", "PENDING"]);
        assert_eq!(prs[0].activity[0].author, "octocat");
        assert!(prs[0].activity.iter().any(|a| a.author.contains("copilot")));
    }

    #[test]
    fn plan_keeps_the_shipped_folder_and_reports_icons() {
        let d = fake_workspace(Some("developmentOnly: true\nSharedIcons: '\"\\\\server\\share\\Icons\"'\n"));
        std::fs::write(d.path().join(r"Recipes\Development\clion.yaml"), "id: clion\niconFile: clion.png\n").unwrap();
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert_eq!(ws.shared_icons.as_deref(), Some(Path::new(r"\\server\share\Icons")));
        let p = plan(&ws, "clion").unwrap();
        assert_eq!(p.dest, PathBuf::from(r"Recipes\Jetbrains\clion.yaml"));
        assert_eq!(p.branch, "studio/clion");
        assert!(p.icon.is_none() && p.icon_note.unwrap().contains("does not exist"));

        std::fs::write(d.path().join(r"Icons\clion.png"), [1u8]).unwrap();
        let p = plan(&ws, "clion").unwrap();
        assert_eq!(p.icon.unwrap().1, PathBuf::from(r"\\server\share\Icons\clion.png"));

        assert_eq!(plan(&ws, "brave").unwrap().dest, PathBuf::from(r"Recipes\brave.yaml"));
        assert!(plan(&ws, "7zip").unwrap_err().to_string().contains("only development recipes"));
        let prod = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(plan(&prod, "brave").unwrap_err().to_string().contains("production"));
    }

    /// The whole flow against a local bare repository standing in for GitHub.
    #[test]
    fn pushes_a_branch_from_the_default_branch_without_touching_the_checkout() {
        let Ok(git_exe) = crate::git::git_exe() else { return };
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let root = d.path();
        let origin = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let g = |cwd: &Path, args: &[&str]| {
            let out = run_output(&git(&git_exe, cwd, args)).unwrap();
            assert!(out.success, "git {args:?}: {}", out.stderr);
            out.stdout
        };
        g(origin.path(), &["init", "--bare", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), "/Recipes/Development/*\n/Icons/*\n/Local/\n").unwrap();
        // Like Yardstick's cleanSecrets: recipes are rewritten as they are committed.
        std::fs::write(root.join(".gitattributes"), "*.yaml filter=scrub\n").unwrap();
        g(root, &["init", "-b", "main"]);
        g(root, &["config", "filter.scrub.clean", "sed -e s/secret.example/placeholder.example/"]);
        g(root, &["config", "filter.scrub.smudge", "cat"]);
        g(root, &["-c", "user.name=t", "-c", "user.email=t@example.com", "add", "-A"]);
        g(root, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-m", "base"]);
        g(root, &["remote", "add", "origin", &origin.path().to_string_lossy()]);
        g(root, &["push", "-u", "origin", "main"]);
        g(root, &["remote", "set-head", "origin", "main"]);
        g(root, &["config", "user.name", "Studio Test"]);
        g(root, &["config", "user.email", "studio@example.com"]);

        let dev_recipe = root.join(r"Recipes\Development\brave.yaml");
        std::fs::write(&dev_recipe, "id: brave\niconFile: brave.png\nurl: https://secret.example/brave.msi\n").unwrap();
        std::fs::write(root.join(r"Icons\brave.png"), [7u8]).unwrap();
        std::fs::write(root.join(r"Local\preferences.yaml"), format!("developmentOnly: true\nSharedIcons: '{}'\n", share.path().display())).unwrap();
        let ws = Workspace::open(root, &[], &[]).unwrap();
        let p = plan(&ws, "brave").unwrap();
        let repo = Repo { remote_url: String::new(), web_url: "https://github.com/o/r".into(), base: "main".into() };

        // A recipe that passed its live test, as the validation ledger records it.
        let tested = crate::validation::recipe_hash(&dev_recipe).unwrap();
        let record = crate::validation::Record { sha256: tested.clone(), warnings: false, tested: "20261109-100000".into(), summary_dir: None };
        let result = RecipeResult {
            id: "brave".into(),
            version: Some("1.2.3".into()),
            phases: crate::results::VALIDATION_PHASES.iter().map(|n| crate::results::Phase { name: Some(n.to_string()), status: "Pass".into(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_762_682_400);
        let evidence = Evidence::new("brave", &Completion::Complete(record), Some(&result), Some(at));
        let mut lines = Vec::new();
        let outcome = open_pull_request(&ws, &p, &repo, None, &evidence, &mut |l| lines.push(l.to_string())).unwrap();
        assert_eq!(outcome, Outcome::Compare { url: "https://github.com/o/r/compare/main...studio/brave?expand=1".into() });

        let committed = g(origin.path(), &["show", "studio/brave:Recipes/brave.yaml"]);
        assert!(committed.contains("placeholder.example") && !committed.contains("secret.example"), "{committed}");
        let msg = g(origin.path(), &["log", "-1", "--format=%B", "studio/brave"]);
        assert!(msg.starts_with("Add recipe: brave") && msg.contains("Validated: a live install test passed on 2025-11-09T10:00:00Z"), "{msg}");
        assert!(msg.contains("Evidence: Tests/RecipeEvidence/brave.json"), "{msg}");

        // The evidence describes the committed recipe, which the clean filter changed.
        let json: serde_json::Value = serde_json::from_str(&g(origin.path(), &["show", "studio/brave:Tests/RecipeEvidence/brave.json"])).unwrap();
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["recipeId"], "brave");
        assert_eq!(json["recipePath"], "Recipes/brave.yaml");
        assert_eq!(json["validated"], true);
        assert_eq!(json["verdict"], "Validated");
        assert_eq!(json["testedAt"], "2025-11-09T10:00:00Z");
        assert_eq!(json["installer"]["version"], "1.2.3");
        assert_eq!(json["phases"].as_array().unwrap().len(), 8);
        assert_eq!(json["recipeSha256"], hash_bytes(committed.as_bytes()));
        assert_ne!(json["recipeSha256"], tested.as_str());
        assert!(lines.iter().any(|l| l.contains("clean filter")), "{lines:#?}");
        assert_eq!(std::fs::read(share.path().join("brave.png")).unwrap(), [7u8]);
        // The user's checkout is untouched and the temporary branch is gone.
        assert_eq!(g(root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
        assert!(g(root, &["branch", "--list", "studio/brave"]).trim().is_empty());
        assert!(g(root, &["worktree", "list"]).lines().count() == 1);

        // Promoting again rebuilds the branch from main and updates it.
        std::fs::write(root.join(r"Recipes\Development\brave.yaml"), "id: brave\niconFile: brave.png\nversion: 2\nurl: https://secret.example/brave.msi\n").unwrap();
        let changed = Evidence::new("brave", &Completion::NotValidated, None, None);
        let mut lines = Vec::new();
        open_pull_request(&ws, &p, &repo, None, &changed, &mut |l| lines.push(l.to_string())).unwrap();
        assert!(g(origin.path(), &["show", "studio/brave:Recipes/brave.yaml"]).contains("version: 2"));
        let json: serde_json::Value = serde_json::from_str(&g(origin.path(), &["show", "studio/brave:Tests/RecipeEvidence/brave.json"])).unwrap();
        assert_eq!((json["validated"].as_bool(), json["verdict"].as_str()), (Some(false), Some("NotValidated")));
        assert_eq!(json["reason"], "no install test of this recipe has been recorded");
        assert!(lines.iter().any(|l| l.contains("evidence-waived")), "{lines:#?}");
        assert_eq!(g(origin.path(), &["rev-list", "--count", "main..studio/brave"]).trim(), "1");

        // Several recipes in one pull request: one commit with each recipe and its evidence.
        std::fs::write(root.join(r"Recipes\Development\vlc.yaml"), "id: vlc\n").unwrap();
        std::fs::write(root.join(r"Recipes\Development\gimp.yaml"), "id: gimp\n").unwrap();
        let plans: Vec<(Plan, Evidence)> = ["vlc", "gimp"].iter()
            .map(|id| (plan(&ws, id).unwrap(), Evidence::new(id, &Completion::NotValidated, None, None)))
            .collect();
        let outcome = open_group_pull_request(&ws, &plans, "studio/batch-1-1", &repo, None, &mut |_| {}).unwrap();
        assert_eq!(outcome.url(), "https://github.com/o/r/compare/main...studio/batch-1-1?expand=1");
        for f in ["Recipes/vlc.yaml", "Recipes/gimp.yaml", "Tests/RecipeEvidence/vlc.json", "Tests/RecipeEvidence/gimp.json"] {
            g(origin.path(), &["show", &format!("studio/batch-1-1:{f}")]);
        }
        let msg = g(origin.path(), &["log", "-1", "--format=%B", "studio/batch-1-1"]);
        assert!(msg.starts_with("Add 2 recipes: vlc, gimp") && msg.contains("<!-- studio-recipes: vlc, gimp -->"), "{msg}");
        assert_eq!(marked_recipes(&msg), ["vlc", "gimp"]);
        assert!(open_group_pull_request(&ws, &plans, "feature/x", &repo, None, &mut |_| {}).is_err(), "only Studio branches");

        // Once main has it, there is nothing to promote.
        g(origin.path(), &["update-ref", "refs/heads/main", "refs/heads/studio/brave"]);
        let err = open_pull_request(&ws, &p, &repo, None, &changed, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("already identical"), "{err:#}");

        let merged = PullRequestStatus {
            number: 42,
            recipe_id: "brave".into(),
            branch: "studio/brave".into(),
            title: "Add brave".into(),
            url: "https://github.com/o/r/pull/42".into(),
            state: "MERGED".into(),
            merged_at: "2026-11-16T12:00:00Z".into(),
            draft: false,
            merge_state: String::new(),
            review_decision: String::new(),
            updated_at: String::new(),
            checks: Vec::new(),
            activity: Vec::new(),
        };
        let retired = retire_merged_recipes(&ws, &repo, std::slice::from_ref(&merged));
        assert_eq!(retired.len(), 1);
        assert!(retired[0].retired, "{:?}", retired[0]);
        assert!(!dev_recipe.exists());
        assert_eq!(
            std::fs::read_to_string(retired[0].archive.as_ref().unwrap()).unwrap(),
            "id: brave\niconFile: brave.png\nversion: 2\nurl: https://secret.example/brave.msi\n"
        );

        // Work added after publication is never retired by an older merge.
        std::fs::write(&dev_recipe, "id: brave\niconFile: brave.png\nversion: 3\n").unwrap();
        let kept = retire_merged_recipes(&ws, &repo, &[PullRequestStatus { number: 43, ..merged }]);
        assert_eq!(kept.len(), 1);
        assert!(!kept[0].retired && kept[0].message.contains("changed since it was published"));
        assert!(dev_recipe.exists());
    }
}
