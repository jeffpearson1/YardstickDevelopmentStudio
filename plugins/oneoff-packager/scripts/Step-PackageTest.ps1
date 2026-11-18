#Requires -Version 7.0
# Runs the YardstickDev recipe harness over this ticket's recipes. params.mode is Prepare
# (installs nothing) or Full (installs, detects, uninstalls on this PC: needs the approval).
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $mode = [string](Get-StudioParam $in 'mode' 'Prepare')
    if ($mode -notin 'Prepare', 'Full') { throw "Unknown mode '$mode'." }
    $yardstickDev = Get-StudioSetting $in 'yardstickDevPath'
    if (-not $yardstickDev) { throw 'Set the YardstickDev folder in the plugin settings (or open a development workspace).' }
    $recipes = @(Get-ChildItem -LiteralPath (Join-Path $workItem 'recipes') -Filter *.yaml -File -ErrorAction SilentlyContinue | Sort-Object Name | Select-Object -ExpandProperty FullName)
    if ($recipes.Count -eq 0) { throw 'There are no recipes in recipes\. Complete the linked Recipe Builder work first.' }

    $arguments = @{
        TicketRoot       = $workItem
        RecipePath       = $recipes
        WorkspaceRoot    = $in.workRoot
        YardstickDevPath = $yardstickDev
        Mode             = $mode
    }
    if ([bool](Get-StudioParam $in 'keepInstaller' $true)) { $arguments['KeepInstaller'] = $true }
    if ($mode -eq 'Full') {
        # The Studio verified the approval before starting; this is the second check, and
        # the approved request must still describe these recipes on this PC.
        $approval = Assert-StudioApproval -StudioInput $in -WorkItem $workItem -GateFile 'artifacts/full-test-request.json'
        $request = [IO.File]::ReadAllText((Join-Path $workItem 'artifacts\full-test-request.json')) | ConvertFrom-Json -Depth 20
        if ($request.host -ne $env:COMPUTERNAME) { throw "The approval is for $($request.host); this PC is $env:COMPUTERNAME." }
        $approvedPaths = @($request.recipes | ForEach-Object { $_.path } | Sort-Object)
        $currentPaths = @($recipes | ForEach-Object { ConvertTo-WorkItemRelative -WorkItem $workItem -Path $_ } | Sort-Object)
        if (($approvedPaths -join '|') -ne ($currentPaths -join '|')) { throw 'The recipes changed since the install test was approved.' }
        foreach ($recipe in $request.recipes) {
            $full = Resolve-WorkItemPath -WorkItem $workItem -Relative $recipe.path
            if ((Get-FileSha256 -Path $full) -ne ([string]$recipe.sha256).ToUpperInvariant()) { throw "$($recipe.path) changed after the install test was approved." }
        }
        $arguments['ApproveLocalInstallUninstall'] = $true
        if ([bool](Get-StudioParam $in 'includeReinstall' $false)) { $arguments['IncludeReinstall'] = $true }
    }

    $run = & (Join-Path $PSScriptRoot 'Invoke-OneOffPackageTest.ps1') @arguments
    if (-not $run -or -not $run.Passed) { throw "The $mode test failed (exit code $($run.ExitCode)). See the log and $($run.InvocationManifest)." }

    $results = foreach ($summaryPath in @($run.SummaryPaths)) {
        $summary = [IO.File]::ReadAllText($summaryPath) | ConvertFrom-Json -Depth 30
        @($summary.Results)
    }
    $outcomes = @($results | ForEach-Object { "$($_.Id): $($_.Overall)" })
    $worrying = @($results | Where-Object { $_.Overall -match 'warn' -or $_.Overall -match 'Skipped|Blocked|Partial' })
    $artifacts = @($run.SummaryPaths | ForEach-Object { New-StudioArtifact -WorkItem $workItem -Path $_ -Label "$mode summary" })
    @{
        Status    = if ($worrying.Count -gt 0) { 'warn' } else { 'ok' }
        Summary   = "$mode test run $($run.RunId): $($outcomes -join '; ')."
        Outputs   = @{ evidence = (ConvertTo-WorkItemRelative -WorkItem $workItem -Path $run.EvidenceRoot) }
        Artifacts = $artifacts
    }
}
