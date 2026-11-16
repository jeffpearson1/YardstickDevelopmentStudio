#Requires -Version 7.0
# Builds the change for you to approve. It reads the tenant to re-check the target group and
# changes nothing there.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $tenantId = Get-StudioSetting $in 'tenantId'
    if (-not $tenantId) { throw 'Set the Tenant ID in the plugin settings.' }
    $prepared = New-OneOffLiveChange -WorkItem $in.workItem -TenantId $tenantId -TenantName (Get-StudioSetting $in 'tenantName') `
        -DefaultGroupId (Get-StudioSetting $in 'defaultGroupId') -CredentialTarget (Get-StudioSetting $in 'credentialTarget')
    $change = $prepared.Change
    $groups = @($change.apps | ForEach-Object { $_.assignment.groupDisplayName } | Select-Object -Unique)
    @{
        Summary   = "Prepared $(@($change.apps).Count) app(s) for $($change.tenantName), Available to $($groups -join ', '). Review the change set and approve it."
        Outputs   = @{ changeSet = 'intune-upload/live-change-set.json'; summary = 'intune-upload/live-change-summary.md' }
        Artifacts = @(
            (New-StudioArtifact -WorkItem $in.workItem -Path $prepared.ChangePath -Label 'Change set'),
            (New-StudioArtifact -WorkItem $in.workItem -Path $prepared.SummaryPath -Label 'Change summary')
        )
    }
}
