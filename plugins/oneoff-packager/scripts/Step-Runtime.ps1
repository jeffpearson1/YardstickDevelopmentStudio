#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $yardstickDev = Get-StudioSetting $in 'yardstickDevPath'
    if (-not $yardstickDev) { throw 'Set the YardstickDev folder in the plugin settings (or open a development workspace).' }
    $runtime = Join-Path $in.workItem 'runtime'
    $manifest = Join-Path $runtime '.oneoff-runtime.json'
    if ((Test-Path -LiteralPath $manifest) -and @(Get-ChildItem -LiteralPath $runtime -Force).Count -gt 0) {
        $note = 'The runtime already exists; its DELETE guard was proven again.'
    } else {
        & (Join-Path $PSScriptRoot 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $yardstickDev -DestinationPath $runtime | Out-Null
        $note = 'Created the runtime and proved its DELETE guard.'
    }
    $safety = & (Join-Path $PSScriptRoot 'Test-OneOffRuntimeSafety.ps1') -RuntimePath $runtime
    if (-not $safety.Safe) { throw 'The runtime DELETE guard did not pass.' }
    @{
        Summary = "$note YardstickDev commit $($safety.SourceCommit.Substring(0, 8))."
        Outputs = @{ runtime = 'runtime' }
    }
}
