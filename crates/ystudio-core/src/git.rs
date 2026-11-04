//! Git for development workspaces: locating git, fetch/pull, and how far the
//! clone is behind its upstream.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::process::{ProcessSpec, refresh_path, run_capture, which};
use crate::workspace::Workspace;

/// Finds git on PATH, re-reading PATH from the registry once if it is missing
/// (git may have been installed since the Studio started).
pub fn git_exe() -> Result<PathBuf> {
    if let Some(g) = which("git") {
        return Ok(g);
    }
    let _ = refresh_path();
    which("git").ok_or_else(|| anyhow!("git was not found on PATH. Install Git (e.g. from Company Portal), then try again."))
}

pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

fn spec(label: &str, git: PathBuf, cwd: &Path, args: &[&str]) -> ProcessSpec {
    ProcessSpec::new(label, git, cwd)
        .args(args.iter().copied())
        // Never block on a credential prompt nobody can see.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stderr_is_progress()
}

/// `git clone <url> <dest>`, creating the parent folders of `dest` if needed.
pub fn clone(url: &str, dest: &Path) -> Result<ProcessSpec> {
    if url.trim().is_empty() {
        bail!("Enter a repository URL.");
    }
    if dest.as_os_str().is_empty() || !dest.is_absolute() {
        bail!("Enter a full destination path, e.g. C:\\Intune\\YardstickDev.");
    }
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
        bail!("'{}' already exists and is not empty.", dest.display());
    }
    let parent = dest.parent().ok_or_else(|| anyhow!("'{}' has no parent folder.", dest.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let git = git_exe()?;
    Ok(spec("git clone", git, parent, &["clone", "--progress", url, &dest.to_string_lossy()]))
}

fn ensure_repo(ws: &Workspace, action: &str) -> Result<PathBuf> {
    ws.ensure_writable(action)?;
    if !is_repo(&ws.root) {
        bail!("'{}' is not a git clone.", ws.root.display());
    }
    git_exe()
}

pub fn fetch(ws: &Workspace) -> Result<ProcessSpec> {
    let git = ensure_repo(ws, "git fetch")?;
    Ok(spec("git fetch", git, &ws.root, &["fetch", "--prune", "--progress"]))
}

/// Fast-forward only, so local commits are never merged or rebased behind the user's back.
/// `Recipes\Development` is git-ignored, so work in progress there is untouched.
pub fn pull(ws: &Workspace) -> Result<ProcessSpec> {
    let git = ensure_repo(ws, "git pull")?;
    Ok(spec("git pull", git, &ws.root, &["pull", "--ff-only", "--progress"]))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub branch: String,
    /// e.g. `origin/main`; `None` when the branch tracks nothing.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// Local branch and ahead/behind counts against the last fetched upstream. Does not touch the network.
pub fn status(root: &Path) -> Result<Status> {
    let git = git_exe()?;
    let run = |args: &[&str]| run_capture(&spec("git", git.clone(), root, args)).map(|s| s.trim().to_string());
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let Ok(upstream) = run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]) else {
        return Ok(Status { branch, upstream: None, ahead: 0, behind: 0 });
    };
    let (ahead, behind) = parse_left_right(&run(&["rev-list", "--left-right", "--count", "HEAD...@{u}"])?)?;
    Ok(Status { branch, upstream: Some(upstream), ahead, behind })
}

/// Parses `git rev-list --left-right --count HEAD...@{u}`: "<ahead>\t<behind>".
fn parse_left_right(s: &str) -> Result<(u32, u32)> {
    let mut it = s.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b))) => Ok((a, b)),
        _ => bail!("unexpected rev-list output '{s}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ahead_behind() {
        assert_eq!(parse_left_right("2\t5\n").unwrap(), (2, 5));
        assert!(parse_left_right("x").is_err());
    }

    #[test]
    fn clone_creates_missing_parents() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join(r"a\b\YardstickDev");
        if which("git").is_none() {
            return;
        }
        let s = clone("https://example.invalid/repo", &dest).unwrap();
        assert!(dest.parent().unwrap().is_dir());
        assert!(s.stderr_is_progress);
        assert!(s.args.contains(&dest.to_string_lossy().into_owned()));
    }

    #[test]
    fn clone_refuses_non_empty_destination() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), "").unwrap();
        assert!(clone("https://example.invalid/repo", d.path()).is_err());
        assert!(clone("https://example.invalid/repo", Path::new("relative")).is_err());
    }

    #[test]
    fn fetch_refused_in_production_and_non_repo() {
        let d = crate::workspace::fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        assert!(fetch(&ws).unwrap_err().to_string().contains("not a git clone"));
        let prod = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(fetch(&prod).unwrap_err().to_string().contains("production"));
    }
}
