#Requires -Version 7.0
# Opens a read-only CIM session to the SMS Provider and reads the site: the catalog page's connection check.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'SccmMigration.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $provider = Get-StudioSetting $in 'smsProvider'
    $siteCode = Get-StudioSetting $in 'siteCode'
    if (-not $provider) { throw 'Set the SMS Provider server in the plugin settings.' }
    if (-not $siteCode) { throw 'Set the site code in the plugin settings.' }
    $protocol = [string](Get-StudioSetting $in 'cimProtocol'); if (-not $protocol) { $protocol = 'Dcom' }
    $session = Open-SccmSession -Provider $provider -Protocol $protocol
    try {
        $site = Test-SccmConnection -Session $session -SiteCode $siteCode
    } finally {
        Remove-CimSession -CimSession $session -ErrorAction SilentlyContinue
    }
    @{ Summary = "Connected to $provider over ${protocol}: site $($site.SiteCode) ""$($site.SiteName)"", version $($site.Version)." }
}
