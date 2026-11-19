#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

# Downloads what the ticket links to, so triage can read the installers' own facts and
# the files can go to the software dropbox. Nothing downloaded is run.
Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $parsedPath = Join-Path $workItem 'parsed-ticket.json'
    if (-not (Test-Path -LiteralPath $parsedPath -PathType Leaf)) { throw 'parsed-ticket.json is missing. Run "Extract links and targets" first.' }
    $parsed = [IO.File]::ReadAllText($parsedPath) | ConvertFrom-Json
    $urls = @($parsed.Urls | Where-Object { $_ })
    $maxGb = [double](Get-StudioParam -StudioInput $in -Name 'maxGb' -Default 4)
    $result = Save-OneOffTicketDownloads -WorkItem $workItem -Urls $urls -MaxBytes ([long]($maxGb * 1GB))
    $failed = @($result.Links | Where-Object { $_.result -like 'failed*' -or $_.result -like 'too large*' -or $_.result -like 'zip kept*' })
    $summary = if ($urls.Count -eq 0) { 'The ticket has no links.' } else {
        "$(@($result.Files).Count) file(s) from $($urls.Count) link(s)" + $(if ($failed.Count) { "; $($failed.Count) link(s) could not be fetched (see downloads.md)." } else { '.' })
    }
    $artifacts = @(
        New-StudioArtifact -WorkItem $workItem -Path (Join-Path $workItem 'downloads.json') -Label 'Downloads'
        New-StudioArtifact -WorkItem $workItem -Path (Join-Path $workItem 'downloads.md') -Label 'Downloaded installers'
    )
    @{
        # A link that cannot be fetched is worth a look, but triage can still go on.
        Status    = if ($failed.Count) { 'warn' } else { 'ok' }
        Summary   = $summary
        Outputs   = @{ downloads = 'downloads.json' }
        Artifacts = $artifacts
    }
}
