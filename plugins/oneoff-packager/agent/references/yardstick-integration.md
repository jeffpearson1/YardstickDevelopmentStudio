# YardstickDev integration

## Isolation

YardstickDev is the one upstream source, and it is read-only for every ticket. The **Create the isolated runtime** step makes `runtime\` a fresh `git clone` of YardstickDev's checked-out commit (uncommitted changes there are left out), copies in the git-ignored files a ticket needs (preferences, the icon library, `*.common.yaml` recipe data), rewrites source-root paths in the copied preferences, inserts a fail-closed guard in `Invoke-YardstickGraphRequest` that throws on any HTTP `DELETE`, and writes `runtime\.oneoff-runtime.json`. It refuses a non-git source. The last stage, **Clean up the runtime**, deletes the clone once the Intune change is done.

A second script proves the guard: a synthetic `DELETE` must be blocked before authentication. Every step that talks to Intune runs that proof first and refuses to go on if it fails. If the copied Graph layer ever stops routing every request through `Invoke-YardstickGraphRequest`, the guard must be updated; never assume safety.

You work **in** `runtime\` only to read. Do not edit it.

## Checking a recipe

Use the runtime's own schema validator on a recipe you wrote:

```powershell
pwsh -NoProfile -File runtime\Tests\RecipeHarness\Test-RecipeSchema.ps1 -RecipePath recipes\<id>.yaml
```

A recipe is valid only when that reports no errors. This proves the recipe's shape, not that it installs.

## Recipes of existing apps

`runtime\Recipes\` holds Yardstick's shipped recipes. A sibling recipe for the same installer family is the best starting point for switches and detection, but it is evidence, not approval: check it against this ticket.

## Not yours

The Prepare and install tests, packaging, the Intune change and the upload are Studio steps. The test wrapper writes evidence under `artifacts\package-tests\<timestamp>\` (`invocation.json` with recipe and harness hashes and the YardstickDev commit, and the harness summaries and logs). The wrapper never passes `-Force` or `-SkipUninstall`, and never contacts Intune.

Publication uses Yardstick's guarded internal Graph and Intune functions through `-NoDelete` and the runtime's guard. YardstickDev's `Deploy-YardstickApps.ps1` is never used: it imports an external Intune module whose HTTP transport is outside the guard. Any app created gets `allowAvailableUninstall = true` unless the ticket needs it off; this only offers the user an Uninstall button on an Available app and is not an uninstall assignment.
