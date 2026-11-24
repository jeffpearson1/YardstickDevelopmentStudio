# Yardstick Development Studio: design

## Goals

1. Make recipe authoring approachable. A guided path takes you from an idea to a validated
   recipe without memorising `fields.md` or the harness scripts.
2. Treat the agent as a first-class author. The repo's `yardstick-recipes` skill can create,
   fix, review and update recipes, and its output feeds straight back into the editor and
   test bench.
3. Make it impossible to damage production by accident. Yardstick deploys to a real Intune
   tenant, so the Studio is structurally unable to write to, test in, or run agents in a
   production folder. The one way the Studio ever reaches a tenant is the optional, off-by-default
   *tenant validation* (see *Safety model*), which runs only from a development clone the user has
   connected and trusted.
4. Be a single self-contained native executable, written in Rust.

## Architecture

```
yardstick-studio.exe
├─ crates/ystudio        GUI (eframe/egui 0.37; wgpu, or a CPU renderer without a GPU)
│   ├─ main.rs           renderer selection, window icon
│   ├─ soft/             winit + softbuffer runner and parallel damage-tracking rasterizer
│   ├─ app.rs            StudioApp: state, job registry, caches, navigation shell, minimum layout size
│   ├─ plugins.rs        the session's plugins: registry, sidebar tab order, page state, work-item locks
│   ├─ highlight.rs      YAML/PowerShell syntax highlighting (egui LayoutJob)
│   ├─ theme.rs          Fluent tokens (light/dark), brand art, fonts, nav items, badges, cards, info bars, accent buttons
│   └─ views/            home (launcher), recipes (+ recipe header and tabs in mod.rs), designer, editor,
│                        testbench, agent, batch (Recipe Builder), icons, runs (History), docs (Help), promote (Publish), settings,
│                        setup (first-run wizard), update, plugins (Settings card, trust prompt), plugin_page (a plugin's tab)
└─ crates/ystudio-core   headless engine; no GUI types; unit tested
    ├─ workspace.rs      open + classify a Yardstick folder, layout from Yardstick.Project.psd1, recipe metadata
    ├─ recipe.rs         ordered YAML document model (key order kept, scripts as literal blocks) + templates
    ├─ fields.rs         field catalogue mirroring Test-RecipeSchema's known fields/enums + skill guidance
    ├─ lint.rs           instant Rust-side checks mirroring the skill's rules
    ├─ harness.rs        builds pwsh invocations of Tests/RecipeHarness/*.ps1 and the shared-content sync
    ├─ promote.rs        publishing by pull request: branch in a temporary worktree, push, gh pr create or compare URL
    ├─ validation.rs     ledger of recipe hashes that passed a live install test ("complete")
    ├─ setup.rs          Install-Yardstick -Status / -DevelopmentOnly -NonInteractive commands and their JSON results
    ├─ tenant.rs         optional tenant validation: the gate every tenant action passes (`ensure_ready`), the
    │                    read-only connection preflight (embedded PowerShell, GET-only, run with `-File` and a JSON
    │                    input), and the command that connects a clone to Intune
    ├─ tenant/run.rs     the run (one Yardstick command, temporary override staged and restored), removal of the
    │                    one test app a run recorded, and the informational `tenant-validated.json` ledger
    ├─ git.rs            git clone/fetch/pull (ff-only) and ahead/behind status; finds git after a PATH refresh
    ├─ update.rs         self-update: latest GitHub release, SHA-256-verified MSI download, install-and-relaunch helper
    ├─ plugin.rs         plugins: plugin.yaml manifest + validation, zip install/update/uninstall with integrity
    │                    records, work items, hash-bound approvals, step runs (pwsh -File with input/result JSON)
    ├─ results.rs        parses _install-tests/<ts>/summary.json into verdicts and phases
    ├─ agent.rs          prompt (guardrails + task), CLI command and model list for Copilot CLI / Claude Code / Codex
    ├─ intake.rs         request-list readers, quick-scan extraction, recipe matching, Batch persistence
    ├─ icons.rs          multi-source icon search, scoring, dedupe, image clean-up
    ├─ process.rs        child processes with streamed stdout/stderr, cancel (taskkill /T), PATH reload from the registry
    └─ settings.rs       %APPDATA%\YardstickStudio\settings.json
```

Core command builders enforce writable-workspace and explicit-consent requirements.
The GUI owns consent scope, sequential workflows and retry limits, with tests for those
boundaries as well as the core rules.

### Jobs

Every external action runs as a `RunningProcess`: harness steps, agent runs, git clone,
fetch and pull, the shared icon/override sync, and publishing. Reader threads pump stdout and stderr into a channel and wake the egui context.
Git writes progress to stderr, so its specs set `stderr_is_progress` and the console shows
those lines dimmed instead of red.
The UI calls `poll()` each frame. `poll()` returns *whether anything changed*; use
`is_running()` to find out whether the process is still alive. Cancelling kills the whole
process tree with `taskkill /T /F`. A live test running in the elevated worker is a remote
`RunningProcess`: its lines and exit arrive over the worker's pipe, and cancelling it asks
the worker to kill the tree (see *One UAC prompt per run* below). When a job finishes, `StudioApp::on_job_finished`
dispatches on `JobKind`:

- harness jobs reload results and verdicts;
- agent jobs reload the recipe library and open the recipes named in the agent's
  `STUDIO-RECIPES:` line;
- live harness runs that validate a recipe record its hash in the validation ledger;
- publishing runs on a background thread that streams git's output into a remote
  `RunningProcess`; its outcomes become "open in the browser?" prompts;
- batch extraction jobs load the agent's `agent-items.json` into the batch;
- agent jobs started from a batch item record changed recipes, then run that item's test-and-repair
  pipeline before starting the next queued item;
- workflow harness jobs verify fresh matching evidence, advance the stage, or send a recipe failure
  to a repair agent; repair jobs restart the checks from lint;
- plugin step jobs read their `result.json` into the work item's record (or into the catalog
  page); plugin agent jobs record what the agent changed, what stopped it and whether it wrote
  the files the task expects. Both release the work item's lock;
- tenant jobs (`TenantCheck`, `TenantRun`, `TenantRemove`) block every other run through `workflow::busy`. A
  finished run puts the staged overrides back, records the result in the tenant ledger, and, when the user chose
  *Remove it after the run*, opens the removal dialog or starts the removal; a finished removal marks the ledger;
- git fetch/pull jobs refresh the ahead/behind status (a pull also rescans recipes and reloads an
  unedited open recipe); shared-sync jobs rescan icons and re-lint.
- git switch jobs (from the footer's branch menu) do the same as a pull; git push jobs refresh
  the status.

**Branches.** The status bar shows the development workspace's branch and ahead/behind (`⑂ main
↓2 ↑1`, or *not published*). Clicking it opens a menu, like VS Code's, that searches local
branches and remote-only ones, switches (`git switch`, or `switch --track` for a remote branch),
creates (`switch -c` from the current commit), and fetches, pulls (`--ff-only`) and pushes
(`push -u origin <branch>` for an unpublished branch). Switching is refused while any job runs or
the open recipe has unsaved edits, never forces, and warns about uncommitted tracked files;
`Recipes\Development` is git-ignored, so work there stays put. Branch names are validated before
they reach git, so a name can never be read as an option.

**Status bar.** Besides the branch, the status bar carries what no page needs to repeat: the
workspace's kind and path (its menu opens another folder or a recent one, reloads, shows the
folder in Explorer, runs the shared-content sync, and trusts a full install as development),
and, away from the recipe tabs, the open recipe (amber with `●` when it has unsaved edits),
which returns to it. Home therefore shows only what blocks work (production, not set up, no
agent skill) above its three actions and a recipe search.

Plugin work-item stages are written as `Running` before their external process starts. If that
state cannot be saved, nothing runs. When a plugin work root is first opened in a new Studio
session, a persisted `Running` stage without a matching live job is reconciled to `Interrupted`;
completed stages and output files remain intact, and the interrupted stage becomes retryable.
Clearing a stale work-item lock performs the same recovery. Completion-state write failures are
reported instead of being silently discarded.

When a writable workspace opens, and on the library's *Refresh*, the Studio runs `git fetch`
and Yardstick's `Sync-RecipeHarnessSharedContent` (only when `syncSharedContent: true`), each
switchable in Settings. Production folders are never fetched or synced.

Self-update runs outside the job system. Background threads check for and download the
release, and the result is verified before anything is installed. To install, the Studio starts a hidden
Windows PowerShell helper (`update::installer_helper`) and exits. The helper waits for
the Studio's PID to exit, runs `msiexec /i … /passive` elevated (`-Verb RunAs`), and starts
the Studio again with `--updated <exit code> <log>`, which the new process reports in the
status bar.

Only one agent process runs at a time, whether it came from the Agent view, a batch, or an extraction,
except in Multi-agent mode, where Recipe Builder runs several authoring and repair agents at once
(see *Test and repair* below). Harness runs are always one at a time.

Claude Code runs with `--output-format stream-json --verbose`. `agent_stream::claude_lines`
renders each event as the pump reads it: assistant text as output, thinking as `Stream::Thinking`
(set in from an accent rule), tool calls as one-line `Stream::Tool` summaries, failed tool results,
and a closing line with turns, time and cost. Narration can be hidden per log (*Thinking & tool
calls*) and is left out of `RunningProcess::report_text`, which is what `STUDIO-RECIPES:`,
approval and failure summaries are read from.

The Agent tab keeps a draft per recipe (`AgentState::focus`), shows the open recipe's own latest
run (with a link when the agent is busy on another recipe), clears the prompt once a run starts,
and keeps the last 50 started prompts in `Settings::agent_prompt_history` (*History*: this
recipe's first, then others').

Job logs are virtualised (`show_rows`), so egui's label selection cannot span rows that have
scrolled away. `app::job_log` therefore selects whole lines by index: click, drag (scrolling past
the edges) or Shift+click, then Ctrl+C, *Copy N lines* or the context menu. Logs only grow, so
the indices stay valid while output arrives. Read-only text boxes use a `&str` buffer rather
than `interactive(false)`, so their text can still be selected and copied.

Live validation requires all eight validation phases to pass (warnings are allowed).
The result reader recognizes the harness's `PostInstallDetect` and
`PostUninstallDetect` names as well as the older `PostDetect` and `PostUninstall`
names. A run with `-SkipUninstall` remains incomplete even if its overall status
is `Pass`; it has not tested uninstall, post-uninstall detection or residue.

### Batch mode (`intake.rs`)

1. **Read.** The file is converted into `Record`s (title, description, ticket id, requester, and the remaining fields). The format is detected from the extension and the content: ServiceNow exports (`records`/`result` arrays, `number`/`short_description` keys), generic JSON/JSONL/YAML arrays, CSV/TSV with delimiter sniffing, spreadsheets via `calamine`, `docx` paragraphs, HTML with tags stripped, and plain text or email split into blocks. Header guessing handles columns such as "Short description", "Software" and "Product".
2. **Quick scan.** Each record is split into software items: one per product line, bullet, or "X and Y" list. The scan strips request boilerplate ("Intune - Package", "please install", version numbers, licence noise), picks up versions and vendor URLs, and flags records that aren't software (meetings, group or device administration).
3. **Agent extraction (optional).** The batch folder receives `digest.md` (the condensed records), `recipes.md` (the existing recipe ids and names), and `plugins.md` (installed work-item plugins and their declared purpose). The agent writes `agent-items.json`, which replaces the quick-scan items. Each item names Recipe Builder or an installed plugin as its handler; unknown handler ids are rejected back to Recipe Builder. The extract prompt allows writes only inside the batch folder.
4. **Match.** Item names are normalised and compared with recipe ids and display names (`RecipeMeta`), to tell an *Update* or *Already packaged* item from a *New* one. The match also records whether the recipe's `iconFile` exists.
5. **Run.** Selected items are queued. Each one becomes an `AgentTask::Create` or `AgentTask::Update` with the ticket context; if an icon was already saved, the agent is told to use it. Status, changed recipes and the log tail are stored on the item. The batch (`batch.json`) is saved after every change, and Running items are reset to Pending when a batch is loaded again.
   Items routed to a work-item plugin are excluded from recipe authoring and test counts. Sending one creates the plugin item with its ticket and extracted context and marks the source **Delegated**, not done. Plugin automation rolls running, attention, approval and complete states back into that source item. Both surfaces retain exact contextual navigation instead of creating a second request or returning to an unselected list.
6. **Test and repair (`workflow.rs`).** Studio runs Lint, Schema, Download, Package,
   DryRun and optionally Live for each authored Development recipe. A recipe failure is
   returned to an `AgentTask::Fix`, then all checks restart. The default limit is five
   test attempts (the first check plus at most four repairs); environmental blockers,
   cancellation, missing recipes and missing/skipped evidence stop automatic repair.
   The agent itself is still forbidden from running live installs.

   The run is a **test coordinator**: test requests queue in arrival order and the harness
   takes one app at a time, because Windows runs one installation at a time. A recipe that
   fails leaves the harness for a repair agent. Normally the harness waits and retests it
   next. In **Multi-agent mode** (`workflow::parallel_agents`: the setting, a backend with
   multi-agent support, and *N apps at once*, default 3, at most 8) Recipe Builder researches
   up to N apps at once: `batch::start_next` fills every free agent slot, each finished authoring
   agent hands its recipe to the coordinator (`workflow::enqueue`) and its slot takes the next
   app, and repairs share the slots (a repair waits for a free one). The harness moves on to
   the next queued app while a recipe is repaired, and the repaired recipe rejoins the back of
   the queue. These agents run without the per-agent coordinator prompt, so they do not each
   start subagents. With several agents writing at once, a finished item takes recipes from
   the workspace diff only when they are its own; the others come from its `STUDIO-RECIPES:`
   line. The progress line lists what is testing, waiting, being repaired and researching.
7. **Retest a batch.** `Test selected batch` gathers every included software item's
   recipe ids, deduplicates them while preserving all ticket associations, and runs
   checks sequentially without editing recipes. Failure stops that recipe, not the
   remaining batch. Each item's `checks` records the outcome, timestamp and harness
   evidence directory and is included in the copied report. Starting a retest invalidates
   earlier outcomes so interrupted checks cannot appear successful.
8. **Repair with agent.** Picks the included items whose recipe exists but has no check
   confirming it for that item. With live checks on, only a passed live test confirms
   (`BatchCheck::live`, or a "Validated" note in batches saved before the flag); with
   them off, passed safe checks do. Items without a recipe are left to *Run agent*.
   Recipes that exist only as shipped recipes are first copied into
   `Recipes\Development` under the same id (`Workspace::copy_to_development`); the
   shipped file is never edited. The selection then runs the same test-and-repair
   pipeline as authoring: each recipe is tested first, so one that now passes is not
   touched, and failures go to `AgentTask::Fix` within the attempt limit.

An item's Done/Failed status comes from the checks of the recipes it currently names; checks
of a recipe a repair replaced are dropped. A live install test that validates a recipe from any
view (`StudioApp::record_validation`) records a passing check on the open working list's
finished items that use it, and an item in another list that still reads Failed shows as
complete once all its recipes are.

Status messages and batch notes carry a one-line summary of a failure
(`RecipeResult::problems`, the lint errors, or the last error line of the output),
capped at 400 characters. The full output and harness evidence still go to the repair
agent.

Live automation requires an explicit dialog listing the applications/recipe ids before
starting. Consent is held only for that run or authoring queue, including bounded repair
retries; selecting the live option is not consent. Automation never uses `-Force` or
`-SkipUninstall` and never promotes or deploys recipes. Existing software can therefore
block full validation without authorizing its removal. Workspace switching and concurrent
agent/test runs are blocked while a pipeline is active.

**One UAC prompt per run (`elevation.rs`).** The consent dialog counts the listed recipes
by context: system-context (the default), user-scope (`installExperience: user`), and not
yet authored (assumed system-context). If any of them may need elevation, *Start* asks for
it once, right away, while the user is at the machine. `Session::launch` runs this same
executable through `ShellExecuteEx` "runas" as `yardstick-studio --elevated-worker <pipe>
<studio pid>`, hidden, so the UAC prompt names Yardstick Development Studio. Safe checks
continue while the prompt is open; a live step that reaches it first waits for the answer.
Each live test is then routed by `harness::runs_elevated`:

- system-context recipes run in the worker, which calls the harness with `-ElevatedChild`.
  The harness is already elevated there, so it does not relaunch itself, and it skips the
  shared-content sync that the unelevated steps already ran;
- user-scope recipes run unelevated as before, with `-NoElevate`, so the harness can never
  raise its own prompt.

The worker's output streams into the normal job console through a remote `RunningProcess`
(`RunningProcess::remote`, fed by a `Feed`). Cancelling a remote job sends the worker a
cancel request, because an unelevated Studio cannot kill an elevated process tree. The
session ends when nothing that could need it is running (no workflow run, request, or
queued batch item), when the user chooses *End elevated session*, or when the Studio exits.
If UAC is declined, or the worker dies, system-context recipes finish as *Blocked* with no
repair attempt, user-scope recipes continue, and *Retry elevation* asks again. A status-bar
badge and a line above the run's progress show the session state; batch items show which
step is running and, once done, whether the live test ran elevated.

The worker accepts exactly two requests over its pipe: run an install test for one recipe
file, or cancel one. It reopens the workspace with the Studio's production and trusted
folder lists, accepts only `pwsh.exe`/`powershell.exe`, refuses `-Force`, and rebuilds the
command itself through `harness::build`, so the core's production and path refusals still
apply. It never runs a command line it is sent. Two one-way pipes are used because
synchronous reads and writes on a single handle serialise. Both pipes allow one instance
(`FILE_FLAG_FIRST_PIPE_INSTANCE`) and local clients only. The Studio checks the client's
PID against the process it launched. The worker checks the server's PID against its
`<studio pid>` argument, checks that this process is the same executable, and connects with
identification-level QoS so the Studio cannot impersonate the elevated token. Closing the
Studio's end stops the worker and kills any test it is running. Known trade-off: for the
length of the run, recipes the repair agent edits are installed elevated without another
UAC review. The consent dialog says so.

Both extraction and authoring prompts prefer authoritative vendor internet downloads.
Shared-media links are preserved as fallback context, not used as the vendor start page.
`manualDownload: true` is appropriate only when legitimate unattended vendor downloads are
unavailable, with the reason and attempted sources documented by the agent.

### Icon engine (`icons.rs`)

The sidebar's **Icons** page is the library-wide finder (an open recipe's Icon tab and Recipe
Builder items use the same view in place). Beside the recipes with missing icons it compares
`Icons\` with the shared icon folder (`SharedIcons`, which production Yardstick syncs from) on a
worker thread, since that folder is usually a network share: *Push* copies an icon the share lacks,
*Replace* overwrites a different one only on request, and *Push N to the server* copies every
missing one. After a save the preview says whether the server has the icon, with the same Push
button. Pushes run on a worker thread and write a temporary file beside the target first, so
Yardstick never syncs a half-written icon.

It replaces `Get-YardstickIcon.ps1`, fixing these problems:

| Old tool | Studio |
|----------|--------|
| A single Bing image query with an "app icon" suffix, and no relevance check, so it often returned unrelated photos | Six sources searched in parallel: installer EXE/MSI resources, local image files, Wikidata/Commons logos, the Chocolatey package icon, the vendor site (web manifest, `apple-touch-icon`, `<link rel=icon>`, favicon) and image search. Each candidate is scored on name/publisher match, source trust, size, squareness and transparency. Identical images (by average hash) are merged, and agreement between sources raises the score. |
| EXE icons taken from index 0 only and stretched to 256 px; no MSI or SVG support | The largest frame of each icon group is taken from the PE resources, the MSI `Icon` table is read, and SVGs are rasterised with `resvg`. |
| Global colour-key removal punched holes in logos, and resizing left halos | Background removal flood-fills only from the edges, with soft edges unmixed. Resizing uses premultiplied Lanczos3, then trim, pad to square and margin. Output is at most 512 px and 750 KB. |
| A blocking WinForms dialog that needed an STA thread | A streaming, cancellable search on worker threads; candidates appear as they arrive, with per-source errors shown, and an offline (installers only) mode. |

### Recipes

The YAML text is the source of truth. The Designer parses it into an ordered `RecipeDoc`
and writes it back keeping the original key order. New keys are appended in catalogue
order, and scripts are written as literal blocks. YAML comments can't survive that round
trip, so the Designer warns first; the YAML Editor edits the raw text, comments included.
*Copy to Development* is a plain file copy. *Rename* (beside the id in the recipe header, and on the Design tab's `id` field) changes a development recipe's file name and `id` together (`Workspace::rename_development_recipe`), carrying unsaved edits and, optionally, an `iconFile` named after the old id. The open working list, the Agent tab's draft and the Icon tab follow the new id; a rename is refused while agents or checks run, onto an existing or shipped id, and for ids that a plugin-linked item fixes. The linter (`lint.rs`) gives instant
feedback, while the harness's `Test-RecipeSchema.ps1` remains the authoritative validator.

## Safety model

Classification (`workspace::classify`), in priority order:

| Rule | Kind |
|------|------|
| Folder is listed in *Production folders* (default `C:\Intune\Yardstick`) | **Production** |
| `Local\preferences.yaml` has `developmentOnly: true` | Development |
| Folder is listed in *Trusted development folders* | Development |
| `Local\preferences.yaml` exists without `developmentOnly` (a full Intune install) | **Production** |
| No preferences yet (fresh clone) | Unconfigured (writable; harness needs `Install-Yardstick -DevelopmentOnly`) |

Enforcement:

- **Refusals.** `Workspace::ensure_writable` is enforced inside the core builders
  (`harness::build`, `harness::install_development_only`, `agent::build_command`,
  `promote::plan` and `promote::open_pull_request`), so no external process can start in
  production even if a UI
  path forgets a check. Tests cover these refusals. Saving, creating and copying recipes
  call it in the app before touching disk.
- **Consent.** Live install tests and `-Force` need `Consent::Confirmed`, which is only
  produced by a modal with an acknowledgement checkbox. Dry runs need no consent.
- **Elevation.** Automatic runs elevate once, through a Studio-owned worker that runs only
  install tests it rebuilds itself and talks only to the Studio that launched it. It never
  outlives the run or the Studio.
- **Publishing by pull request.** The Studio never copies into a production folder. It
  uses a `studio/<id>` branch in a temporary worktree so the user's checkout is untouched,
  and opens a pull request; production pulls merged work. When the branch already has an
  open pull request, the Studio builds on that remote branch and appends a commit instead
  of replacing its history or opening a duplicate. Recipes published together share pull
  requests (`promote::group`): a recipe already in an open Studio pull request keeps building
  on it, and the rest go into new pull requests of up to *N* recipes (`publish_per_pr`, default
  10, set in the Publish dialog) on a `studio/batch-<stamp>-<n>` branch; a pull request of one
  recipe keeps the `studio/<id>` branch. A combined pull request commits each recipe with its
  evidence file and lists its recipes in a `<!-- studio-recipes: a, b -->` line, which the
  status refresh reads back so checks, reviews and merged-recipe cleanup still work per recipe. Publish monitors open Studio pull
  requests in place: check runs, review decisions, comments, and reviews from people or
  GitHub Copilot refresh in the background with loading and recovery states. Refreshing
  Publish also reconciles merged Studio pull requests: a matching
  `Recipes\Development` file is moved to `Artifacts\PublishedRecipes` only when its hash
  still matches the recipe on the fetched remote default branch; newer local edits are
  kept. Icons go to the shared icon folder, not git. Recipes that are not *complete*
  (no passing live test of their current contents) show their recorded checks first, and
  the pull request says they are not validated. Nothing opens in the browser without asking.
- **Merge gate.** Each pull request commits `Tests/RecipeEvidence/<id>.json`: the verdict,
  the harness phases, when the test ran, and the SHA-256 of the recipe as git commits it
  (BOM dropped, CRLF as LF; hashed from the staged blob, because Yardstick's `cleanSecrets`
  clean filter can rewrite a recipe as it is committed). Yardstick's required `Recipe
  evidence` check rejects a recipe whose evidence is missing, unvalidated or for other
  contents, unless a maintainer labels the pull request `evidence-waived`.
- **Tenant validation (optional, off by default).** *Run on tenant* on a recipe's Test tab runs the real
  `Yardstick.ps1` for one recipe against the user's own tenant. Every layer is enforced in core
  (`tenant::ensure_ready`, `tenant::run`), not in the UI:
  1. The clone must be a **trusted** development folder that is a full instance (`developmentOnly: false`;
     `Workspace::tenant_connected`). Production, development-only and unconfigured clones are refused, and the
     development-only setup is refused on a connected clone so it cannot switch the connection off.
  2. The user names the tenant (a GUID); the clone's stored credential and the connected organization must both
     match it (and the name, if given). A preflight also fails on `syncSharedContent: true` (the temporary override
     would sync to the share) and on a scheduled task that runs Yardstick from the clone (it would deploy every
     recipe there, Development included).
  3. A per-run consent dialog, never persisted and never part of batch or workflow automation. It names the
     tenant, the test app, the groups and what happens afterwards.
  4. One fixed command, `Yardstick.ps1 -ApplicationId <id> -NoEmail -NoDelete` (`-Force` only on request). Never
     `-All`, `-Group` or `-Repair`; asserted by a test on the script.
  5. **Isolation.** A temporary entry for the recipe in `Local\recipeOverrides.yaml` renames the app
     `YDS TEST - <name>` (Yardstick matches versions by exact display name, so it cannot reach a real app's
     versions, assignments or supersedence; the prefix has no brackets because `-like` reads them as a character
     class), forces `defaultDeploymentGroups` to the test groups (or none), and turns supersedence and
     auto-update off. The original file is backed up byte for byte, restored when the run ends, and restored at the
     next start if the Studio died mid-run; an edit made since is never overwritten. The script refuses to run
     Yardstick unless that override is in place.
  6. **Removal** is a separate script that deletes exactly one app: the id a Studio run recorded. It re-reads the
     app and refuses unless it is a Win32 app, its name still starts with `YDS TEST - `, and it was created when the
     run recorded. It never searches or deletes by name.
  7. Results go in their own ledger (`_studio\tenant-validated.json`) and the Test tab, Publish dialog and recipe
     badge. They never change `Tests/RecipeEvidence/<id>.json` or Yardstick's CI gate: validating on a tenant stays
     optional.
  Agents remain forbidden from `Yardstick.ps1`, Graph and `git push`; none of this is reachable from an agent run.
- **Agent guardrails.** Agents run in the development workspace only, and their file
  tools are confined to it (`--add-dir`, no `--allow-all-paths`). The shell tool may not
  run `Yardstick.ps1`, `Invoke-Yardstick`, `Publish-Yardstick.ps1` or `git push`. Review
  tasks also deny writes. The prompt opens with explicit rules: work only in
  `Recipes\Development`, never touch production, run only harness dry runs, and report
  the recipes changed on a `STUDIO-RECIPES:` line.
- **Codex** (`codex exec`) is confined differently: by its own sandbox rather than tool
  flags. `workspace-write` lets commands write only in the work folder and temp (`read-only`
  for review tasks), with `approval_policy="never"` so nothing waits on a prompt nobody can
  answer, and Codex's live web search for research. The Windows sandbox cannot make HTTPS
  requests from commands (Schannel has no credentials there), so downloading an installer
  works only with the sandbox off: the user's explicit *Allow the agent to download and
  inspect installers* approval switches a writing task to `danger-full-access`, and the
  checkbox says so. Codex has no per-run list of refused commands, so the rules that are
  enforced for Copilot and Claude (no `Yardstick.ps1`, Graph, `git push`, a plugin's own
  step scripts) are in the prompt only; the Studio shows a warning when Codex is selected.
  Codex has no subagents here, so Multi-agent is off, and its runs are not counted in the
  credit totals.
- **Recipe paths** are checked to lie inside the workspace. Recipe ids are validated
  before they are passed to PowerShell, and every argument is single-quote escaped
  (`ps_quote`).

## User journeys

1. **Design.** Home or Recipe Library → *New recipe* (from a `Templates\` file) or *Copy to
   Development* → the recipe's Design tab steps (Identity, Source & version, Download scripts, Install & uninstall,
   Detection, Lifecycle & assignments, Advanced) with live lint → Ctrl+S.
2. **Test.** The recipe's Test tab → *Run safe checks* (Lint → Schema → install-test dry run). Then,
   optionally: Lab download/package, and a live install test after consent. History shows
   the history and per-phase results.
3. **Agentic.** *New recipe* opens Recipe Builder's inline composer; *Write with agent* adds it
   to the working list and runs the same author, test and repair pipeline as any other item;
   or the open recipe's Agent tab (Fix with the
   latest failing run, Review, Update, Freeform) → stream → back to Test.
4. **Publish.** Publish → select complete Development recipes → *Open pull requests…* →
   confirm → open the pull request (or GitHub's compare page) in the browser.
5. **Recipe Builder.** Its persistent left-hand working list holds existing recipes, new recipe
   drafts and every software item from imported batches. Add existing recipes through the
   Library, compose one inline, or import/paste a request list; opening another list offers
   append or save/replace. The list is the page's only navigation: its first row is the list's
   overview (name and how many are left), and selecting an existing recipe keeps the list visible
   and opens its Design, YAML, Test, Icon and Agent tabs in the right pane without losing
   progress. Saving, opening, reporting and clearing the list live in its `⋯` menu, and the
   agent and test options for a run open from a one-line summary under the actions (the agent
   defaults are the ones Settings keeps). Complete all remaining recipes, a checked subset, or one recipe, and pause a
   running batch after its active recipe to inspect or repair work manually. Removing the open
   recipe closes its editor; dirty edits require Save, Discard, Keep in editor, or Cancel. Save and Load preserve the complete session. Review extracted items
   (optionally *Extract with agent*) → find missing icons → *Run agent on N selected* →
   publish the recipes it produced.

## Platform notes

- **Renderer.** With a real GPU the renderer is `wgpu`. `glow` needs OpenGL 2+, which
  VMs and RDP sessions often lack. On GPU-less machines wgpu falls back to WARP, which
  takes about 550 ms per frame. `main.rs` therefore enumerates adapters, and if every
  adapter is a CPU device it runs `soft/` instead. That path uses winit, egui-winit and
  softbuffer, plus a multithreaded strip rasterizer (`soft/raster.rs`, rayon). The
  rasterizer hashes the triangles touching each 16-row strip and redraws and presents only
  the strips that changed, which gives about 100 fps on the same VM.
  `YSTUDIO_RENDERER=software|wgpu` overrides the choice.
- **Per-frame cost.** Views never touch the disk while drawing. Icons, groups, docs and
  skill presence live in `WsCache`, refreshed with the recipe list. The parsed recipe is
  cached per edit revision, and highlighted YAML galleys are memoised in egui's
  `FrameCache`. Ctrl+Shift+P shows UI-build and paint times. `YSTUDIO_PERF_LOG=<file>`
  logs them per frame, and `YSTUDIO_PERF_CONTINUOUS=1` repaints continuously for
  benchmarking.
- **Look and feel.** The UI follows the Windows 11 (Fluent) design guidelines: a Mica-toned
  navigation pane with an accent selection pill, pages on a layer surface, 4 px corners on
  controls and 8 px on dialogs and menus, the Fluent type ramp (Caption 12, Body 14,
  Subtitle 20, Title 28), one accent button per surface (`theme::primary`), and the
  WinUI signal colours for success, caution, critical and informational states. Colours
  come from `theme::palette()`; call `theme::green()`/`red()`/`amber()`/`blue()`/`grey()`
  rather than hard-coding values, because they differ between light and dark.
- **Split panes.** Every sibling beside a resizable divider uses `theme::pane_frame()` or
  `theme::pane()`. Their 12 px horizontal inset corrects egui's asymmetric side-panel and
  center-panel defaults, keeping controls, cards and scroll content equally clear of the
  divider. Page-level margins remain 24 px and card gaps remain 16 px.
- **Branding.** The accent is Yardstick orange: `#FE700A` in dark mode and `#C15101`
  (buttons) / `#AD4901` (text) in light mode, the same hue darkened so white button text
  and orange text on light surfaces stay above 4.5:1. The artwork in `assets/branding`
  is derived from the master files (GIMP `.xcf`, kept outside the repo): `mark-*` (drone)
  and `wordmark-*` for the navigation header, `logo-*` for the README, and
  `yardstick.ico`/`icon-256.png` (the light mark on a charcoal tile, simplified to the box
  and ruler arms at 32 px and below). `-dark` files are for dark backgrounds. The
  binary embeds the PNGs (`theme::brand`), sets the window icon at runtime, and
  `crates/ystudio/build.rs` embeds the `.ico` with `winresource` for Explorer and the
  taskbar. `theme::brand::image` resamples the artwork with premultiplied alpha to the
  exact device-pixel size and caches the texture, because neither renderer builds
  mipmaps.
- **Fonts.** `theme::install_fonts` loads Segoe UI Variable twice from `%WINDIR%\Fonts`, at
  `wght` 400 for body text and 600 for the `semibold` family (`FluentText::semibold`; Fluent
  uses weight, not colour, for emphasis), falling back to static Segoe UI on Windows 10.
  Icons are Segoe Fluent Icons (or Segoe MDL2 Assets) code points written inline, such as
  `"\u{E74E}  Save"`; the icon font sits right after Segoe UI in every family. Code uses
  Cascadia Mono or Consolas. Segoe UI Symbol and Emoji remain last-resort fallbacks.
- **Theme.** In egui 0.37, `set_visuals` only changes the active theme. `theme::apply`
  installs both styles with `set_style_of` and switches with `set_theme`.
- **Toolchain.** With the `x86_64-pc-windows-gnu` host, rustc needs MinGW-w64 `dlltool`
  and `as` for raw-dylib imports. `build.ps1` locates WinLibs and passes
  `-C dlltool=...`.
- **Fresh clones.** A fresh clone lacks the git-ignored `Recipes\Development`, which the
  harness creates on demand, and the icons, which the Clone flow copies from production.
- **Text.** Windows PowerShell 5.1 writes UTF-8 with a BOM, so all reads go through
  `read_text`, which strips it.

## Plugins

A plugin adds a tab after Icons in the sidebar (Home, Recipe Library, Recipe Builder,
Publish, Icons, *plugins*, History, Settings, Help). It is a zip with a `plugin.yaml` manifest,
PowerShell step scripts and agent instructions, installed to
`%APPDATA%\YardstickStudio\plugins\<id>\`. There is no native code: a page is drawn by one of
the Studio's built-in page types, so a plugin can change without a Studio release, and a
new page type needs one (`studio.hostApi` in the manifest says which kit it expects).

- **`workitems` pages** list folders under the plugin's work root. Each has a pipeline of
  steps and agent tasks in order (`after`), with state, risk and gate, and read-only panels
  (a JSON table, markdown, a file list) bound to files in the folder.
- **`catalog` pages** probe a connection, search an outside system (a step whose `rows`
  fill a table) and offer per-row actions: run a step, start a work item in another plugin
  (`host.plugin-workitem`, which copies files into its `intake` folder), or pre-fill the New
  Recipe Builder form (`host.recipe-builder`). The old `handoff.*` names remain parse aliases.
- **Host actions** keep Studio-owned controls out of plugin implementations. `page.hostActions`
  may declare `new-work`, `import`, `refresh`, `work-folder` and `settings`; the host validates
  that the page can support each one and renders them in the standard chrome. `settings`
  always opens the plugin's card under global Settings rather than embedding a second settings
  form. Host API 1-2 manifests with no list retain their inferred legacy actions.
- **Steps** run as `pwsh -File <script> -StudioInput <input.json> -StudioResult <result.json>`.
  Only those paths are on the command line; settings, parameters, the Studio's version and
  workspace, and any approval go in `input.json`, and the script reports `status`, `summary`,
  `outputs`, `artifacts` and `rows` in `result.json`. A non-zero exit, or a missing or invalid
  result, is a failure.
- **Agent tasks** use the same Copilot CLI / Claude Code / Codex runners as recipes
  (`agent::build_plugin_command`). The prompt carries the Studio's rules, the plugin's
  `INSTRUCTIONS.md`, the task prompt and the results of the steps it takes evidence from; the
  plugin's reference files are copied to `<work item>\.studio\agent`.
- **Safe pipeline runs** are explicitly opt-in: a task or step declares `automatic: true`.
  The host walks opted-in stages in manifest order, honors `after`, and starts at most one
  external process at a time. It never crosses a confirmation/hash gate, local install, or
  tenant write without returning control to the user. Failure, interruption and an unmet
  manual prerequisite pause the run. The work item's automation state and note are saved
  beside its stage records, so a successful approved stage can continue the run and a
  restarted Studio can recover it without replaying completed work.
- **Per-row Recipe Builder routing** is a table-panel capability. A panel declares
  `recipeBuilder.routedRows` and `reviewBefore`; the host pauses an automatic run at
  that boundary when the active array contains several rows. Moving a row atomically
  removes it from the plugin's active JSON array, retains the exact row in the declared
  routed array, and creates a separate Recipe Builder item with its product fields and
  original ticket context. Rows can be returned until the downstream plugin stage starts.
  The confirmed review is stored on the work item. An empty active array completes the
  plugin run without invoking steps that no longer have products to process.
- **Already-done rows** (host API 4). A panel may also declare `recipeBuilder.completedRows`,
  a third array its own steps fill with rows that need no work (One-Off Packager: an existing
  Yardstick recipe or Intune app). The host lists them under *Already available* and always
  pauses for review when any exist, even for one product. *Package anyway* moves a row back
  and marks it `completedOverride: true`, which the plugin's checks respect. Any row may carry
  `flags: [{label, tone, detail}]`, shown as coloured badges (tone green, amber, red, blue
  or grey) with the detail beneath.
- **Linked Recipe Builder authoring** reuses the same panel declaration with `authorBefore`,
  `recipeFolder` and `iconFolder`. At that boundary the host creates or reuses one Recipe
  Builder item per active row and records the origin in both the batch item and plugin work
  item. The plugin pipeline waits while authoring or checks run and stops with a direct link
  when they need attention. A matching passed safe check atomically copies the exact
  development recipe and referenced icon into the confined plugin work item, marks that
  product ready, and resumes safe automation. Destination and recipe id are fixed in the
  linked item so an edit cannot silently redirect synchronization.
- **Cross-workflow lifecycle** keeps ownership explicit. A Recipe Builder handoff is
  `Delegated` until its plugin automation reaches `Complete`; stage progress and recovery
  notes are persisted back into the originating batch even when that list is not open.
  Plugin work created by Recipe Builder carries its batch id and item index, with a path
  fallback if rows move. The plugin header returns to that exact item, Recipe Builder opens
  the exact plugin folder, and each routed product opens its own Recipe Builder row. Generic
  “continue” and list-level navigation are omitted where a durable relationship exists.
- **Cross-plugin activity** makes those durable states discoverable outside each plugin tab.
  A three-second host cache scans resolved work roots, reconciles orphaned `Running` records
  before presenting them, and classifies work as running, waiting for the user, needing
  attention, ready to continue or complete. Home shows the most urgent active work; History's
  Plugin work view filters the complete collection and opens the exact plugin item. Disabled,
  unconfigured and non-work-item plugins do not contribute misleading entries.

Distribution. Plugin sources live in `plugins\<id>\`; `build.ps1 -Task plugins` runs
`plugins\Build-Plugins.ps1`, which packs each into `target\plugins\<id>-<version>.zip` plus a
`.sha256`: files in name order, fixed timestamps, LF line endings in text files, no `tests\`.
A release is tagged `plugin-<id>-v<version>` with the zip attached and created with
`--latest=false` (the Studio's own update check reads the latest release and needs an MSI there).
*Settings → Plugins* lists the newest release of each plugin in this repository's releases
(`plugin::parse_catalog`), flags installed plugins that have a newer one, and downloads with
`update::download_verified`: the zip's SHA-256 must match GitHub's `digest`, and its manifest must
name the plugin and version the release does. A plugin may name another repository in its
`source`; it is checked too. Both checks run at startup (quietly, when *Check for updates at
startup* is on) and on demand. A core test packs the sample plugin with the script and installs the
result, so the packer and the Studio's validator cannot drift apart.

Shipped plugins. **One-Off Packager** is a `workitems` plugin: its agent owns ticket triage,
Recipe Builder owns recipe research/authoring/safe repair, and the plugin pipeline owns the
isolated runtime, lifecycle test, package and Intune change. The ticket folder and stages are
declared in `plugin.yaml`; its scripts share `StudioPlugin.psm1` (the input/result protocol and
approval check) and `OneOffPackager.psm1` (inventory, packaging, change set and the resumable live
change, with every tenant, group and path coming in as a setting or from the work item).
**SCCM App Migration** is a `catalog` plugin: a connection check, a search whose rows fill the table,
and per-row steps (export, copy content) and host integrations (`host.plugin-workitem` to the One-Off Packager,
`host.recipe-builder` into Recipe Builder's new-recipe form). A handoff's `files` may end in `?` for "if present", and
folders are hard-linked where possible. The SCCM scripts only ever `SELECT`: a test fails the build if
one calls a cmdlet that could change the site. Each plugin is a self-contained package, so
`StudioPlugin.psm1` is duplicated, and a test checks the copies are identical.

Plugin safety, in addition to the model above:

- **Trust.** Installing or updating shows the publisher, the package SHA-256 and every
  step with its risk (reads/writes local files, installs software, reads or changes the
  tenant, reads Configuration Manager) and gate. An update marks new or changed steps.
- **Integrity.** Install records the SHA-256 of every file. A plugin whose files were
  changed, added or removed afterwards is shown as modified and its steps and agent tasks
  refuse to run until it is reinstalled. Zips are read in memory with limits (2,000 entries,
  50 MB), no links, no paths outside the folder, and only script, text and image files.
- **Approvals.** A step that installs software or changes the tenant must name a gate file.
  The user approves the SHA-256 of exactly that file, shown with the plugin's summary of it.
  The grant is written to `approvals\`, is single use, and is checked again when the step
  starts: if the file changed, the step refuses. The script checks it a second time.
- **Confinement.** A step or agent runs only in a work item under the plugin's work root,
  never in or around a production folder, and one at a time per work item (a `.studio-lock`
  file, because the work root may be a share). Steps that install software and agent runs
  wait for each other and for recipe agents and install tests (`workflow::busy`).
- **Agents** write only in their work item (`--add-dir`), may not run the Studio's denied
  commands, the plugin's `deniedCommands` or any of the plugin's own step scripts, and are
  told never to use Graph or credentials and to treat the request text as untrusted. This is
  a prompt and a prefix deny list, not a sandbox: a shell can still be asked for anything,
  so tenant-writing steps rely on the user's approval, not on the agent's restraint. The
  agent's changes are listed from a snapshot that ignores the Studio's own files.
- **Private plugin clones.** A plugin folder setting may offer a Studio-managed repository clone. One-Off Packager uses this for `yardstickDevPath`; with a real private clone selected, its file-only agent tasks may run beside Recipe Builder agents and checks because the two workflows cannot edit the same checkout. Local-install and tenant-writing steps keep their existing serialization and approval gates.
- No elevated worker for plugins: a plugin that installs software does it in the user's
  session and asks the user's consent.

## Window size

The layout is designed for at least 1100 × 680 (`app::MIN_LAYOUT`). A smaller window keeps
that size and scrolls over it, with solid scrollbars, instead of squeezing pages into too
little room. The window itself can still be shrunk to 900 × 600 so it never exceeds a small
screen at high display scaling.

## Testing

- `.\build.ps1 -Task test` runs the unit tests: classification, refusals, lossless edits,
  lint rules, command construction, summary parsing and highlighting.
- `crates/ystudio-core/tests/fixtures/plugins/sample` is a small plugin the tests install and
  run; GUI tests render its pages headlessly (set `YSTUDIO_TEST_SCREENSHOT_DIR` to keep
  PNGs), including below the minimum window size.
- `crates/ystudio-core/tests/live_harness.rs` is ignored by default. It runs the real
  Schema check and an install-test dry run against a throwaway clone, and checks that the
  production folder is refused.
