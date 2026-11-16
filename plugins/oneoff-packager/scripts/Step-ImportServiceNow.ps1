#Requires -Version 7.0
# Turns the items of a ServiceNow export (raw records, or the packaging handoff) into tickets.
# Runs against the plugin's work folder rather than one ticket; the Studio creates the tickets
# from the rows after you tick the ones you want.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force

function ConvertTo-TicketRow {
    param([string]$Number, [string]$Title, [string]$Text)
    [ordered]@{ key = $Number; reference = $Number; title = $Title.Trim(); text = $Text.Trim() }
}

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $file = [string](Get-StudioParam $in 'file' '')
    if (-not $file -or -not (Test-Path -LiteralPath $file -PathType Leaf)) { throw 'Choose an export file.' }
    $rows = [System.Collections.Generic.List[object]]::new()
    switch ([IO.Path]::GetExtension($file).ToLowerInvariant()) {
        '.json' {
            $document = [IO.File]::ReadAllText($file) | ConvertFrom-Json -Depth 30
            $items = if ($document.PSObject.Properties['items']) { @($document.items) } elseif ($document.PSObject.Properties['records']) { @($document.records) } else { @($document) }
            foreach ($item in $items) {
                $get = { param($n) $p = $item.PSObject.Properties[$n]; if ($p -and $null -ne $p.Value) { [string]$p.Value } else { '' } }
                $number = & $get 'number'
                $title = (& $get 'software'); if (-not $title) { $title = & $get 'short_description' }
                $text = (& $get 'description_verbatim'); if (-not $text) { $text = & $get 'description' }
                $urls = $item.PSObject.Properties['supplied_urls']
                if ($urls -and $urls.Value) { $text += "`n`nLinks:`n" + ((@($urls.Value) | ForEach-Object { "- $_" }) -join "`n") }
                $short = & $get 'short_description'
                if ($short -and $short -ne $title) { $text = "$short`n`n$text" }
                if ($number -or $title) { $rows.Add((ConvertTo-TicketRow -Number $number -Title $(if ($title) { $title } else { $number }) -Text $text)) }
            }
        }
        '.csv' {
            foreach ($row in Import-Csv -LiteralPath $file) {
                $get = { param($n) $p = $row.PSObject.Properties[$n]; if ($p -and $p.Value) { [string]$p.Value } else { '' } }
                $number = & $get 'number'
                $title = & $get 'short_description'; if (-not $title) { $title = & $get 'software' }
                $rows.Add((ConvertTo-TicketRow -Number $number -Title $(if ($title) { $title } else { $number }) -Text ((& $get 'description') -replace '^$', $title)))
            }
        }
        default {
            $rows.Add((ConvertTo-TicketRow -Number '' -Title ([IO.Path]::GetFileNameWithoutExtension($file)) -Text ([IO.File]::ReadAllText($file))))
        }
    }
    if ($rows.Count -eq 0) { throw 'No tickets were found in that file.' }
    @{ Summary = "Found $($rows.Count) ticket(s) in $([IO.Path]::GetFileName($file))."; Rows = @($rows) }
}
