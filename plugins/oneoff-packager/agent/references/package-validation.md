# Local package validation

## What the tests are

Both tests are Studio steps that wrap the YardstickDev recipe harness. Neither contacts Graph or Intune. Both run the recipe's download and pre-processing PowerShell, so a recipe is code: inspect it before you rely on a result.

- **Prepare** (the default) runs schema validation, the recipe's pre- and post-download scripts, downloads or copies the installer, checks signature, hash and metadata, generates the install and detection scripts and runs pre-install detection. It does **not** run install or uninstall. The installer is kept so it can be packaged later.
- **Full** installs on the Studio's PC, detects, optionally reinstalls, uninstalls, detects again and looks for residue. It runs only after the user approves the exact recipes and that PC, and it asks for one UAC prompt for system-context recipes.

Evidence for a run is in `artifacts\package-tests\<timestamp>\`: `invocation.json` (recipe and harness hashes, the YardstickDev commit, settings) and the harness `summary.json` and `summary.md`, with a `result.json` and logs per recipe.

## Outcomes and what they mean

A Prepare result proves preparation and pre-detection only. It is never evidence that install or uninstall works. Report `WhatIf`, `Blocked`, `Skipped`, partial and warning outcomes as they are.

A recipe is **locally validated** only when the selected payload and version have evidence for all of: schema and download success; the expected signature status or an explicitly reviewed warning; pre-detection false; install exit success; post-install detection true; uninstall exit success; post-uninstall detection false; residue reviewed with warnings explained; no harness error or skipped required phase.

## Protections you should know about

- The harness refuses a recipe whose app is **already installed** (pre-detection true), because an uninstall test could remove software it did not install. The wrapper never overrides this.
- A successful Full run must attempt the cleanup; the wrapper has no switch to skip uninstall.
- The wrapper will not start while another installer is active: Windows Installer's in-progress key, its `_MSIExecute` mutex or a running `msiexec` blocks it.
- The install test runs on a packaging PC or disposable VM, not a normal workstation, because an uninstall can remove pre-existing software.

## Fixing a failed test

Read the first non-passing phase and its detail. Fix recipe faults. Do not change a recipe for an environmental failure (an installer already running, UAC dismissed, a full disk, a machine policy); say so and finish with `STUDIO-BLOCKED: <reason>`. Never weaken detection to get a pass.

Changing a recipe changes its SHA-256: the Prepare test must be run again, and the install test needs a new approval.
