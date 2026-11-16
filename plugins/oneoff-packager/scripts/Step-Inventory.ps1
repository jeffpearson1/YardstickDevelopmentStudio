#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $tenantId = Get-StudioSetting $in 'tenantId'
    if (-not $tenantId) { throw 'Set the Tenant ID in the plugin settings.' }
    $run = New-OneOffInventory -WorkItem $in.workItem -TenantId $tenantId -TenantName (Get-StudioSetting $in 'tenantName') `
        -DefaultGroupId (Get-StudioSetting $in 'defaultGroupId') -CredentialTarget (Get-StudioSetting $in 'credentialTarget')
    $inventory = $run.Inventory
    $existing = @($inventory.matchingApps)
    $unsuitable = @($inventory.groups | Where-Object { -not $_.suitable })
    $summary = "Scanned $($inventory.totalAppsScanned) apps in $($inventory.tenant.displayName): $($existing.Count) possible existing app(s)."
    $status = 'ok'
    if ($existing.Count -gt 0) {
        $status = 'warn'
        $summary += ' Review them before packaging: ' + ((@($existing | ForEach-Object { "$($_.displayName) $($_.displayVersion)".Trim() }) | Select-Object -First 5) -join '; ') + '.'
    }
    if ($unsuitable.Count -gt 0) {
        $status = 'warn'
        $summary += ' Target group problem: ' + ((@($unsuitable | ForEach-Object { "$($_.displayName) ($($_.problems -join ', '))" })) -join '; ') + '.'
    }
    @{
        Status    = $status
        Summary   = $summary
        Outputs   = @{ inventory = 'inventory/intune-inventory.json' }
        Artifacts = @(New-StudioArtifact -WorkItem $in.workItem -Path $run.Path -Label 'Intune inventory')
    }
}
