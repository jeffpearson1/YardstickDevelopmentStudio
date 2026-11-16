#Requires -Version 7.0
# Creates the approved apps in Intune and makes them Available. It runs only for the
# change set whose SHA-256 you approved, never sends a Graph DELETE, and can be run again
# under the same approval to finish an interrupted change.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $tenantId = Get-StudioSetting $in 'tenantId'
    if (-not $tenantId) { throw 'Set the Tenant ID in the plugin settings.' }
    $approval = Assert-StudioApproval -StudioInput $in -WorkItem $in.workItem -GateFile 'intune-upload/live-change-set.json'
    $result = Invoke-OneOffLiveChange -WorkItem $in.workItem -ChangeSetSha256 ([string]$approval.sha256) -TenantId $tenantId `
        -TenantName (Get-StudioSetting $in 'tenantName') -CredentialTarget (Get-StudioSetting $in 'credentialTarget')
    $path = Join-Path $in.workItem 'intune-upload\live-change-result.json'
    @{
        Summary   = "Created and assigned $(@($result.apps).Count) app(s) as Available: $((@($result.apps | ForEach-Object { "$($_.displayName) $($_.version)" })) -join ', ')."
        Outputs   = @{ result = 'intune-upload/live-change-result.json' }
        Artifacts = @(New-StudioArtifact -WorkItem $in.workItem -Path $path -Label 'Live change result')
    }
}
