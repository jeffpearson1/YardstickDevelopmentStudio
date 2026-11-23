//! Git for development workspaces: locating git, fetch/pull, and how far the
//! clone is behind its upstream.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::process::{ProcessSpec, find_program, run_capture};
use crate::workspace::Workspace;

/// Finds git on PATH, re-reading PATH from the registry if it is missing
/// (git may have been installed since the Studio started).
pub fn git_exe() -> Result<PathBuf> {
    find_program("git", &[]).ok_or_else(|| anyhow!("git was not found on PATH. Install Git (e.g. from Company Portal), then try again."))
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

/// A branch the workspace can switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// `main`, or `origin/feature` for a remote branch with no local copy yet.
    pub name: String,
    pub remote: bool,
    pub current: bool,
    /// The upstream of a local branch, e.g. `origin/main`.
    pub upstream: Option<String>,
}

/// Local branches (current first), then remote branches that have no local
/// branch of the same name. Reads refs only; does not touch the network.
pub fn branches(root: &Path) -> Result<Vec<Branch>> {
    let git = git_exe()?;
    let out = run_capture(&spec("git", git, root, &["for-each-ref", "--format=%(refname)%09%(HEAD)%09%(upstream:short)", "refs/heads", "refs/remotes"]))?;
    Ok(parse_branches(&out))
}

fn parse_branches(out: &str) -> Vec<Branch> {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in out.lines() {
        let mut f = line.split('\t');
        let (Some(refname), head, upstream) = (f.next(), f.next().unwrap_or(""), f.next().unwrap_or("")) else { continue };
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            let upstream = Some(upstream.trim().to_string()).filter(|u| !u.is_empty());
            local.push(Branch { name: name.to_string(), remote: false, current: head.trim() == "*", upstream });
        } else if let Some(name) = refname.strip_prefix("refs/remotes/")
            && !name.ends_with("/HEAD")
        {
            remote.push(Branch { name: name.to_string(), remote: true, current: false, upstream: None });
        }
    }
    remote.retain(|r| {
        let short = r.name.split_once('/').map_or(r.name.as_str(), |(_, b)| b);
        !local.iter().any(|l| l.name == short)
    });
    local.sort_by_key(|b| (!b.current, b.name.to_lowercase()));
    remote.sort_by_key(|b| b.name.to_lowercase());
    local.extend(remote);
    local
}

/// Whether `name` is a branch name git accepts (the common rules of
/// `git check-ref-format --branch`), so it never reaches git as an option.
pub fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name != "@"
        && !name.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// Tracked files with uncommitted changes. `Recipes\Development` is git-ignored,
/// so work in progress there does not count.
pub fn changed_files(root: &Path) -> Result<usize> {
    let git = git_exe()?;
    let out = run_capture(&spec("git", git, root, &["status", "--porcelain", "--untracked-files=no"]))?;
    Ok(out.lines().filter(|l| !l.trim().is_empty()).count())
}

/// Switches the workspace to `branch`. A remote branch (`origin/x`) becomes a
/// local branch tracking it. Never forces: git refuses when uncommitted changes
/// would be overwritten.
pub fn switch(ws: &Workspace, branch: &Branch) -> Result<ProcessSpec> {
    let git = ensure_repo(ws, "switch branches in")?;
    if !is_valid_branch_name(&branch.name) {
        bail!("'{}' is not a valid branch name.", branch.name);
    }
    let label = format!("git switch {}", branch.name);
    Ok(if branch.remote {
        spec(&label, git, &ws.root, &["switch", "--track", &branch.name])
    } else {
        spec(&label, git, &ws.root, &["switch", &branch.name])
    })
}

/// Creates `name` from the current commit and switches to it.
pub fn create_branch(ws: &Workspace, name: &str) -> Result<ProcessSpec> {
    let git = ensure_repo(ws, "create branches in")?;
    let name = name.trim();
    if !is_valid_branch_name(name) {
        bail!("'{name}' is not a valid branch name: no spaces, and none of ~ ^ : ? * [ \\ or '..'.");
    }
    Ok(spec(&format!("git switch -c {name}"), git, &ws.root, &["switch", "-c", name]))
}

/// Pushes the current branch; a branch with no upstream is published to
/// `origin` and starts tracking it.
pub fn push(ws: &Workspace, status: &Status) -> Result<ProcessSpec> {
    let git = ensure_repo(ws, "git push from")?;
    if status.upstream.is_some() {
        return Ok(spec("git push", git, &ws.root, &["push", "--progress"]));
    }
    if !is_valid_branch_name(&status.branch) || status.branch == "HEAD" {
        bail!("Switch to a branch before pushing (HEAD is detached).");
    }
    Ok(spec(&format!("git push -u origin {}", status.branch), git, &ws.root, &["push", "--progress", "-u", "origin", &status.branch]))
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
///
/// One `git status` call: starting git can take half a second where
/// antivirus scans each launch, so this does not run one command per field.
pub fn status(root: &Path) -> Result<Status> {
    let git = git_exe()?;
    // Optional locks off: a status running beside a fetch or pull must not
    // take the index lock they need.
    let args = ["--no-optional-locks", "status", "--porcelain=v2", "--branch", "--untracked-files=no", "--ignore-submodules=all"];
    parse_status(&run_capture(&spec("git", git, root, &args))?)
}

/// Reads the `# branch.*` headers of `git status --porcelain=v2 --branch`.
fn parse_status(out: &str) -> Result<Status> {
    let mut status = Status { branch: String::new(), upstream: None, ahead: 0, behind: 0 };
    for line in out.lines() {
        let Some(header) = line.strip_prefix("# branch.") else { continue };
        let (key, value) = header.split_once(' ').unwrap_or((header, ""));
        match key {
            // `git rev-parse --abbrev-ref HEAD` spells a detached HEAD "HEAD".
            "head" => status.branch = if value == "(detached)" { "HEAD".into() } else { value.to_string() },
            "upstream" => status.upstream = Some(value.to_string()),
            "ab" => {
                let mut it = value.split_whitespace();
                let parse = |v: Option<&str>, sign: char| v.and_then(|v| v.strip_prefix(sign)).and_then(|n| n.parse::<u32>().ok());
                match (parse(it.next(), '+'), parse(it.next(), '-')) {
                    (Some(a), Some(b)) => (status.ahead, status.behind) = (a, b),
                    _ => bail!("unexpected git status branch line '{line}'"),
                }
            }
            _ => {}
        }
    }
    if status.branch.is_empty() {
        bail!("git status did not name the current branch");
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_local_branches_first_and_remote_only_ones_after() {
        let out = "refs/heads/feature\t \torigin/feature\nrefs/heads/main\t*\torigin/main\n\
                   refs/remotes/origin/HEAD\t \t\nrefs/remotes/origin/main\t \t\nrefs/remotes/origin/feature\t \t\n\
                   refs/remotes/origin/recipe/brave\t \t\n";
        let b = parse_branches(out);
        let names: Vec<_> = b.iter().map(|b| (b.name.as_str(), b.remote, b.current)).collect();
        assert_eq!(names, [("main", false, true), ("feature", false, false), ("origin/recipe/brave", true, false)]);
        assert_eq!(b[0].upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn branch_names_that_could_be_options_or_invalid_are_refused() {
        for ok in ["main", "recipe/brave-1.2", "jeff/fix_icons"] {
            assert!(is_valid_branch_name(ok), "{ok}");
        }
        for bad in ["", "-f", "--force", "a b", "a..b", "a~1", "x.lock", "/x", "x/", "a\\b", "@", "a@{1}", "a:b"] {
            assert!(!is_valid_branch_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn switching_creating_and_pushing_build_safe_commands() {
        if crate::process::which("git").is_none() {
            return;
        }
        let d = crate::workspace::fake_workspace(Some("developmentOnly: true\n"));
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        let remote = Branch { name: "origin/feature".into(), remote: true, current: false, upstream: None };
        assert_eq!(switch(&ws, &remote).unwrap().args, ["switch", "--track", "origin/feature"]);
        assert_eq!(create_branch(&ws, " new/one ").unwrap().args, ["switch", "-c", "new/one"]);
        assert!(create_branch(&ws, "-D").is_err());
        let tracked = Status { branch: "main".into(), upstream: Some("origin/main".into()), ahead: 1, behind: 0 };
        assert_eq!(push(&ws, &tracked).unwrap().args, ["push", "--progress"]);
        let new = Status { upstream: None, branch: "new/one".into(), ..tracked };
        assert_eq!(push(&ws, &new).unwrap().args, ["push", "--progress", "-u", "origin", "new/one"]);
        let prod = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        assert!(switch(&prod, &remote).unwrap_err().to_string().contains("production"));
    }

    #[test]
    fn parses_branch_headers() {
        let tracked = "# branch.oid f141877\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -5\n1 .M N... 100644 100644 100644 a b x.md\n";
        assert_eq!(parse_status(tracked).unwrap(), Status { branch: "main".into(), upstream: Some("origin/main".into()), ahead: 2, behind: 5 });
        // No upstream, or one that was deleted on the remote: nothing to compare.
        let local = "# branch.oid f141877\n# branch.head recipe/brave\n";
        assert_eq!(parse_status(local).unwrap(), Status { branch: "recipe/brave".into(), upstream: None, ahead: 0, behind: 0 });
        let gone = "# branch.oid f141877\n# branch.head main\n# branch.upstream origin/gone\n";
        assert_eq!(parse_status(gone).unwrap(), Status { branch: "main".into(), upstream: Some("origin/gone".into()), ahead: 0, behind: 0 });
        assert_eq!(parse_status("# branch.oid f141877\n# branch.head (detached)\n").unwrap().branch, "HEAD");
        assert!(parse_status("# branch.head main\n# branch.ab x y\n").is_err());
        assert!(parse_status("").is_err());
    }

    #[test]
    fn clone_creates_missing_parents() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join(r"a\b\YardstickDev");
        if crate::process::which("git").is_none() {
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
