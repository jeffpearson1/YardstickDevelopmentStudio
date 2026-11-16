#Requires -Version 7.0
# Lists the applications whose name or manufacturer contains params.query (all of them when empty).
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
    $query = [string](Get-StudioParam $in 'query' '')
    $limit = 300
    $session = Open-SccmSession -Provider $provider -Protocol $protocol
    try {
        $found = Find-SccmApplications -Session $session -Namespace (Get-SccmNamespace -SiteCode $siteCode) -Text $query -Limit $limit -WorkRoot $in.workItem
    } finally {
        Remove-CimSession -CimSession $session -ErrorAction SilentlyContinue
    }
    $summary = if ($found.Total -eq 0) { 'No applications match.' }
        elseif ($found.Total -gt $limit) { "Showing the first $limit of $($found.Total) applications; narrow the search." }
        else { "$($found.Total) application(s)." }
    @{ Summary = $summary; Rows = @($found.Rows) }
}
