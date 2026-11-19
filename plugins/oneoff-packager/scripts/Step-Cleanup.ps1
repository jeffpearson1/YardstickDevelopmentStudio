#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

# The ticket is finished: remove its YardstickDev clone. Its manifest (source commit, guard
# hash) is kept in artifacts\ as the record of what the ticket ran with.
Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $runtime = Join-Path $workItem 'runtime'
    $record = Join-Path $workItem 'artifacts\runtime-manifest.json'
    $manifestPath = Join-Path $runtime '.oneoff-runtime.json'
    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
        New-Item -ItemType Directory -Path (Split-Path $record -Parent) -Force | Out-Null
        Copy-Item -LiteralPath $manifestPath -Destination $record -Force
    }
    $manifest = Remove-OneOffRuntime -Path $runtime
    $summary = if ($manifest) {
        "Removed the ticket's YardstickDev clone (commit $(([string]$manifest.sourceCommit).Substring(0, [Math]::Min(8, ([string]$manifest.sourceCommit).Length))))."
    } else { 'There was no YardstickDev clone to remove.' }
    @{
        Summary   = $summary
        Artifacts = @(if (Test-Path -LiteralPath $record) { New-StudioArtifact -WorkItem $workItem -Path $record -Label 'Runtime record' })
    }
}
