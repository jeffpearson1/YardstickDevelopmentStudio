A recipe failed a test. Read the evidence the Studio gives you below, work out the cause, and fix the recipe.

Read `.studio\agent\references\package-validation.md` first.

## Reading the evidence

The evidence lists the latest Prepare and install tests. Each `summary.json` has, per recipe, an `Overall` outcome and a list of phases (Schema, Download, PreDetect, Install, PostInstallDetect, Uninstall, PostUninstallDetect, Residue) with a status and detail for each. Start at the first phase that is not a pass, and read its detail and the logs next to the summary.

## What you may change

- `recipes\<id>.yaml`: switches, detection, download logic, commands, fields.
- `icons\` and `research-and-status.md`, to keep the notes honest.

Do not change the evidence, the packages or the YardstickDev copy in `runtime\`.

## Judge the cause before you edit

- **A recipe fault** (wrong switch, detection true or false at the wrong time, wrong file name, a download script bug, a placeholder not filled): fix it.
- **An environmental fault** (Windows Installer busy, error 1618, a UAC prompt dismissed, the test PC low on disk, a machine-wide policy, the app already installed so the test refused it, the network): **do not edit the recipe**. Explain the cause in your report and finish with `STUDIO-BLOCKED: <reason>`.
- **A detection that passed after install but failed before uninstall cleanup, or leftovers (Residue warnings)**: a leftover is a warning, not a failure. Fix the uninstall only if the leftover is the app's own and is clearly meant to be removed.

Do not weaken detection to make a test pass: a detection that is true for the wrong thing is worse than a failing one.

## Finish

Check the recipe with `runtime\Tests\RecipeHarness\Test-RecipeSchema.ps1 -RecipePath recipes\<id>.yaml`. Do not run any test that installs. Say exactly what you changed and why. The user will run the Prepare test again, and the install test needs their approval again because the recipe's hash changed.
