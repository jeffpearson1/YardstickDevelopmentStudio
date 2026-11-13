#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $ticket = Join-Path $workItem 'ticket.md'
    if (-not (Test-Path -LiteralPath $ticket -PathType Leaf)) {
        # A ticket started from an SCCM application has its description in intake\export.md.
        $export = Join-Path $workItem 'intake\export.md'
        if (-not (Test-Path -LiteralPath $export -PathType Leaf)) { throw 'There is no ticket.md. Create the ticket with the request text.' }
        Copy-Item -LiteralPath $export -Destination $ticket
    }
    $parsed = & (Join-Path $PSScriptRoot 'ConvertFrom-SoftwareTicket.ps1') -TicketText ([IO.File]::ReadAllText($ticket))
    $path = Join-Path $workItem 'parsed-ticket.json'
    $parsed | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $path -Encoding utf8NoBOM
    @{
        Summary   = "$(@($parsed.Urls).Count) link(s), $(@($parsed.DropboxUrls).Count) Dropbox, $(@($parsed.TargetStrings).Count) asset tag(s)."
        Outputs   = @{ parsed = 'parsed-ticket.json' }
        Artifacts = @(New-StudioArtifact -WorkItem $workItem -Path $path -Label 'Parsed ticket')
    }
}
