#Requires -Version 7.0
# Reads the selected application's details from the site and writes its export folder.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'SccmMigration.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $provider = Get-StudioSetting $in 'smsProvider'
    $siteCode = Get-StudioSetting $in 'siteCode'
    if (-not $provider -or -not $siteCode) { throw 'Set the SMS Provider server and site code in the plugin settings.' }
    $protocol = [string](Get-StudioSetting $in 'cimProtocol'); if (-not $protocol) { $protocol = 'Dcom' }
    $selection = Get-StudioParam $in 'selection'
    if (-not $selection) { throw 'Select an application first.' }
    $workRoot = $in.workItem
    $session = Open-SccmSession -Provider $provider -Protocol $protocol
    try {
        $made = Export-SccmApplication -Session $session -Namespace (Get-SccmNamespace -SiteCode $siteCode) -SiteCode $siteCode -Provider $provider -WorkRoot $workRoot -Row $selection
    } finally {
        Remove-CimSession -CimSession $session -ErrorAction SilentlyContinue
    }
    $mapping = $made.Mapping
    $dts = @($mapping.deploymentTypes)
    $status = 'ok'
    $parts = foreach ($dt in $dts) { "$($dt.title): detection $($dt.status)" }
    if (@($dts | Where-Object { $_.status -ne 'mapped' }).Count -gt 0 -or @($mapping.notes).Count -gt 0) { $status = 'warn' }
    $artifacts = foreach ($file in 'sccm-export.json', 'intune-mapping.json', 'export.md') {
        $path = Join-Path $made.Folder $file
        [ordered]@{ path = "$(Split-Path $made.Folder -Leaf)/$file"; sha256 = Get-FileSha256 -Path $path; label = $file }
    }
    @{
        Status    = $status
        Summary   = "Exported $(Get-DisplayTitle -Name $made.Export.application.name -Version $made.Export.application.version): $($dts.Count) deployment type(s). $($parts -join '; ')."
        Outputs   = @{ folder = (Split-Path $made.Folder -Leaf) }
        Artifacts = @($artifacts)
    }
}
