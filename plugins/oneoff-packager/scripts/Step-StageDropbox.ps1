#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

# Puts each product's ticket installers in <SoftwareDropbox>\<product id>\, where Yardstick's
# manualDownload recipes look for them, and notes them on the product for recipe authoring.
Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $dropbox = Get-OneOffSoftwareDropbox -YardstickDev (Get-StudioSetting -StudioInput $in -Name 'yardstickDevPath')
    $result = Copy-OneOffTicketInstallers -WorkItem $workItem -SoftwareDropbox $dropbox
    $products = Join-Path $workItem 'products.json'
    $status = 'ok'
    $summary = if (-not $dropbox) {
        $status = 'warn'
        'No SoftwareDropbox is set in the YardstickDev preferences, so the downloads stay in this ticket only.'
    } elseif ($result.Staged + $result.AlreadyThere -eq 0) {
        'No product has a downloaded installer to stage.'
    } else {
        "Staged $($result.Staged) file(s) in the software dropbox" + $(if ($result.AlreadyThere) { "; $($result.AlreadyThere) were already there." } else { '.' })
    }
    if ($result.Notes.Count) {
        $status = 'warn'
        $summary += ' ' + ($result.Notes -join ' ')
    }
    @{
        Status    = $status
        Summary   = $summary
        Outputs   = @{ products = 'products.json' }
        Artifacts = @(New-StudioArtifact -WorkItem $workItem -Path $products -Label 'Products')
    }
}
