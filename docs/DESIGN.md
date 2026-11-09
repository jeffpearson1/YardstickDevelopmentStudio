# Yardstick Development Studio: design

## Goals

1. Make recipe authoring approachable. A guided path takes you from an idea to a validated
   recipe without memorising `fields.md` or the harness scripts.
2. Treat the agent as a first-class author. The repo's `yardstick-recipes` skill can create,
   fix, review and update recipes, and its output feeds straight back into the editor and
   test bench.
3. Make it impossible to damage production by accident. Yardstick deploys to a real Intune
   tenant, so the Studio is structurally unable to write to, test in, or run agents in a
   production folder.
4. Be a single self-contained native executable, written in Rust.

## Architecture

```
yardstick-studio.exe
├─ crates/ystudio        GUI (eframe/egui 0.37; wgpu, or a CPU renderer without a GPU)
│   ├─ main.rs           renderer selection, window icon
│   ├─ soft/             winit + softbuffer runner and parallel damage-tracking rasterizer
│   ├─ app.rs            StudioApp: state, job registry, caches, navigation shell
│   ├─ highlight.rs      YAML/PowerShell syntax highlighting (egui LayoutJob)
│   ├─ theme.rs          Fluent tokens (light/dark), brand art, fonts, nav items, badges, cards, info bars, accent buttons
│   └─ views/            home (launcher), recipes (+ recipe header and tabs in mod.rs), designer, editor,
│                        testbench, agent, batch (Requests), icons, runs, docs (Help), promote (Publish), settings, update
└─ crates/ystudio-core   headless engine; no GUI types; unit tested
    ├─ workspace.rs      open + classify a Yardstick folder, layout from Yardstick.Project.psd1, recipe metadata
    ├─ recipe.rs         ordered YAML document model (key order kept, scripts as literal blocks) + templates
    ├─ fields.rs         field catalogue mirroring Test-RecipeSchema's known fields/enums + skill guidance
    ├─ lint.rs           instant Rust-side checks mirroring the skill's rules
    ├─ harness.rs        builds pwsh invocations of Tests/RecipeHarness/*.ps1 and the shared-content sync
    ├─ promote.rs        publishing by pull request: branch in a temporary worktree, push, gh pr create or compare URL
    ├─ validation.rs     ledger of recipe hashes that passed a live install test ("complete")
    ├─ git.rs            git clone/fetch/pull (ff-only) and ahead/behind status; finds git after a PATH refresh
    ├─ update.rs         self-update: latest GitHub release, SHA-256-verified MSI download, install-and-relaunch helper
    ├─ results.rs        parses _install-tests/<ts>/summary.json into verdicts and phases
    ├─ agent.rs          prompt (guardrails + task), CLI command and model list for Copilot CLI / Claude Code
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
- git fetch/pull jobs refresh the ahead/behind status (a pull also rescans recipes and reloads an
  unedited open recipe); shared-sync jobs rescan icons and re-lint.

When a writable workspace opens, and on the library's *Refresh*, the Studio runs `git fetch`
and Yardstick's `Sync-RecipeHarnessSharedContent` (only when `syncSharedContent: true`), each
switchable in Settings. Production folders are never fetched or synced.

Self-update runs outside the job system. Background threads check for and download the
release, and the result is verified before anything is installed. To install, the Studio starts a hidden
Windows PowerShell helper (`update::installer_helper`) and exits. The helper waits for
the Studio's PID to exit, runs `msiexec /i … /passive` elevated (`-Verb RunAs`), and starts
the Studio again with `--updated <exit code> <log>`, which the new process reports in the
status bar.

Only one agent process runs at a time, whether it came from the Agent view, a batch, or an extraction.

Live validation requires all eight validation phases to pass (warnings are allowed).
The result reader recognizes the harness's `PostInstallDetect` and
`PostUninstallDetect` names as well as the older `PostDetect` and `PostUninstall`
names. A run with `-SkipUninstall` remains incomplete even if its overall status
is `Pass`; it has not tested uninstall, post-uninstall detection or residue.

### Batch mode (`intake.rs`)

1. **Read.** The file is converted into `Record`s (title, description, ticket id, requester, and the remaining fields). The format is detected from the extension and the content: ServiceNow exports (`records`/`result` arrays, `number`/`short_description` keys), generic JSON/JSONL/YAML arrays, CSV/TSV with delimiter sniffing, spreadsheets via `calamine`, `docx` paragraphs, HTML with tags stripped, and plain text or email split into blocks. Header guessing handles columns such as "Short description", "Software" and "Product".
2. **Quick scan.** Each record is split into software items: one per product line, bullet, or "X and Y" list. The scan strips request boilerplate ("Intune - Package", "please install", version numbers, licence noise), picks up versions and vendor URLs, and flags records that aren't software (meetings, group or device administration).
3. **Agent extraction (optional).** The batch folder receives `digest.md` (the condensed records) and `recipes.md` (the existing recipe ids and names). The agent writes `agent-items.json`, which replaces the quick-scan items. The extract prompt allows writes only inside the batch folder.
4. **Match.** Item names are normalised and compared with recipe ids and display names (`RecipeMeta`), to tell an *Update* or *Already packaged* item from a *New* one. The match also records whether the recipe's `iconFile` exists.
5. **Run.** Selected items are queued. Each one becomes an `AgentTask::Create` or `AgentTask::Update` with the ticket context; if an icon was already saved, the agent is told to use it. Status, changed recipes and the log tail are stored on the item. The batch (`batch.json`) is saved after every change, and Running items are reset to Pending when a batch is loaded again.
6. **Test and repair (`workflow.rs`).** Studio runs Lint, Schema, Download, Package,
   DryRun and optionally Live for each authored Development recipe. A recipe failure is
   returned to an `AgentTask::Fix`, then all checks restart. The default limit is five
   test attempts (the first check plus at most four repairs); environmental blockers,
   cancellation, missing recipes and missing/skipped evidence stop automatic repair.
   The agent itself is still forbidden from running live installs.
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
*Copy to Development* is a plain file copy. The linter (`lint.rs`) gives instant
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
  pushes a `studio/<id>` branch cut from the upstream default branch, in a temporary
  worktree so the user's checkout is untouched, and opens a pull request; production pulls
  merged work. Icons go to the shared icon folder, not git. Recipes that are not *complete*
  (no passing live test of their current contents) show their recorded checks first, and
  the pull request says they are not validated. Nothing opens in the browser without asking.
- **Agent guardrails.** Agents run in the development workspace only, and their file
  tools are confined to it (`--add-dir`, no `--allow-all-paths`). The shell tool may not
  run `Yardstick.ps1`, `Invoke-Yardstick`, `Publish-Yardstick.ps1` or `git push`. Review
  tasks also deny writes. The prompt opens with explicit rules: work only in
  `Recipes\Development`, never touch production, run only harness dry runs, and report
  the recipes changed on a `STUDIO-RECIPES:` line.
- **Recipe paths** are checked to lie inside the workspace. Recipe ids are validated
  before they are passed to PowerShell, and every argument is single-quote escaped
  (`ps_quote`).

## User journeys

1. **Design.** Home or Recipes → *New recipe* (from a `Templates\` file) or *Copy to
   Development* → the recipe's Design tab steps (Identity, Source & version, Download scripts, Install & uninstall,
   Detection, Lifecycle & assignments, Advanced) with live lint → Ctrl+S.
2. **Test.** The recipe's Test tab → *Run safe checks* (Lint → Schema → install-test dry run). Then,
   optionally: Lab download/package, and a live install test after consent. Runs shows
   the history and per-phase results.
3. **Agentic.** *New recipe* → *Write it with the agent* makes a one-item request that is
   written, tested and repaired like any other; or the open recipe's Agent tab (Fix with the
   latest failing run, Review, Update, Freeform) → stream → back to Test.
4. **Publish.** Publish → select complete Development recipes → *Open pull requests…* →
   confirm → open the pull request (or GitHub's compare page) in the browser.
5. **Requests.** Import a ticket export or paste a list → review the extracted items
   (optionally *Extract with agent*) → find missing icons → *Run agent on N selected* →
   publish the recipes it produced. *Update several recipes* makes a request from library
   recipes to re-test, repair or update together.

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

## Testing

- `.\build.ps1 -Task test` runs the unit tests: classification, refusals, lossless edits,
  lint rules, command construction, summary parsing and highlighting.
- `crates/ystudio-core/tests/live_harness.rs` is ignored by default. It runs the real
  Schema check and an install-test dry run against a throwaway clone, and checks that the
  production folder is refused.
