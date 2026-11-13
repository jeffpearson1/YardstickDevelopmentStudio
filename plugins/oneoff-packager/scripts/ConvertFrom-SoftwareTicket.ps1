#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory, ValueFromPipeline)]
    [AllowEmptyString()]
    [string]$TicketText,

    [switch]$AsJson
)

begin { $parts = [System.Collections.Generic.List[string]]::new() }
process { $parts.Add($TicketText) }
end {
    $raw = $parts -join [Environment]::NewLine
    $normalized = $raw -replace '\\([&_])', '$1'

    $urls = [System.Collections.Generic.List[string]]::new()
    $seenUrls = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $markdownTargets = [regex]::Matches($normalized, '\[[^\]]*\]\((https?://[^\s\)]+)\)')
    foreach ($match in $markdownTargets) {
        $value = $match.Groups[1].Value.TrimEnd('.', ',', ';')
        if ($seenUrls.Add($value)) { $urls.Add($value) }
    }
    $withoutMarkdown = [regex]::Replace($normalized, '\[[^\]]*\]\(https?://[^\s\)]+\)', ' ')
    foreach ($match in [regex]::Matches($withoutMarkdown, 'https?://[^\s<>\]\)]+')) {
        $value = $match.Value.TrimEnd('.', ',', ';')
        if ($seenUrls.Add($value)) { $urls.Add($value) }
    }

    $targets = @([regex]::Matches($normalized, '(?i)\bNC-\d+\b') |
        ForEach-Object { $_.Value.ToUpperInvariant() } |
        Sort-Object -Unique)

    $products = @()
    $productMatch = [regex]::Match($normalized, '(?is)\bsoftware\s+(?:is|are)\s+(.+?)(?:\.|\r?\n|$)')
    if ($productMatch.Success) {
        $products = @($productMatch.Groups[1].Value -split '\s*,\s*|\s+and\s+' |
            ForEach-Object { ($_.Trim(' ', '.', ';', ':') -replace '^(?i:and)\s+', '') } |
            Where-Object { $_ } |
            Select-Object -Unique)
    }

    $result = [pscustomobject]@{
        ProductsHeuristic = $products
        TargetStrings     = $targets
        Urls              = @($urls)
        DropboxUrls       = @($urls | Where-Object { $_ -match '(?i)^https://(?:www\.)?dropbox\.com/' })
        HasDownloadLinks  = ($urls.Count -gt 0)
        NormalizedText    = $normalized
    }

    if ($AsJson) { $result | ConvertTo-Json -Depth 5 } else { $result }
}
