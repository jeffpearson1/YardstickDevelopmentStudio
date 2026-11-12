//! The tools the Studio relies on - PowerShell 7, git, gh and an agent CLI:
//! finding them, installing a missing one with winget, and whether the ones
//! that talk to GitHub or an AI service are signed in. Everything here that
//! runs a process blocks: call it off the UI thread.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::agent::AgentBackend;
use crate::harness::ps_quote;
use crate::process::{ProcessSpec, find_program, launcher, run_output};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    PowerShell,
    Git,
    Gh,
    Agent(AgentBackend),
}

impl Tool {
    pub const ALL: [Tool; 5] = [Tool::PowerShell, Tool::Git, Tool::Gh, Tool::Agent(AgentBackend::CopilotCli), Tool::Agent(AgentBackend::ClaudeCode)];

    pub fn label(self) -> &'static str {
        match self {
            Tool::PowerShell => "PowerShell 7",
            Tool::Git => "Git",
            Tool::Gh => "GitHub CLI (gh)",
            Tool::Agent(b) => b.label(),
        }
    }

    /// The Studio cannot set up or test recipes without it.
    pub fn required(self) -> bool {
        matches!(self, Tool::PowerShell | Tool::Git)
    }

    pub fn winget_id(self) -> &'static str {
        match self {
            Tool::PowerShell => "Microsoft.PowerShell",
            Tool::Git => "Git.Git",
            Tool::Gh => "GitHub.cli",
            Tool::Agent(AgentBackend::CopilotCli) => "GitHub.Copilot",
            Tool::Agent(AgentBackend::ClaudeCode) => "Anthropic.ClaudeCode",
        }
    }

    /// Where to get it by hand when winget is not available.
    pub fn download_url(self) -> &'static str {
        match self {
            Tool::PowerShell => "https://aka.ms/powershell",
            Tool::Git => "https://git-scm.com/download/win",
            Tool::Gh => "https://cli.github.com",
            Tool::Agent(AgentBackend::CopilotCli) => "https://github.com/github/copilot-cli",
            Tool::Agent(AgentBackend::ClaudeCode) => "https://code.claude.com/docs/en/setup",
        }
    }

    /// Whether it has an account to sign in to.
    pub fn signs_in(self) -> bool {
        !matches!(self, Tool::PowerShell)
    }

    pub fn locate(self, configured_pwsh: Option<&Path>) -> Option<PathBuf> {
        match self {
            Tool::PowerShell => crate::setup::pwsh7(configured_pwsh),
            Tool::Git => find_program("git", &[]),
            Tool::Gh => find_program("gh", &[]),
            Tool::Agent(b) => b.locate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    SignedIn { account: Option<String> },
    SignedOut(String),
    /// Signed in, but the account cannot do what the Studio needs (push, say).
    NoAccess(String),
    /// The check itself failed (offline, say), which says nothing either way.
    Unknown(String),
}

impl SignIn {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, SignIn::SignedIn { .. })
    }

    /// Signing in (again) would help.
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, SignIn::SignedOut(_) | SignIn::NoAccess(_))
    }
}

/// What is installed and signed in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub paths: Vec<(Tool, Option<PathBuf>)>,
    pub sign_in: Vec<(Tool, SignIn)>,
    /// Whether the git check was against the workspace's `origin` (push
    /// access) rather than the repository URL (read access).
    pub git_checked_push: bool,
    pub winget: Option<PathBuf>,
}

/// Where the checks look.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub configured_pwsh: Option<PathBuf>,
    /// A development clone: git checks it can push to its `origin`.
    pub workspace: Option<PathBuf>,
    /// Without a workspace, git checks it can read this.
    pub repo_url: String,
}

impl Report {
    pub fn probe(cx: &Context) -> Report {
        let paths: Vec<(Tool, Option<PathBuf>)> = Tool::ALL.iter().map(|t| (*t, t.locate(cx.configured_pwsh.as_deref()))).collect();
        let path = |tool: Tool| paths.iter().find(|(t, _)| *t == tool).and_then(|(_, p)| p.clone());
        let host = host_of(&cx.repo_url);
        let push_root = cx.workspace.as_deref().filter(|r| crate::git::is_repo(r));
        let checks: Vec<(Tool, PathBuf)> = Tool::ALL.iter().filter(|t| t.signs_in()).filter_map(|t| path(*t).map(|p| (*t, p))).collect();
        // Each check can wait on the network; run them side by side.
        let sign_in = std::thread::scope(|s| {
            let handles: Vec<_> = checks
                .iter()
                .map(|(tool, exe)| {
                    let host = &host;
                    s.spawn(move || {
                        let state = match tool {
                            Tool::Gh => gh_status(exe, host),
                            Tool::Git => match push_root {
                                Some(root) => git_push_status(exe, root),
                                None => git_read_status(exe, &cx.repo_url),
                            },
                            Tool::Agent(AgentBackend::CopilotCli) => copilot_status(),
                            Tool::Agent(AgentBackend::ClaudeCode) => claude_status(exe),
                            Tool::PowerShell => unreachable!(),
                        };
                        (*tool, state)
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        Report { paths, sign_in, git_checked_push: push_root.is_some(), winget: winget() }
    }

    pub fn path(&self, tool: Tool) -> Option<&Path> {
        self.paths.iter().find(|(t, _)| *t == tool).and_then(|(_, p)| p.as_deref())
    }

    pub fn sign_in(&self, tool: Tool) -> Option<&SignIn> {
        self.sign_in.iter().find(|(t, _)| *t == tool).map(|(_, s)| s)
    }

    /// What needs doing before the Studio works fully, worded for a banner.
    /// `agent` is the agent CLI the user picked.
    pub fn attention(&self, agent: AgentBackend) -> Vec<String> {
        let mut out = Vec::new();
        for tool in [Tool::PowerShell, Tool::Git, Tool::Gh] {
            if self.path(tool).is_none() {
                out.push(format!("{} is not installed", tool.label()));
            }
        }
        let agent_tool = Tool::Agent(agent);
        if self.path(agent_tool).is_none() {
            out.push(format!("{} is not installed", agent.label()));
        }
        for tool in [Tool::Git, Tool::Gh, agent_tool] {
            match self.sign_in(tool) {
                Some(SignIn::SignedOut(_)) => out.push(format!("{} is not signed in", tool.label())),
                Some(SignIn::NoAccess(_)) if tool == Tool::Git => out.push("Git cannot push to the repository".to_string()),
                _ => {}
            }
        }
        out
    }
}

/// `github.com`, or the host of a GitHub Enterprise repository URL.
pub fn host_of(repo_url: &str) -> String {
    crate::promote::web_url(repo_url)
        .and_then(|u| u.strip_prefix("https://").and_then(|r| r.split('/').next()).map(str::to_string))
        .unwrap_or_else(|| "github.com".to_string())
}

/// gh keeps its sign-in under `%APPDATA%\GitHub CLI`, so without APPDATA it
/// reports "not logged in" even when it is. Some launchers leave it out.
pub fn with_user_env(spec: ProcessSpec) -> ProcessSpec {
    match (std::env::var_os("APPDATA"), dirs::config_dir()) {
        (None, Some(dir)) => spec.env("APPDATA", &dir.to_string_lossy()),
        _ => spec,
    }
}

fn gh_spec(gh: &Path, args: &[&str]) -> ProcessSpec {
    with_user_env(ProcessSpec::new("gh", gh, &std::env::temp_dir()).args(args.iter().copied()).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1"))
}

pub fn gh_status(gh: &Path, host: &str) -> SignIn {
    match run_output(&gh_spec(gh, &["auth", "status", "--hostname", host])) {
        Ok(out) => classify_gh(out.success, &format!("{}\n{}", out.stdout, out.stderr)),
        Err(e) => SignIn::Unknown(format!("{e:#}")),
    }
}

fn classify_gh(success: bool, text: &str) -> SignIn {
    if success {
        let account = text.lines().find_map(|l| l.split_once(" account ").map(|(_, rest)| rest.split_whitespace().next().unwrap_or_default().to_string()));
        return SignIn::SignedIn { account: account.filter(|a| !a.is_empty()) };
    }
    let lower = text.to_lowercase();
    let line = last_line(text);
    if lower.contains("not logged in") || lower.contains("gh auth login") || lower.contains("invalid") || lower.contains("expired") {
        SignIn::SignedOut(line)
    } else {
        SignIn::Unknown(line)
    }
}

fn git_spec(git: &Path, cwd: &Path, args: &[&str]) -> ProcessSpec {
    ProcessSpec::new("git", git, cwd)
        .args(args.iter().copied())
        // A check must never wait on a prompt: not git's, not Git Credential Manager's.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
}

/// The ref a push check names. `--dry-run` never creates it.
const CHECK_REF: &str = "HEAD:refs/heads/ystudio/sign-in-check";

/// Whether git can push to the clone's `origin`, without pushing anything.
pub fn git_push_status(git: &Path, root: &Path) -> SignIn {
    let spec = git_spec(git, root, &["push", "--dry-run", "--no-verify", "--porcelain", "origin", CHECK_REF]);
    match run_output(&spec) {
        Ok(out) => classify_git(out.success, &format!("{}\n{}", out.stdout, out.stderr)),
        Err(e) => SignIn::Unknown(format!("{e:#}")),
    }
}

/// Whether git can read `url` (a private repository needs a sign-in).
pub fn git_read_status(git: &Path, url: &str) -> SignIn {
    if url.trim().is_empty() {
        return SignIn::Unknown("no repository URL to check".into());
    }
    match run_output(&git_spec(git, &std::env::temp_dir(), &["ls-remote", "--heads", url.trim()])) {
        Ok(out) => classify_git(out.success, &out.stderr),
        Err(e) => SignIn::Unknown(format!("{e:#}")),
    }
}

fn classify_git(success: bool, text: &str) -> SignIn {
    if success {
        return SignIn::SignedIn { account: None };
    }
    let lower = text.to_lowercase();
    let line = last_line(text);
    if lower.contains("permission to") || lower.contains("denied to") || lower.contains("write access") {
        SignIn::NoAccess(line)
    } else if ["authentication failed", "could not read username", "terminal prompts disabled", "logon failed", "invalid username or password", "repository not found", "error: 401", "error: 403"]
        .iter()
        .any(|m| lower.contains(m))
    {
        SignIn::SignedOut(line)
    } else {
        SignIn::Unknown(line)
    }
}

/// Copilot CLI has no status command. A token in the environment wins;
/// otherwise `config.json` lists the accounts that signed in with `copilot login`.
pub fn copilot_status() -> SignIn {
    for var in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            return SignIn::SignedIn { account: Some(format!("token in {var}")) };
        }
    }
    let home = std::env::var_os("COPILOT_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".copilot")));
    let Some(config) = home.map(|h| h.join("config.json")) else {
        return SignIn::Unknown("no home folder".into());
    };
    match crate::read_text(&config) {
        Ok(text) => copilot_account(&text),
        Err(_) => SignIn::SignedOut("Copilot CLI has not been signed in on this computer".into()),
    }
}

fn copilot_account(config: &str) -> SignIn {
    // The file starts with `//` comment lines.
    let json: String = config.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
        return SignIn::Unknown("could not read Copilot CLI's config.json".into());
    };
    let login = |v: &serde_json::Value| v.get("login").and_then(|l| l.as_str()).map(str::to_string);
    let users = value.get("loggedInUsers").and_then(|u| u.as_array()).cloned().unwrap_or_default();
    if users.is_empty() {
        return SignIn::SignedOut("no account has signed in to Copilot CLI".into());
    }
    let account = value.get("lastLoggedInUser").and_then(login).or_else(|| users.iter().find_map(login));
    SignIn::SignedIn { account }
}

/// `claude auth status` prints JSON and exits 1 when signed out.
pub fn claude_status(exe: &Path) -> SignIn {
    let Ok((program, prefix)) = launcher(exe) else {
        return SignIn::Unknown(format!("cannot run {}", exe.display()));
    };
    let base = || ProcessSpec::new("claude", &program, &std::env::temp_dir()).args(prefix.iter().cloned()).env("NO_COLOR", "1");
    // Versions before 2 have no `auth` command and would read it as a prompt.
    let version = run_output(&base().args(["--version"])).map(|o| o.stdout).unwrap_or_default();
    if !version.trim_start().split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m >= 2) {
        return SignIn::Unknown(format!("Claude Code {} is too old to report its sign-in; update it", version.trim()));
    }
    match run_output(&base().args(["auth", "status"])) {
        Ok(out) => classify_claude(out.success, &out.stdout, &out.stderr),
        Err(e) => SignIn::Unknown(format!("{e:#}")),
    }
}

fn classify_claude(success: bool, stdout: &str, stderr: &str) -> SignIn {
    let json = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok();
    let method = json.as_ref().and_then(|j| j.get("authMethod")).and_then(|m| m.as_str()).unwrap_or_default().to_string();
    if success && method != "none" {
        let account = json.as_ref().and_then(find_email).or_else(|| (!method.is_empty()).then(|| method.replace('_', " ")));
        return SignIn::SignedIn { account };
    }
    if json.is_some() {
        SignIn::SignedOut("Claude Code is not signed in".into())
    } else {
        SignIn::Unknown(last_line(&format!("{stdout}\n{stderr}")))
    }
}

/// The first string under a key that mentions "email", at any depth.
fn find_email(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => map
            .iter()
            .find_map(|(k, v)| v.as_str().filter(|s| k.to_lowercase().contains("email") && !s.is_empty()).map(str::to_string))
            .or_else(|| map.values().find_map(find_email)),
        _ => None,
    }
}

fn last_line(text: &str) -> String {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no output").to_string()
}

pub fn winget() -> Option<PathBuf> {
    let apps = dirs::data_local_dir().map(|d| d.join(r"Microsoft\WindowsApps")).unwrap_or_default();
    find_program("winget", &[apps])
}

/// `winget install` for `tool`, run as a job so its output is logged.
/// Machine-wide installers (PowerShell, Git) ask for administrator rights.
pub fn install_spec(tool: Tool) -> Result<ProcessSpec> {
    let winget = winget().ok_or_else(|| anyhow!("winget (App Installer) was not found. Install {} from {} instead.", tool.label(), tool.download_url()))?;
    Ok(ProcessSpec::new(format!("Install {}", tool.label()), winget, &std::env::temp_dir()).args([
        "install",
        "--id",
        tool.winget_id(),
        "--exact",
        "--source",
        "winget",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]))
}

/// A console window that runs PowerShell `script`, then waits for Enter so
/// whatever happened stays readable.
fn console(label: &str, script: &str) -> ProcessSpec {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let ps = PathBuf::from(system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let script = format!("{script}\nWrite-Host ''\nRead-Host 'Done. Press Enter to close this window'");
    let mut spec = with_user_env(ProcessSpec::new(label, ps, &std::env::temp_dir()).args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script]));
    spec.new_console = true;
    spec
}

/// `& 'program' 'arg' ...` for a console script.
fn invoke(exe: &Path, args: &[&str]) -> Result<String> {
    let (program, prefix) = launcher(exe)?;
    let parts: Vec<String> = std::iter::once(program.to_string_lossy().into_owned()).chain(prefix).chain(args.iter().map(|a| a.to_string())).map(|p| ps_quote(&p)).collect();
    Ok(format!("& {}", parts.join(" ")))
}

/// A console where the user signs `tool` in. `exe` is the tool's own
/// executable, `gh` is used for git when it is installed (it signs git in
/// too), and `workspace` is the clone whose `origin` git pushes to.
pub fn sign_in_spec(tool: Tool, exe: &Path, gh: Option<&Path>, workspace: Option<&Path>, repo_url: &str) -> Result<ProcessSpec> {
    let host = host_of(repo_url);
    let gh_login = |gh: &Path| -> Result<String> {
        Ok(format!(
            "Write-Host 'Signing in to {host} with the GitHub CLI. Choose HTTPS, and let it sign git in too.'\n{}\nif ($LASTEXITCODE -eq 0) {{ {} }}",
            invoke(gh, &["auth", "login", "--hostname", &host, "--web", "--git-protocol", "https"])?,
            invoke(gh, &["auth", "setup-git", "--hostname", &host])?,
        ))
    };
    let script = match tool {
        Tool::PowerShell => anyhow::bail!("PowerShell has nothing to sign in to."),
        Tool::Gh => gh_login(exe)?,
        Tool::Git => match gh {
            Some(gh) => gh_login(gh)?,
            // Git Credential Manager shows its own sign-in when git needs one.
            None => match workspace {
                Some(root) => format!(
                    "Set-Location -LiteralPath {}\nWrite-Host 'Checking that git can push to origin. Sign in if Git Credential Manager asks.'\n{}",
                    ps_quote(&root.to_string_lossy()),
                    invoke(exe, &["push", "--dry-run", "--no-verify", "origin", CHECK_REF])?
                ),
                None => format!("Write-Host 'Checking that git can read {url}. Sign in if Git Credential Manager asks.'\n{}", invoke(exe, &["ls-remote", "--heads", repo_url.trim()])?, url = repo_url.trim()),
            },
        },
        Tool::Agent(AgentBackend::CopilotCli) => invoke(exe, &["login"])?,
        Tool::Agent(AgentBackend::ClaudeCode) => invoke(exe, &["auth", "login"])?,
    };
    Ok(console(&format!("Sign in to {}", tool.label()), &script))
}

/// Points git at gh's sign-in for the repository's host (no window).
pub fn gh_setup_git_spec(gh: &Path, repo_url: &str) -> ProcessSpec {
    let host = host_of(repo_url);
    let mut spec = gh_spec(gh, &["auth", "setup-git", "--hostname", &host]);
    spec.label = "Sign git in with gh".into();
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gh_status() {
        let ok = "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n  - Active account: true\n";
        assert_eq!(classify_gh(true, ok), SignIn::SignedIn { account: Some("octocat".into()) });
        let out = "You are not logged into any GitHub hosts. To log in, run: gh auth login";
        assert!(matches!(classify_gh(false, out), SignIn::SignedOut(_)));
        let bad = "github.com\n  X Failed to log in to github.com account octocat (keyring)\n  - The token in keyring is invalid.\n";
        assert!(matches!(classify_gh(false, bad), SignIn::SignedOut(_)));
        assert!(matches!(classify_gh(false, "error connecting to api.github.com"), SignIn::Unknown(_)));
    }

    #[test]
    fn reads_git_push_failures() {
        assert_eq!(classify_git(true, "To https://github.com/o/r\n*\tHEAD:refs/heads/x\t[new branch]\nDone"), SignIn::SignedIn { account: None });
        let signed_out = "fatal: could not read Username for 'https://github.com': terminal prompts disabled";
        assert!(matches!(classify_git(false, signed_out), SignIn::SignedOut(_)));
        let denied = "remote: Permission to o/r.git denied to someone.\nfatal: unable to access 'https://github.com/o/r/': The requested URL returned error: 403";
        assert!(matches!(classify_git(false, denied), SignIn::NoAccess(_)));
        assert!(matches!(classify_git(false, "fatal: unable to access: Could not resolve host: github.com"), SignIn::Unknown(_)));
    }

    #[test]
    fn reads_copilot_config() {
        let config = "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"loggedInUsers\": [{ \"host\": \"https://github.com\", \"login\": \"octocat\" }],\n  \"lastLoggedInUser\": { \"host\": \"https://github.com\", \"login\": \"octocat\" }\n}";
        assert_eq!(copilot_account(config), SignIn::SignedIn { account: Some("octocat".into()) });
        assert!(matches!(copilot_account("{ \"loggedInUsers\": [] }"), SignIn::SignedOut(_)));
        assert!(matches!(copilot_account("{}"), SignIn::SignedOut(_)));
        assert!(matches!(copilot_account("not json"), SignIn::Unknown(_)));
    }

    #[test]
    fn reads_claude_status() {
        let signed_in = r#"{"loggedIn": true, "authMethod": "claude.ai", "account": {"emailAddress": "dev@example.com"}}"#;
        assert_eq!(classify_claude(true, signed_in, ""), SignIn::SignedIn { account: Some("dev@example.com".into()) });
        assert_eq!(classify_claude(true, r#"{"authMethod": "api_key"}"#, ""), SignIn::SignedIn { account: Some("api key".into()) });
        assert!(matches!(classify_claude(false, r#"{"loggedIn": false, "authMethod": "none"}"#, ""), SignIn::SignedOut(_)));
        assert!(matches!(classify_claude(false, "", "boom"), SignIn::Unknown(_)));
    }

    #[test]
    fn attention_lists_what_blocks_the_studio() {
        let report = Report {
            paths: vec![(Tool::PowerShell, Some("pwsh.exe".into())), (Tool::Git, None), (Tool::Gh, Some("gh.exe".into())), (Tool::Agent(AgentBackend::CopilotCli), Some("copilot.exe".into()))],
            sign_in: vec![(Tool::Gh, SignIn::SignedOut("x".into())), (Tool::Agent(AgentBackend::CopilotCli), SignIn::Unknown("offline".into()))],
            ..Default::default()
        };
        assert_eq!(report.attention(AgentBackend::CopilotCli), ["Git is not installed", "GitHub CLI (gh) is not signed in"]);
        assert_eq!(report.attention(AgentBackend::ClaudeCode), ["Git is not installed", "Claude Code is not installed", "GitHub CLI (gh) is not signed in"]);
        let no_gh = Report { paths: vec![(Tool::PowerShell, Some("pwsh.exe".into())), (Tool::Git, Some("git.exe".into())), (Tool::Agent(AgentBackend::CopilotCli), Some("copilot.exe".into()))], ..Default::default() };
        assert_eq!(no_gh.attention(AgentBackend::CopilotCli), ["GitHub CLI (gh) is not installed"]);
    }

    #[test]
    fn hosts_and_install_commands() {
        assert_eq!(host_of("https://github.com/o/r"), "github.com");
        assert_eq!(host_of("git@ghe.example.com:o/r.git"), "ghe.example.com");
        assert_eq!(host_of(""), "github.com");
        if winget().is_some() {
            let s = install_spec(Tool::Git).unwrap();
            assert_eq!(&s.args[..3], ["install", "--id", "Git.Git"]);
            assert!(s.args.iter().any(|a| a == "--accept-package-agreements"));
        }
    }

    #[test]
    fn sign_in_consoles_wait_and_quote() {
        let s = sign_in_spec(Tool::Gh, Path::new(r"C:\Program Files\GitHub CLI\gh.exe"), None, None, "https://github.com/o/r").unwrap();
        assert!(s.new_console);
        let script = s.args.last().unwrap();
        assert!(script.contains(r"& 'C:\Program Files\GitHub CLI\gh.exe' 'auth' 'login' '--hostname' 'github.com'"), "{script}");
        assert!(script.contains("'setup-git'") && script.contains("Read-Host"));
        let git = sign_in_spec(Tool::Git, Path::new("git.exe"), None, Some(Path::new(r"C:\it's")), "").unwrap();
        assert!(git.args.last().unwrap().contains(r"Set-Location -LiteralPath 'C:\it''s'"));
        assert!(sign_in_spec(Tool::PowerShell, Path::new("pwsh.exe"), None, None, "").is_err());
    }
}
