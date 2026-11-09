<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/branding/logo-dark.png">
    <img src="assets/branding/logo-light.png" alt="Yardstick" width="280">
  </picture>
</p>

# Yardstick Development Studio

A desktop IDE, written in Rust with [egui](https://github.com/emilk/egui), for building
[Yardstick](https://github.com/jeffpearson1/Yardstick) recipes. It walks you through
**designing** a recipe, **testing** it with the recipe harness, **creating or fixing it
agentically** with the repo's `yardstick-recipes` skill, and **publishing** it as a pull
request.

The Studio never writes to a production Yardstick folder and never runs the harness
there. You work in a development clone and publish finished recipes as pull requests;
production pulls them once they are merged.

## Feature tour

| View | What it does |
|------|--------------|
| **Home** | A launcher: **Continue** a request (the open one, or earlier ones with items still to do), **Import requests** from a file or pasted text, **New recipe**, **Edit a recipe** (search by id or name) and **Update several recipes**. Below it, the workspace status (production, development or unconfigured) with the clone's branch, how far behind it is, and *Fetch* / *Pull* / *Sync icons & overrides*, and a form that clones a development workspace from GitHub. |
| **Recipes** | Library of all shipped and `Recipes\Development` recipes, with filters, location and status badges, *New recipe*, *Copy to Development*, *Missing icons*, and *Update several…*, which picks recipes to re-test, repair or update together as one request. Opening a recipe shows its header (status, Save/Revert) and five tabs that all work on it: **Design**, **YAML**, **Test**, **Icon** and **Agent**. |
| **Recipes → Design** | Form-based editor whose field catalogue mirrors Yardstick's schema (`Test-RecipeSchema`) and the skill's guidance. Fields are grouped into steps, script fields get a highlighted PowerShell editor with optional line numbers, and it lints as you type. A field whose variable a download script assigns is not marked required; if the YAML leaves it blank it is shown read-only as *set by preDownloadScript* (or whichever script sets it). Saving keeps key order and writes scripts as literal blocks. It warns before dropping YAML comments; use the YAML tab to keep them. |
| **Recipes → YAML** | Raw YAML with syntax highlighting and optional line numbers, kept in sync with Design. |
| **Recipes → Test** | Runs the harness steps: Lint, Schema, Lab (download/package) and Install test (dry run or live). **Run all checks** runs all six steps in order, stops on failure, and pauses for explicit consent before the live install test. **Run safe checks** chains Lint, Schema and DryRun without installing anything. |
| **Recipes → Icon** | The icon finder, which replaces Yardstick's `Get-YardstickIcon.ps1`, filled in for the open recipe. It shows the recipe's current icon, and searches the web by itself only when there is no usable one. It searches installer resources (EXE/MSI), Wikidata/Commons, Chocolatey, the vendor site (manifest, touch icon, favicon) and image search in parallel, scores each result for relevance and merges duplicates. Preview on checker/dark/light backgrounds, clean it up (background removal, trim, square, margin), save it to `Icons\` and set `iconFile`. Also opens for a request item, from the library's *Missing icons*, or with `--icons "<app>"`. |
| **Recipes → Agent** | Runs the `yardstick-recipes` skill on the open recipe through GitHub Copilot CLI or Claude Code: Fix, Review, Update or Freeform. Fix and Update can **Test and repair automatically**: Studio runs every harness stage and returns recipe failures to the agent, up to five test attempts by default. Live install/uninstall checks require explicit run consent. Review is read-only. Every run is also kept under **Requests** as a one-item request (*fix app*, *review app*, …) with its status and test results, and *Run agent on this* there repeats the same task; the request you have open in Requests is not replaced. |
| **Requests** | New recipes, imported request lists and recipe updates go through one pipeline: the agent writes (or updates) each recipe, then Studio tests and repairs it. *New recipe…* with the agent makes a one-item request. Import a request list (ServiceNow/JSON export, CSV/TSV, Excel `xlsx/xls/ods`, YAML, text/Markdown/email, HTML, Word `docx`) by picker, *Paste text…*, drag and drop, or `yardstick-studio.exe <file>`; a quick scan pulls out the software, version, vendor URL and ticket for each request and flags requests that aren't software, and *Extract with agent* re-reads messy lists. *Run agent on N selected* creates or updates, tests and repairs each recipe in turn. **Test selected** rechecks the included recipes without editing them. **Repair with agent** tests the recipes no check has confirmed working yet and hands failures to the agent. One consent dialog covers a run's live install/uninstall tests, and system-context live tests share **one UAC prompt per run** through a hidden elevated worker (user-scope recipes run unelevated). Items show *recipe written*, *checks passed* or *complete · not published*. Requests are saved under `%APPDATA%\YardstickStudio\batches`; *Copy report* includes recorded check results. |
| **Runs** | Browses `Artifacts\RecipeHarness\_install-tests`, parsing `summary.json` into verdicts and phase tables. It opens on the open recipe's newest run. |
| **Publish** | Each selected development recipe becomes a pull request: the Studio cuts a `studio/<id>` branch from the upstream default branch in a temporary worktree (your checkout is untouched), commits the recipe to its shipped path with its test evidence, pushes it, and opens the pull request with `gh` when it is installed, or offers GitHub's compare page. It asks before opening anything in the browser. The recipe's icon is copied to the shared icon folder (`SharedIcons`), not into git. Production pulls merged changes itself. Recipes without a passing live test of their current contents get a dialog listing the recorded checks before you can *Open anyway*. |
| **Setup** | The first-run wizard: pick or clone a development workspace, check the tools (PowerShell 7, git, gh, an agent CLI) with links to install what is missing, then Yardstick's development-only setup. It reads what is already in place with `Install-Yardstick.ps1 -Status`, pre-fills the answers (optional PowerShell modules, allowing scripts for your account, shared icon and recipe override folders), and applies them with a non-interactive `Install-Yardstick.ps1 -DevelopmentOnly` run, showing each step's result. It opens on first run, after cloning, from Home's *Set up this clone…*, and from Settings. Needs a Yardstick clone that includes jeffpearson1/Yardstick#36; older clones fall back to running the installer in a console. |
| **Settings** | Updates, production and trusted development folders, the agent CLI and model (models are read from the CLI), PowerShell, the tool check (pwsh, git, gh, agent CLIs; *Re-check* reloads `PATH`), and appearance. Changes save as you make them. |
| **Help** | The skill and its reference docs, rendered as Markdown. |

A recipe is **complete** when a live install test passed with exactly its current contents:
the Studio records the recipe's hash in `Artifacts\RecipeHarness\_studio\validated.json`
when a live test passes, and shows *Complete · not published* until the recipe changes
(then *Changed since validation*). When a newer Studio is released, a banner above every
page offers it until you open it or choose *Later*.

See [docs/DESIGN.md](docs/DESIGN.md) for the architecture and the safety model.

## Prerequisites

- Windows 10/11 and PowerShell 7 (`pwsh`). Windows PowerShell 5.1 is used as a fallback.
- Rust from [rustup](https://rustup.rs). Either host toolchain works:
  - **x86_64-pc-windows-msvc**: needs the Visual Studio C++ build tools.
  - **x86_64-pc-windows-gnu**: no Visual Studio needed, but it does need MinGW-w64 binutils:
    `winget install BrechtSanders.WinLibs.POSIX.MSVCRT --scope user`.
    `build.ps1` finds it automatically and sets `-C dlltool=`.
- A GPU with DirectX 12 or Vulkan, or the WARP software rasteriser. Rendering uses
  `wgpu`, so it works on VMs and RDP sessions that lack OpenGL 2.
- For the agent, GitHub Copilot CLI (`copilot`) or Claude Code (`claude`). The Studio also
  looks in their usual install folders and re-reads `PATH`, so one installed while it is
  open is found.
- Optional: the GitHub CLI (`gh`), to open pull requests directly from Publish.

## Build and run

`cargo` doesn't need to be on `PATH`, because `build.ps1` adds `%USERPROFILE%\.cargo\bin`:

```powershell
.\build.ps1                 # release build -> target\release\yardstick-studio.exe
.\build.ps1 -Task run       # debug build and launch
.\build.ps1 -Task test      # unit tests (headless engine + GUI helpers)
.\build.ps1 -Task clippy    # lint with -D warnings
.\build.ps1 -Task installer # release build + MSI (see Installer below)
.\build.ps1 -Task build -Debug
```

Command-line shortcuts:

```powershell
yardstick-studio.exe "Exported ticket list.json"   # open a request list in Batch mode
yardstick-studio.exe --batch .\requests.xlsx       # same thing, explicitly
yardstick-studio.exe --icons "Notepad++"           # open the icon finder and search
```

The live integration tests are ignored by default. They run the real harness against a
throwaway development clone and check that production is refused:

```powershell
$env:YSTUDIO_TEST_CLONE = 'C:\Temp\ysclone'          # a git clone of Yardstick
$env:YSTUDIO_TEST_PRODUCTION = 'C:\Intune\Yardstick'  # used read-only, for icons and the refusal check
.\build.ps1 -Task test -CargoArgs '--test','live_harness','--','--ignored','--nocapture','--test-threads=1'
```

> If a build fails with `os error 32 ... being used by another process`, real-time
> antivirus scanning is locking a file in `target\`. Run the build again.

## Installer

```powershell
.\build.ps1 -Task installer   # release build + MSI -> target\installer\YardstickDevelopmentStudio-<version>-x64.msi
```

The installer is a per-machine x64 MSI built with [WiX Toolset](https://wixtoolset.org)
5.0.2, which is MS-RL licensed. WiX 6 and later charge an Open Source Maintenance Fee, so
the version is pinned. The first run downloads WiX, checks its SHA-256, and unpacks it into
`.tools\`. This needs no admin rights and installs nothing system-wide. The executable only
uses DLLs that ship with Windows, so the MSI installs just `yardstick-studio.exe` and
`LICENSE.txt` into `Program Files\Yardstick Development Studio`, plus a Start menu shortcut.

| Task | Command |
| --- | --- |
| Interactive install | Double-click the MSI. It shows the licence, then asks for admin rights. |
| Silent install | `msiexec /i YardstickDevelopmentStudio-<version>-x64.msi /qn` |
| Silent uninstall | `msiexec /x YardstickDevelopmentStudio-<version>-x64.msi /qn` |

To deploy with Intune, add the MSI as a *Line-of-business app* (or wrap it as a Win32
app), install it in the device context, and detect it by its MSI product code. Every
build gets a new product code and shares one UpgradeCode, so installing a newer version
replaces the old one, and installing an older version over a newer one is refused. Bump
`version` in the root `Cargo.toml` for each release; it becomes the MSI version.

Settings in `%APPDATA%\YardstickStudio` are left in place on uninstall. The MSI and
executable aren't code-signed yet, so SmartScreen may warn on first run. Sign both with
`signtool` before distributing widely.

### Updates

The Studio updates itself from this repository's
[GitHub Releases](https://github.com/jeffpearson1/YardstickDevelopmentStudio/releases).
It checks at startup (switch this off under *Updates* on Home) or when you choose
*Check for updates*. *Download and install* fetches the release's `*-x64.msi` and checks
it against the SHA-256 digest GitHub publishes for the asset. It then closes the Studio,
runs the MSI with a progress bar (Windows asks for admin rights) and reopens the Studio.
The update waits while jobs are running or a recipe has unsaved changes. To publish an
update, bump `version` in `Cargo.toml`, run `.\build.ps1 -Task installer`, and attach
the MSI to a non-draft, non-pre-release GitHub release tagged `v<version>`.
Intune-managed installs can ignore the updater: the next Intune deployment of the same or
a newer MSI simply upgrades in place.

The installer artwork in `installer\*.bmp` comes from `assets\branding`. To regenerate it,
run `python installer\make-art.py`, which needs Pillow.

## Performance

On machines without a GPU, such as VMs and RDP sessions, the Studio automatically uses its
own multithreaded CPU renderer instead of WARP. On the reference VM that takes rendering
from about 1.8 fps to about 100 fps.

| Setting | Effect |
| --- | --- |
| `YSTUDIO_RENDERER=software` or `wgpu` | Forces a renderer. |
| Ctrl+Shift+P | Shows UI-build and paint times in the status bar. |
| `YSTUDIO_PERF_LOG=<file>` | Logs the view, UI ms, timestamp and paint ms for each frame. |
| `YSTUDIO_PERF_CONTINUOUS=1` | Repaints continuously, for benchmarking. |

## First run

1. Launch the Studio. The setup wizard opens: clone a development workspace (or open one;
   `C:\Intune\Yardstick` stays read-only as *Production*), check the tools, and run
   Yardstick's development-only setup. Settings are stored in
   `%APPDATA%\YardstickStudio\settings.json`, which also holds the production paths and
   trusted development paths.
2. Run setup again at any time from **Settings**.
3. From **Home**, start a **New recipe** (the agent writes, tests and repairs it), import
   requests, or open a recipe and *Copy to Development* to edit it on its **Design** tab.
4. On the recipe's **Test** tab, run *safe checks*, then a live install test. A passing
   live test marks it *Complete · not published*.
5. **Publish** it: the Studio opens a pull request.
