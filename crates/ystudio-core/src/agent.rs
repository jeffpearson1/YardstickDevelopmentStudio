//! Agentic recipe authoring through a coding-agent CLI (GitHub Copilot CLI or
//! Claude Code) that follows the repository's `yardstick-recipes` skill.
//!
//! The Studio does not trust the agent with anything irreversible. The prompt
//! restricts it to the development folder and to schema/dry-run testing; live
//! installs and promotion stay behind the Studio's own confirmation dialogs.
//! Where the CLI supports it, the same rules are also enforced with tool denials
//! and by not granting access to any other folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::process::{ProcessSpec, find_program, launcher};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AgentBackend {
    #[default]
    CopilotCli,
    ClaudeCode,
}

impl AgentBackend {
    pub const ALL: [AgentBackend; 2] = [AgentBackend::CopilotCli, AgentBackend::ClaudeCode];

    pub fn label(self) -> &'static str {
        match self {
            AgentBackend::CopilotCli => "GitHub Copilot CLI",
            AgentBackend::ClaudeCode => "Claude Code",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            AgentBackend::CopilotCli => "copilot",
            AgentBackend::ClaudeCode => "claude",
        }
    }

    /// Where the CLI's installers put it, in case PATH does not have it yet.
    fn install_dirs(self) -> Vec<PathBuf> {
        let local = dirs::data_local_dir().unwrap_or_default();
        let npm = dirs::data_dir().unwrap_or_default().join("npm");
        let winget = local.join(r"Microsoft\WinGet\Links");
        match self {
            AgentBackend::CopilotCli => vec![local.join("GitHubCopilotCLI"), winget, npm],
            AgentBackend::ClaudeCode => vec![dirs::home_dir().unwrap_or_default().join(r".local\bin"), winget, npm],
        }
    }

    pub fn locate(self) -> Option<PathBuf> {
        find_program(self.executable(), &self.install_dirs())
    }

    /// A process for this CLI, ready for arguments.
    fn spec(self, label: impl Into<String>, cwd: &Path) -> Result<ProcessSpec> {
        let exe = self.locate().ok_or_else(|| {
            anyhow::anyhow!("{} (`{}`) was not found on PATH or in its usual install folders.", self.label(), self.executable())
        })?;
        let (program, prefix) = launcher(&exe)?;
        Ok(ProcessSpec::new(label, program, cwd).args(prefix).env("NO_COLOR", "1"))
    }

    /// Models the CLI accepts for `--model`. Copilot CLI lists them in
    /// `copilot help config`, so the list follows the installed version;
    /// Claude Code has no such listing, so its stable aliases are offered.
    /// Runs a process for Copilot CLI: call it off the UI thread.
    pub fn models(self) -> Result<Vec<String>> {
        match self {
            AgentBackend::CopilotCli => {
                let spec = self.spec("list models", &std::env::temp_dir())?.args(["help", "config"]);
                let models = parse_copilot_models(&crate::process::run_capture(&spec)?);
                if models.is_empty() {
                    bail!("`copilot help config` listed no models.");
                }
                Ok(models)
            }
            AgentBackend::ClaudeCode => Ok(["opus", "sonnet", "haiku"].map(String::from).to_vec()),
        }
    }
}

/// The quoted values under the `model` setting in `copilot help config`.
fn parse_copilot_models(help: &str) -> Vec<String> {
    let mut lines = help.lines().skip_while(|l| !l.trim_start().starts_with("`model`:")).skip(1);
    let mut models = Vec::new();
    for line in lines.by_ref() {
        let Some(item) = line.trim().strip_prefix("- ") else { break };
        let value = item.trim().trim_matches('"');
        if value.is_empty() || value.contains(char::is_whitespace) {
            break;
        }
        models.push(value.to_string());
    }
    models
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentTask {
    /// Research, write and dry-run a brand-new recipe.
    Create { app: String, id_hint: String, vendor_url: String, notes: String },
    /// Fix a recipe, optionally with the failure evidence the Studio collected.
    Fix { id: String, evidence: String, notes: String },
    /// Review an existing recipe against the skill without changing it.
    Review { id: String },
    /// Bring a recipe up to date (vendor changed URL, version scheme, switches).
    Update { id: String, notes: String },
    Freeform { prompt: String },
}

impl AgentTask {
    pub fn title(&self) -> String {
        match self {
            AgentTask::Create { app, .. } => format!("Create recipe · {app}"),
            AgentTask::Fix { id, .. } => format!("Fix recipe · {id}"),
            AgentTask::Review { id } => format!("Review recipe · {id}"),
            AgentTask::Update { id, .. } => format!("Update recipe · {id}"),
            AgentTask::Freeform { prompt } => format!("Agent · {}", prompt.chars().take(40).collect::<String>()),
        }
    }

    /// `create`, `fix`, `review`, `update` or `freeform`.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentTask::Create { .. } => "create",
            AgentTask::Fix { .. } => "fix",
            AgentTask::Review { .. } => "review",
            AgentTask::Update { .. } => "update",
            AgentTask::Freeform { .. } => "freeform",
        }
    }

    /// The recipe the task works on, if any.
    pub fn recipe_id(&self) -> Option<String> {
        match self {
            AgentTask::Create { app, id_hint, .. } => {
                Some(if id_hint.trim().is_empty() { crate::recipe::suggest_id(app) } else { id_hint.trim().to_string() })
            }
            AgentTask::Fix { id, .. } | AgentTask::Review { id } | AgentTask::Update { id, .. } => Some(id.clone()),
            AgentTask::Freeform { .. } => None,
        }
    }

    /// Whether the agent may edit files for this task.
    pub fn writes(&self) -> bool {
        !matches!(self, AgentTask::Review { .. })
    }
}

/// The line an agent ends with when a command it needed was not approved.
pub const NEEDS_APPROVAL: &str = "STUDIO-NEEDS-APPROVAL:";

/// How an agent run works, chosen by the user before it starts. The agent
/// CLI runs headless, so it cannot ask; anything it would ask about is denied
/// unless approved here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentOptions {
    /// Approved: download vendor files and unpack/inspect them with tools
    /// anywhere on this computer (Copilot CLI's path checks are lifted).
    pub downloads: bool,
    /// A coordinating agent splits the work across parallel subagents
    /// (Copilot CLI's fleet mode; Claude Code's subagents).
    pub multi_agent: bool,
}

/// The coordinator's instructions, when multi-agent mode is on.
fn coordinator_prompt(options: AgentOptions) -> &'static str {
    if options.multi_agent {
        "Multi-agent mode is on: work as the coordinator. Split the work into independent parts that can run in \
         parallel - for example the vendor's download source and versioning, the silent install and uninstall \
         switches, detection, and the icon - and hand each to a subagent. Give every subagent these same rules and \
         approvals. You stay responsible for the recipe file: merge the findings, write the recipe once, run the \
         checks, and write the final report yourself.\n"
    } else {
        ""
    }
}

/// The approvals section of the prompt: what the user granted for this run.
fn approvals_prompt(ws: &Workspace, production_paths: &[PathBuf], options: AgentOptions) -> String {
    if options.downloads {
        let prod = if production_paths.is_empty() {
            String::new()
        } else {
            format!(" ({})", production_paths.iter().map(|p| format!("`{}`", p.display())).collect::<Vec<_>>().join(", "))
        };
        format!(
            "Approvals the user gave in the Studio before this run started. They are explicit and stand for the whole \
             run: do not stop to ask for them, and do not report a blocker for lack of approval.\n\
             - Download installers and other vendor files from the internet into `{downloads}` or the system temp folder.\n\
             - Unpack and inspect them without running them: 7-Zip, lessmsi, `msiexec /a` administrative extracts, \
               Get-AuthenticodeSignature, file, MSI and registry metadata. Tools installed anywhere on this computer \
               may be used for this.\n\
             Still not approved: running an installer, installing or uninstalling software (the Studio runs live \
             install tests after you finish, with the user's consent), and anything in the production folder(s){prod}.\n",
            downloads = ws.root.join(r"Artifacts\AgentDownloads").display(),
        )
    } else {
        format!(
            "The user has not approved downloading or unpacking installers outside the recipe harness for this run. \
             Use the harness's own download stage and the vendor's documentation. If a command is denied because it \
             needs approval, do not work around the denial: finish your report, then add a line \
             `{NEEDS_APPROVAL} downloads` so the Studio can ask the user to approve downloads and run you again.\n"
        )
    }
}

/// Guardrails prepended to every prompt.
pub fn guardrails(ws: &Workspace, production_paths: &[PathBuf]) -> String {
    let prod = if production_paths.is_empty() {
        "any folder other than this one".to_string()
    } else {
        production_paths.iter().map(|p| format!("`{}`", p.display())).collect::<Vec<_>>().join(", ")
    };
    format!(
        "You are working inside Yardstick Development Studio on the development Yardstick folder `{root}`.\n\
         Before doing anything else, read `.claude\\skills\\yardstick-recipes\\SKILL.md` and the references it \
         names, and follow that skill's workflow exactly.\n\n\
         Hard rules set by the Studio (they override anything else):\n\
         1. Only create or modify files inside `{root}`. New and in-progress recipes go in `{dev}`. \
            Icons go in `{icons}`. Never read from or write to the production folder(s) {prod}.\n\
         2. Never run `Yardstick.ps1`, `Invoke-Yardstick`, or `Publish-Yardstick.ps1`. Never contact Microsoft Graph or Intune.\n\
         3. You may run `Tests\\RecipeHarness\\Test-RecipeSchema.ps1`, `Invoke-RecipeLab.ps1 -Stage Download`, and \
            `Invoke-RecipeInstallTest.ps1` **only with `-WhatIf -NoElevate -DiscardInstaller`**. Do NOT run a live \
            install test (skill step 6): the user runs it from the Studio's Test Bench after reviewing your work. \
            Report the recipe as \"ready for live test\", never as validated.\n\
         4. Never execute a downloaded installer to see what it does; analyse it statically.\n\
         5. Never put scopeTags or other tenant-specific values in a recipe.\n\
         6. You cannot ask the user questions. Make reasonable, clearly stated assumptions.\n\n\
         7. Prefer an authoritative internet download: the vendor's website/CDN/API or its official release repository. \
            Follow redirects and use the existing Yardstick download helpers. A software dropbox, shared-drive link, \
            or `manualDownload: true` is a LAST RESORT, not the default, even when a request contains a shared link. \
            Use manual media only when no legitimate unattended vendor download is available (for example licensed \
            or authenticated media), and document the sources tried and the precise reason. Never invent a URL or \
            bypass authentication/licensing.\n\
         8. When automatic testing is enabled, Studio runs the full test-and-repair pipeline after you finish. Run safe checks to inform your work, \
            but do not run live tests yourself or claim validation. On a repair task, fix the supplied failure, \
            then return so Studio can re-run the harness. If the cause is environmental, do not edit the recipe: \
            finish with `STUDIO-BLOCKED: <reason>` as well as the recipe report.\n\n\
         Finish with the skill's final report (recipe path, resolved version, vendor sources, schema and -WhatIf \
         results, warnings, harness summary directory), then a line `STUDIO-RECIPES: <comma-separated ids you created or changed>`.\n",
        root = ws.root.display(),
        dev = ws.layout.development_recipes.display(),
        icons = ws.layout.icons.display(),
    )
}

pub fn task_prompt(task: &AgentTask) -> String {
    match task {
        AgentTask::Create { app, id_hint, vendor_url, notes } => {
            let mut p = format!("Task: package **{app}** for Intune by writing a new Yardstick recipe.\n");
            if !id_hint.trim().is_empty() {
                p += &format!("Use the recipe id `{}` (file `Recipes\\Development\\{}.yaml`).\n", id_hint.trim(), id_hint.trim());
            }
            if !vendor_url.trim().is_empty() {
                let label = if vendor_start_url(vendor_url, &[]).is_empty() {
                    "Request-supplied fallback media (research the vendor first)"
                } else { "Vendor page to start from" };
                p += &format!("{label}: {}\n", vendor_url.trim());
            }
            if !notes.trim().is_empty() {
                p += &format!("Notes from the user:\n{}\n", notes.trim());
            }
            p += "Research the vendor's authoritative download source, silent switches and uninstall path, look at \
                  sibling recipes for the same installer family, write the recipe from the matching template, make \
                  sure its icon exists, then run schema validation and the -WhatIf dry run and fix what they report.";
            p
        }
        AgentTask::Fix { id, evidence, notes } => {
            let mut p = format!("Task: the recipe `{id}` is failing. Diagnose and fix it.\n");
            if !evidence.trim().is_empty() {
                p += &format!("Evidence collected by the Studio:\n```\n{}\n```\n", truncate(evidence.trim(), 12_000));
            }
            if !notes.trim().is_empty() {
                p += &format!("Notes from the user:\n{}\n", notes.trim());
            }
            p += "Follow the skill's 'Reading failures' guidance: do not change the recipe in response to an \
                  environmental failure (e.g. MSI 1618); say so instead. If the recipe is shipped (not in \
                  Recipes\\Development), copy it to Recipes\\Development first and edit the copy. Re-run schema and \
                  -WhatIf afterwards.";
            p
        }
        AgentTask::Review { id } => format!(
            "Task: review the recipe `{id}` against the skill and references/fields.md. Do NOT modify any files. \
             Report concrete problems (wrong detection type, version set too late, hard-coded product codes, \
             fragile URLs, missing exit codes, reserved variables) with the exact fix for each, ordered by severity. \
             You may run schema validation."
        ),
        AgentTask::Update { id, notes } => {
            let mut p = format!(
                "Task: bring the recipe `{id}` up to date with the vendor's current download source, version scheme, \
                 and silent install/uninstall switches. If it is a shipped recipe, copy it to Recipes\\Development \
                 and edit the copy.\n"
            );
            if !notes.trim().is_empty() {
                p += &format!("Notes from the user:\n{}\n", notes.trim());
            }
            p
        }
        AgentTask::Freeform { prompt } => prompt.clone(),
    }
}

pub fn full_prompt(ws: &Workspace, production_paths: &[PathBuf], task: &AgentTask, options: AgentOptions) -> String {
    format!(
        "{}\n{}{}\n---\n\n{}",
        guardrails(ws, production_paths),
        approvals_prompt(ws, production_paths, options),
        coordinator_prompt(options),
        task_prompt(task)
    )
}

/// Whether the agent stopped because a command needed an approval this run
/// did not have: it said so, or Copilot CLI denied a command headlessly.
pub fn needs_approval(output: &str) -> bool {
    output.lines().any(|l| l.trim().trim_start_matches(['*', '`']).starts_with(NEEDS_APPROVAL))
        || output.contains("no interactive user response was available")
}

/// Avoid treating a request's shared-media link as its vendor research source.
pub fn vendor_start_url(vendor: &str, urls: &[String]) -> String {
    std::iter::once(vendor).chain(urls.iter().map(String::as_str))
        .find(|url| {
            let url = url.trim().to_ascii_lowercase();
            let Some(authority) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else { return false };
            let host = authority.split(['/', '?', '#']).next().unwrap_or_default()
                .rsplit('@').next().unwrap_or_default().split(':').next().unwrap_or_default();
            !host.is_empty() && !["dropbox.com", "dropboxusercontent.com", "sharepoint.com", "onedrive.com", "1drv.ms", "box.com"]
                .iter().any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        })
        .unwrap_or_default().trim().to_string()
}

pub fn blocked_reason(output: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        line.trim().trim_start_matches(['*', '`']).strip_prefix("STUDIO-BLOCKED:")
            .map(|reason| reason.trim().trim_end_matches(['*', '`']).to_string())
    })
}

/// Builds the agent CLI invocation. Refuses production workspaces.
pub fn build_command(
    ws: &Workspace,
    backend: AgentBackend,
    model: &str,
    production_paths: &[PathBuf],
    task: &AgentTask,
    options: AgentOptions,
) -> Result<ProcessSpec> {
    ws.ensure_writable("run a coding agent")?;
    if !ws.has_skill() {
        bail!("This Yardstick folder has no .claude\\skills\\yardstick-recipes\\SKILL.md; update it from GitHub first.");
    }
    let prompt = full_prompt(ws, production_paths, task, options);
    let root = ws.root.to_string_lossy().into_owned();
    let mut spec = backend.spec(task.title(), &ws.root)?;
    match backend {
        AgentBackend::CopilotCli => {
            let usage = crate::usage::Pending::new(task.kind(), task.recipe_id(), backend.label(), options.multi_agent);
            spec = spec.args(copilot_args(&prompt, &root, task.writes(), options));
            spec = spec.arg("--usage-output-file").arg(usage.file.to_string_lossy());
            spec.usage = Some(usage);
        }
        AgentBackend::ClaudeCode => {
            let mut allowed = if task.writes() {
                "Bash Read Edit Write Glob Grep WebFetch WebSearch TodoWrite Skill".to_string()
            } else {
                "Bash Read Glob Grep WebFetch WebSearch Skill".to_string()
            };
            if options.multi_agent {
                // Claude Code's subagents run through its Task tool.
                allowed += " Task";
            }
            let denied: Vec<String> = DENIED_COMMANDS.iter().map(|c| format!("Bash({c}:*)")).collect();
            spec = spec.args(["-p", &prompt, "--add-dir", &root, "--permission-mode", "acceptEdits", "--allowedTools", &allowed]);
            spec = spec.arg("--disallowedTools").arg(denied.join(" "));
        }
    }
    if !model.trim().is_empty() {
        spec = spec.args(["--model", model.trim()]);
    }
    Ok(spec)
}

/// Prompt for turning a batch's request list into structured items.
pub fn extract_prompt(batch_dir: &Path) -> String {
    format!(
        "You are helping an Intune packaging team triage a list of requests exported from a ticketing system.\n\n\
         Files in `{dir}`:\n\
         - `digest.md`: every request (ticket number, title, description, useful fields).\n\
         - `recipes.md`: the software the team already packages (Yardstick recipe ids).\n\n\
         For every request, list each separate piece of software that needs to be packaged or updated. One ticket \
         can ask for many apps (bulleted lists, bare lists of names, \"X with Y\"), and some tickets are not about \
         software at all (meetings, group changes, device tags, onboarding) - record those once with \
         `\"request\": \"not-software\"` and the ticket title as `app`. Drivers and plug-ins count as software.\n\
         Use the product's proper name and publisher; you may search the web to identify a product or its vendor \
         page, but do not download installers. Mark software that matches a recipe in `recipes.md` as `existing` \
         (or `update` if the requester needs a newer version or the request says the deployed one is outdated) and \
         set `existingRecipe` to that id. Keep links the requester gave in `urls`; mention shared-drive links \
         (Dropbox, OneDrive, Box) in `notes` as fallback media only. Find the authoritative vendor product/download \
         page for `vendorUrl` whenever possible; do not assume a shared link requires manualDownload. Put licensing, \
         architecture, prerequisites and anything a packager should know in `notes`.\n\n\
         Write the result as JSON to `{out}` in exactly this shape (the comments explain the fields; do not write \
         comments in the file):\n```\n{schema}\n```\n\
         Only create that one file. Do not modify any other file. When done, reply with a one-line summary.\n",
        dir = batch_dir.display(),
        out = batch_dir.join(crate::intake::Batch::AGENT_OUTPUT).display(),
        schema = crate::intake::AGENT_SCHEMA,
    )
}

/// Agent invocation for batch extraction: confined to the batch folder, no shell.
pub fn build_extract_command(backend: AgentBackend, model: &str, batch_dir: &Path, options: AgentOptions) -> Result<ProcessSpec> {
    let prompt = extract_prompt(batch_dir);
    let dir = batch_dir.to_string_lossy().into_owned();
    let mut spec = backend.spec("Extract software from request list", batch_dir)?;
    match backend {
        AgentBackend::CopilotCli => {
            spec = spec.args(["-p", &prompt, "--no-color", "--no-ask-user", "--stream", "on", "--add-dir", &dir]);
            spec = spec.args(["--allow-all-tools", "--allow-all-urls", "--deny-tool=shell"]);
            if options.multi_agent {
                spec = spec.arg("--fleet");
            }
            let usage = crate::usage::Pending::new("extract", None, backend.label(), options.multi_agent);
            spec = spec.arg("--usage-output-file").arg(usage.file.to_string_lossy());
            spec.usage = Some(usage);
        }
        AgentBackend::ClaudeCode => {
            let tools = if options.multi_agent { "Read Write Glob Grep WebFetch WebSearch Task" } else { "Read Write Glob Grep WebFetch WebSearch" };
            spec = spec.args(["-p", &prompt, "--add-dir", &dir, "--permission-mode", "acceptEdits"]);
            spec = spec.args(["--allowedTools", tools, "--disallowedTools", "Bash Edit"]);
        }
    }
    if !model.trim().is_empty() {
        spec = spec.args(["--model", model.trim()]);
    }
    Ok(spec)
}

/// Copilot CLI arguments for an agent run. It runs headless, so anything it
/// would ask about is denied: tools and URLs are allowed outright, and paths
/// outside the workspace and temp folder only when downloads are approved.
/// Deny rules beat every allow, so the denied commands stay denied.
fn copilot_args(prompt: &str, root: &str, writes: bool, options: AgentOptions) -> Vec<String> {
    let mut args: Vec<String> = ["-p", prompt, "--no-color", "--no-ask-user", "--stream", "on", "--add-dir", root]
        .map(String::from)
        .to_vec();
    // Web access is needed to research vendors.
    args.extend(["--allow-all-tools".into(), "--allow-all-urls".into()]);
    if options.multi_agent {
        // A coordinator with parallel subagents.
        args.push("--fleet".into());
    }
    if options.downloads {
        // Unpacking and inspecting installers uses tools outside the
        // workspace (7-Zip in Program Files, say), which the path check
        // would otherwise refuse with nobody there to approve it.
        args.push("--allow-all-paths".into());
    }
    args.extend(DENIED_COMMANDS.iter().map(|d| format!("--deny-tool=shell({d})")));
    if !writes {
        args.push("--deny-tool=write".into());
    }
    args
}

/// Command prefixes the agent's shell tool may never run.
pub const DENIED_COMMANDS: &[&str] = &[
    ".\\Yardstick.ps1",
    "./Yardstick.ps1",
    "Yardstick.ps1",
    "Invoke-Yardstick",
    ".\\Publish-Yardstick.ps1",
    "Publish-Yardstick.ps1",
    "git push",
];

/// The `STUDIO-RECIPES:` line from an agent's final report.
pub fn reported_recipes(output: &str) -> Vec<String> {
    output
        .lines()
        .rev()
        .find_map(|l| l.trim().trim_start_matches(['*', '`']).strip_prefix("STUDIO-RECIPES:"))
        .map(|rest| {
            rest.split(',')
                .map(|s| s.trim().trim_matches(['`', '*', '.']).to_string())
                .filter(|s| !s.is_empty() && crate::recipe::is_valid_id(s))
                .collect()
        })
        .unwrap_or_default()
}

/// File modification snapshot of the folders an agent may touch.
#[derive(Debug, Clone, Default)]
pub struct Snapshot(BTreeMap<PathBuf, (SystemTime, u64)>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Change::Added(p) | Change::Modified(p) | Change::Removed(p) => p,
        }
    }
}

impl Snapshot {
    pub fn take(ws: &Workspace) -> Self {
        let mut map = BTreeMap::new();
        for dir in [&ws.layout.recipes, &ws.layout.icons, &ws.root.join("Modules")] {
            walk(dir, &mut map);
        }
        Snapshot(map)
    }

    pub fn diff(&self, after: &Snapshot) -> Vec<Change> {
        let mut changes = Vec::new();
        for (path, meta) in &after.0 {
            match self.0.get(path) {
                None => changes.push(Change::Added(path.clone())),
                Some(before) if before != meta => changes.push(Change::Modified(path.clone())),
                _ => {}
            }
        }
        for path in self.0.keys() {
            if !after.0.contains_key(path) {
                changes.push(Change::Removed(path.clone()));
            }
        }
        changes
    }
}

fn walk(dir: &Path, map: &mut BTreeMap<PathBuf, (SystemTime, u64)>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, map);
        } else if let Ok(meta) = entry.metadata() {
            map.insert(path, (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let start = (start..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
    format!("[... earlier output truncated ...]\n{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::fake_workspace;

    fn dev_ws() -> (tempfile::TempDir, Workspace) {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        std::fs::create_dir_all(d.path().join(r".claude\skills\yardstick-recipes")).unwrap();
        std::fs::write(d.path().join(r".claude\skills\yardstick-recipes\SKILL.md"), "# skill").unwrap();
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        (d, ws)
    }

    #[test]
    fn prompt_carries_guardrails_and_task() {
        let (_d, ws) = dev_ws();
        let task = AgentTask::Create { app: "Obsidian".into(), id_hint: "obsidian".into(), vendor_url: "https://obsidian.md".into(), notes: String::new() };
        let p = full_prompt(&ws, &[PathBuf::from(r"C:\Intune\Yardstick")], &task, AgentOptions::default());
        assert!(p.contains("SKILL.md"));
        assert!(p.contains(r"C:\Intune\Yardstick"));
        assert!(p.contains("-WhatIf -NoElevate -DiscardInstaller"));
        assert!(p.contains("Never run `Yardstick.ps1`"));
        assert!(p.contains(r"Recipes\Development\obsidian.yaml"));
        assert!(p.contains("STUDIO-RECIPES"));
        assert!(p.contains("LAST RESORT") && p.contains("authoritative internet download"));
        assert!(p.contains("do not run live tests yourself"));
        assert!(p.contains("STUDIO-BLOCKED"));
    }

    #[test]
    fn shared_media_does_not_become_the_vendor_start_page() {
        assert_eq!(vendor_start_url("https://team.sharepoint.com/software", &[
            "https://www.dropbox.com/s/installer".into(), "https://vendor.example/download".into()
        ]), "https://vendor.example/download");
        assert!(vendor_start_url(r"\\server\softwaredropbox", &[]).is_empty());
        assert_eq!(vendor_start_url("https://xbox.com/download", &[]), "https://xbox.com/download");
        assert_eq!(vendor_start_url("https://vendor.example/?ref=dropbox.com", &[]), "https://vendor.example/?ref=dropbox.com");
        assert_eq!(blocked_reason("**STUDIO-BLOCKED: MSI 1618**"), Some("MSI 1618".into()));
    }

    #[test]
    fn download_approval_is_explicit_in_the_prompt_and_the_flags() {
        let (_d, ws) = dev_ws();
        let task = AgentTask::Fix { id: "brave".into(), evidence: String::new(), notes: String::new() };
        let prod = [PathBuf::from(r"C:\Intune\Yardstick")];

        let approved = full_prompt(&ws, &prod, &task, AgentOptions { downloads: true, multi_agent: false });
        assert!(approved.contains("Approvals the user gave in the Studio before this run started"));
        assert!(approved.contains("do not report a blocker for lack of approval"));
        assert!(approved.contains(r"Artifacts\AgentDownloads"));
        assert!(approved.contains("Still not approved: running an installer") && approved.contains(r"`C:\Intune\Yardstick`"));
        let args = copilot_args(&approved, "C:\\ws", true, AgentOptions { downloads: true, multi_agent: false });
        assert!(args.contains(&"--allow-all-paths".to_string()));
        assert!(args.contains(&"--deny-tool=shell(git push)".to_string()));

        let plain = full_prompt(&ws, &prod, &task, AgentOptions::default());
        assert!(plain.contains("has not approved downloading") && plain.contains("STUDIO-NEEDS-APPROVAL: downloads"));
        assert!(!plain.contains("Approvals the user gave"));
        let args = copilot_args(&plain, "C:\\ws", false, AgentOptions::default());
        assert!(!args.contains(&"--allow-all-paths".to_string()));
        assert!(args.contains(&"--deny-tool=write".to_string()));
    }

    #[test]
    fn multi_agent_runs_a_coordinator() {
        let (_d, ws) = dev_ws();
        let task = AgentTask::Create { app: "Obsidian".into(), id_hint: String::new(), vendor_url: String::new(), notes: String::new() };
        let multi = AgentOptions { downloads: false, multi_agent: true };
        let p = full_prompt(&ws, &[], &task, multi);
        assert!(p.contains("work as the coordinator") && p.contains("hand each to a subagent"));
        assert!(copilot_args(&p, "C:\\ws", true, multi).contains(&"--fleet".to_string()));
        assert!(!full_prompt(&ws, &[], &task, AgentOptions::default()).contains("coordinator"));
        assert!(!copilot_args(&p, "C:\\ws", true, AgentOptions::default()).contains(&"--fleet".to_string()));
        assert_eq!((task.kind(), task.recipe_id().as_deref()), ("create", Some("obsidian")));
        assert_eq!(AgentTask::Freeform { prompt: "x".into() }.recipe_id(), None);
    }

    #[test]
    fn recognises_runs_that_needed_approval() {
        assert!(needs_approval("report...\n**STUDIO-NEEDS-APPROVAL: downloads**\n"));
        assert!(needs_approval("Permission denied because no interactive user response was available. Retry..."));
        assert!(!needs_approval("STUDIO-RECIPES: brave\n"));
    }

    #[test]
    fn review_is_read_only() {
        assert!(!AgentTask::Review { id: "x".into() }.writes());
        assert!(task_prompt(&AgentTask::Review { id: "x".into() }).contains("Do NOT modify"));
    }

    #[test]
    fn extract_prompt_names_files_and_schema() {
        let p = extract_prompt(Path::new(r"C:\b\1"));
        assert!(p.contains("digest.md") && p.contains("recipes.md"));
        assert!(p.contains(r"C:\b\1\agent-items.json"));
        assert!(p.contains("\"existingRecipe\"") && p.contains("not-software"));
    }

    #[test]
    fn production_workspace_refused() {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[d.path().to_path_buf()], &[]).unwrap();
        let err = build_command(&ws, AgentBackend::CopilotCli, "", &[], &AgentTask::Review { id: "x".into() }, AgentOptions::default()).unwrap_err();
        assert!(err.to_string().contains("production"));
    }

    #[test]
    fn parses_reported_recipes() {
        let out = "blah\nSTUDIO-RECIPES: obsidian, `brave`, Bad Id\n";
        assert_eq!(reported_recipes(out), vec!["obsidian", "brave"]);
        assert!(reported_recipes("nothing").is_empty());
    }

    #[test]
    fn snapshot_diff() {
        let (_d, ws) = dev_ws();
        let before = Snapshot::take(&ws);
        std::fs::write(ws.development_recipe_path("new"), "id: new\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(ws.development_recipe_path("brave"), "id: brave\nchanged: true\n").unwrap();
        std::fs::remove_file(ws.root.join(r"Recipes\7zip.yaml")).unwrap();
        let changes = before.diff(&Snapshot::take(&ws));
        assert!(changes.contains(&Change::Added(ws.development_recipe_path("new"))));
        assert!(changes.contains(&Change::Modified(ws.development_recipe_path("brave"))));
        assert!(changes.iter().any(|c| matches!(c, Change::Removed(_))));
    }

    #[test]
    fn parses_copilot_model_list() {
        let help = "  `logLevel`: ...\n  `model`: AI model to use for Copilot CLI; can be changed with /model.\n    - \"claude-sonnet-5.5\"\n    - \"gpt-5.4\"\n\n  `contextTier`: tier\n    - \"default\"\n";
        assert_eq!(parse_copilot_models(help), vec!["claude-sonnet-5.5", "gpt-5.4"]);
        assert!(parse_copilot_models("no models here").is_empty());
    }

    #[test]
    #[ignore = "runs the installed Copilot CLI"]
    fn lists_installed_copilot_models() {
        let models = AgentBackend::CopilotCli.models().unwrap();
        assert!(models.len() > 3, "{models:?}");
    }

    #[test]
    fn evidence_truncated_from_the_front() {
        let s = "a".repeat(100) + "END";
        let t = truncate(&s, 10);
        assert!(t.ends_with("END") && t.contains("truncated"));
    }
}
