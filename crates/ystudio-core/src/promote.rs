//! Promotion by pull request. A development recipe is committed to a branch
//! cut from the upstream default branch, in a temporary worktree so the
//! user's checkout and `Recipes\Development` are untouched, then pushed and
//! offered as a pull request. Production pulls merged work itself; the Studio
//! never writes to a production folder.
//!
//! Icons are not kept in git: promotion copies a recipe's icon to the shared
//! icon folder (`SharedIcons` in preferences), which Yardstick syncs from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::process::{Output, ProcessSpec, run_output};
use crate::workspace::{RecipeLocation, Workspace};

/// Branches the Studio pushes live under this prefix.
pub const BRANCH_PREFIX: &str = "studio/";

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

fn git(git: &Path, cwd: &Path, args: &[&str]) -> ProcessSpec {
    // Never block on a credential prompt nobody can see.
    ProcessSpec::new("git", git, cwd).args(args.iter().copied()).env("GIT_TERMINAL_PROMPT", "0")
}

/// Runs git, logging the command; fails with git's own message.
fn run(git_exe: &Path, cwd: &Path, args: &[&str], log: &mut dyn FnMut(&str)) -> Result<String> {
    log(&format!("> git {}", args.join(" ")));
    let out = run_output(&git(git_exe, cwd, args))?;
    for line in out.stderr.lines().filter(|l| !l.trim().is_empty()) {
        log(line);
    }
    if !out.success {
        bail!("git {} failed: {}", args.first().copied().unwrap_or_default(), last_line(&out));
    }
    Ok(out.stdout)
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

/// Commits `plan` on its branch from `origin/<base>`, pushes it, copies the
/// icon to the shared folder, and opens a pull request with `gh` when it is
/// given (falling back to GitHub's compare page). `evidence` goes in the
/// commit and pull request bodies.
pub fn open_pull_request(ws: &Workspace, plan: &Plan, repo: &Repo, gh: Option<&Path>, evidence: &str, log: &mut dyn FnMut(&str)) -> Result<Outcome> {
    ws.ensure_writable("open a pull request from")?;
    let git_exe = crate::git::git_exe()?;
    let root = &ws.root;
    run(&git_exe, root, &["fetch", "origin", &repo.base], log)?;

    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("ystudio-pr-{}-{stamp}", plan.id));
    let dir_s = dir.to_string_lossy().into_owned();
    let base_ref = format!("origin/{}", repo.base);
    run(&git_exe, root, &["worktree", "add", "--force", "-B", &plan.branch, &dir_s, &base_ref], log)?;
    let _cleanup = Worktree { git: &git_exe, repo: root, dir: dir.clone(), branch: plan.branch.clone() };

    let dest = dir.join(&plan.dest);
    let new = !dest.exists();
    std::fs::create_dir_all(dest.parent().unwrap_or(&dir)).with_context(|| format!("creating the folder for {}", plan.dest.display()))?;
    std::fs::copy(&plan.source, &dest).with_context(|| format!("copying {} into the branch", plan.source.display()))?;
    let dest_s = plan.dest.to_string_lossy().replace('\\', "/");
    run(&git_exe, &dir, &["add", "--", &dest_s], log)?;
    if run_output(&git(&git_exe, &dir, &["diff", "--cached", "--quiet"]))?.success {
        bail!("{} is already identical on {}; there is nothing to promote.", plan.dest.display(), repo.base);
    }
    let title = format!("{} recipe: {}", if new { "Add" } else { "Update" }, plan.id);
    let body = format!("{evidence}\n\nOpened by Yardstick Development Studio from Recipes/Development/{}.yaml.", plan.id);
    run(&git_exe, &dir, &["commit", "-m", &title, "-m", &body], log)?;
    run(&git_exe, &dir, &["push", "--force-with-lease", "origin", &format!("HEAD:refs/heads/{}", plan.branch)], log)?;

    copy_icon(plan, log);

    let compare = format!("{}/compare/{}...{}?expand=1", repo.web_url, repo.base, plan.branch);
    let Some(gh) = gh else {
        log("gh is not installed; the pull request is created from GitHub's compare page.");
        return Ok(Outcome::Compare { url: compare });
    };
    let slug = repo.web_url.trim_start_matches("https://");
    let slug = slug.strip_prefix("github.com/").unwrap_or(slug);
    log(&format!("> gh pr create --repo {slug} --base {} --head {}", repo.base, plan.branch));
    let spec = ProcessSpec::new("gh", gh, root)
        .args(["pr", "create", "--repo", slug, "--base", &repo.base, "--head", &plan.branch, "--title", &title, "--body", &body])
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    let out = run_output(&spec)?;
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
    fn web_urls_from_remotes() {
        assert_eq!(web_url("https://github.com/jeffpearson1/Yardstick").as_deref(), Some("https://github.com/jeffpearson1/Yardstick"));
        assert_eq!(web_url("https://user@github.com/o/r.git/").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("git@github.com:o/r.git").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("ssh://git@ghe.example.com:22/o/r.git").as_deref(), Some("https://ghe.example.com/o/r"));
        assert_eq!(web_url(r"C:\repos\bare.git"), None);
        assert_eq!(web_url("https://github.com/only-owner"), None);
    }

    #[test]
    fn finds_pull_request_urls() {
        assert_eq!(pull_url("https://github.com/o/r/pull/12\n").as_deref(), Some("https://github.com/o/r/pull/12"));
        let err = "a pull request for branch \"studio/x\" into branch \"main\" already exists:\nhttps://github.com/o/r/pull/7";
        assert_eq!(pull_url(err).as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(pull_url("https://github.com/o/r/compare/main...x"), None);
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
        g(root, &["init", "-b", "main"]);
        g(root, &["-c", "user.name=t", "-c", "user.email=t@example.com", "add", "-A"]);
        g(root, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-m", "base"]);
        g(root, &["remote", "add", "origin", &origin.path().to_string_lossy()]);
        g(root, &["push", "-u", "origin", "main"]);
        g(root, &["remote", "set-head", "origin", "main"]);
        g(root, &["config", "user.name", "Studio Test"]);
        g(root, &["config", "user.email", "studio@example.com"]);

        std::fs::write(root.join(r"Recipes\Development\brave.yaml"), "id: brave\niconFile: brave.png\n").unwrap();
        std::fs::write(root.join(r"Icons\brave.png"), [7u8]).unwrap();
        std::fs::write(root.join(r"Local\preferences.yaml"), format!("developmentOnly: true\nSharedIcons: '{}'\n", share.path().display())).unwrap();
        let ws = Workspace::open(root, &[], &[]).unwrap();
        let p = plan(&ws, "brave").unwrap();
        let repo = Repo { remote_url: String::new(), web_url: "https://github.com/o/r".into(), base: "main".into() };
        let mut lines = Vec::new();
        let outcome = open_pull_request(&ws, &p, &repo, None, "Validated on 2026-11-09.", &mut |l| lines.push(l.to_string())).unwrap();
        assert_eq!(outcome, Outcome::Compare { url: "https://github.com/o/r/compare/main...studio/brave?expand=1".into() });

        assert_eq!(g(origin.path(), &["show", "studio/brave:Recipes/brave.yaml"]).replace("\r\n", "\n"), "id: brave\niconFile: brave.png\n");
        let msg = g(origin.path(), &["log", "-1", "--format=%B", "studio/brave"]);
        assert!(msg.starts_with("Add recipe: brave") && msg.contains("Validated on 2026-11-09."), "{msg}");
        assert_eq!(std::fs::read(share.path().join("brave.png")).unwrap(), [7u8]);
        // The user's checkout is untouched and the temporary branch is gone.
        assert_eq!(g(root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
        assert!(g(root, &["branch", "--list", "studio/brave"]).trim().is_empty());
        assert!(g(root, &["worktree", "list"]).lines().count() == 1);

        // Promoting again rebuilds the branch from main and updates it.
        std::fs::write(root.join(r"Recipes\Development\brave.yaml"), "id: brave\niconFile: brave.png\nversion: 2\n").unwrap();
        open_pull_request(&ws, &p, &repo, None, "", &mut |_| {}).unwrap();
        assert!(g(origin.path(), &["show", "studio/brave:Recipes/brave.yaml"]).contains("version: 2"));
        assert_eq!(g(origin.path(), &["rev-list", "--count", "main..studio/brave"]).trim(), "1");

        // Once main has it, there is nothing to promote.
        g(origin.path(), &["update-ref", "refs/heads/main", "refs/heads/studio/brave"]);
        let err = open_pull_request(&ws, &p, &repo, None, "", &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("already identical"), "{err:#}");
    }
}
