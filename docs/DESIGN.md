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
│   └─ views/            home, recipes, designer, editor, testbench, agent, batch, icons, runs, docs, promote
└─ crates/ystudio-core   headless engine; no GUI types; unit tested
    ├─ workspace.rs      open + classify a Yardstick folder, layout from Yardstick.Project.psd1, recipe metadata
    ├─ recipe.rs         ordered YAML document model (key order kept, scripts as literal blocks) + templates
    ├─ fields.rs         field catalogue mirroring Test-RecipeSchema's known fields/enums + skill guidance
    ├─ lint.rs           instant Rust-side checks mirroring the skill's rules
    ├─ harness.rs        builds pwsh invocations of Tests/RecipeHarness/*.ps1 and Publish-Yardstick
    ├─ results.rs        parses _install-tests/<ts>/summary.json into verdicts and phases
    ├─ agent.rs          prompt (guardrails + task) and CLI command for Copilot CLI / Claude Code
    ├─ intake.rs         request-list readers, quick-scan extraction, recipe matching, Batch persistence
    ├─ icons.rs          multi-source icon search, scoring, dedupe, image clean-up
    ├─ process.rs        child processes with streamed stdout/stderr, cancel (taskkill /T)
    └─ settings.rs       %APPDATA%\YardstickStudio\settings.json
```

The GUI only renders state owned by core types and forwards user intent to them, so every
rule that matters for safety lives in `ystudio-core` and is tested there.

### Jobs

Every external action runs as a `RunningProcess`: harness steps, agent runs, git clone and
publish. Reader threads pump stdout and stderr into a channel and wake the egui context.
The UI calls `poll()` each frame. `poll()` returns *whether anything changed*; use
`is_running()` to find out whether the process is still alive. Cancelling kills the whole
process tree with `taskkill /T /F`. When a job finishes, `StudioApp::on_job_finished`
dispatches on `JobKind`:

- harness jobs reload results and verdicts;
- agent jobs reload the recipe library and open the recipes named in the agent's
  `STUDIO-RECIPES:` line;
- publish previews record whether the `-WhatIf` run succeeded for the current selection;
- batch extraction jobs load the agent's `agent-items.json` into the batch;
- agent jobs started from a batch item record that item's outcome, then start the next queued item.

Only one agent process runs at a time, whether it came from the Agent view, a batch, or an extraction.

### Batch mode (`intake.rs`)

1. **Read.** The file is converted into `Record`s (title, description, ticket id, requester, and the remaining fields). The format is detected from the extension and the content: ServiceNow exports (`records`/`result` arrays, `number`/`short_description` keys), generic JSON/JSONL/YAML arrays, CSV/TSV with delimiter sniffing, spreadsheets via `calamine`, `docx` paragraphs, HTML with tags stripped, and plain text or email split into blocks. Header guessing handles columns such as "Short description", "Software" and "Product".
2. **Quick scan.** Each record is split into software items: one per product line, bullet, or "X and Y" list. The scan strips request boilerplate ("Intune - Package", "please install", version numbers, licence noise), picks up versions and vendor URLs, and flags records that aren't software (meetings, group or device administration).
3. **Agent extraction (optional).** The batch folder receives `digest.md` (the condensed records) and `recipes.md` (the existing recipe ids and names). The agent writes `agent-items.json`, which replaces the quick-scan items. The extract prompt allows writes only inside the batch folder.
4. **Match.** Item names are normalised and compared with recipe ids and display names (`RecipeMeta`), to tell an *Update* or *Already packaged* item from a *New* one. The match also records whether the recipe's `iconFile` exists.
5. **Run.** Selected items are queued. Each one becomes an `AgentTask::Create` or `AgentTask::Update` with the ticket context; if an icon was already saved, the agent is told to use it. Status, changed recipes and the log tail are stored on the item. The batch (`batch.json`) is saved after every change, and Running items are reset to Pending when a batch is loaded again.

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
  (`harness::build`, `harness::publish`, `harness::install_development_only`,
  `agent::build_command`), so no external process can start in production even if a UI
  path forgets a check. Tests cover these refusals. Saving, creating and copying recipes
  call it in the app before touching disk.
- **Consent.** Live install tests, `-Force` and real publishes need `Consent::Confirmed`,
  which is only produced by a modal with an acknowledgement checkbox. Dry runs and
  `-WhatIf` need no consent.
- **Two-step promote.** *Publish…* stays disabled until a successful `-WhatIf` preview of
  exactly the current selection. Recipes without a validated live install test get a
  warning.
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

1. **Design.** Recipes → *New recipe* (from a `Templates\` file) or *Copy to Development*
   → Designer steps (Identity, Source & version, Download scripts, Install & uninstall,
   Detection, Lifecycle & assignments, Advanced) with live lint → Ctrl+S.
2. **Test.** Test Bench → *Run safe checks* (Lint → Schema → install-test dry run). Then,
   optionally: Lab download/package, and a live install test after consent. Runs shows
   the history and per-phase results.
3. **Agentic.** Agent → choose a task (Create from a vendor URL/description, Fix with the
   latest failing run, Review, Update, Freeform) → stream → changed recipes open in the
   Designer → back to Test.
4. **Promote.** Promote → select validated Development recipes → Preview (`-WhatIf`) →
   Publish… → confirm.
5. **Batch.** Batch → open a ticket export → review the extracted items (optionally
   *Extract with agent*) → find missing icons → *Run agent on N selected* → test and
   promote the recipes it produced.

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
