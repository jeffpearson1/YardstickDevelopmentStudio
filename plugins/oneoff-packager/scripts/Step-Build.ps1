#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $built = New-OneOffLocalPackages -WorkItem $in.workItem
    $artifacts = @(New-StudioArtifact -WorkItem $in.workItem -Path $built.ManifestPath -Label 'Package manifest')
    $summary = "Built $(@($built.Packages).Count) package(s): $((@($built.Packages | ForEach-Object { "$($_['id']) $($_['version'])" })) -join ', ')."
    $status = 'ok'
    if (@($built.Skipped).Count -gt 0) {
        $status = 'warn'
        $summary += " Not packaged: $($built.Skipped -join '; ')."
    }
    @{
        Status    = $status
        Summary   = $summary
        Outputs   = @{ manifest = 'local-packages/package-manifest.json' }
        Artifacts = $artifacts
    }
}
