#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$RuntimePath)

$ErrorActionPreference = 'Stop'
$runtime = (Resolve-Path -LiteralPath $RuntimePath -ErrorAction Stop).Path
$manifestPath = Join-Path $runtime '.oneoff-runtime.json'
$graphPath = Join-Path $runtime 'Modules\YardstickGraph.psm1'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw 'Runtime manifest is missing.' }
if (-not (Test-Path -LiteralPath $graphPath -PathType Leaf)) { throw 'Copied Graph module is missing.' }

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$currentHash = (Get-FileHash -LiteralPath $graphPath -Algorithm SHA256).Hash
if ($currentHash -ne $manifest.graphModuleHash) { throw 'Graph module changed after the safety manifest was written.' }
if ((Get-Content -LiteralPath $graphPath -Raw) -notmatch 'ONEOFF_DELETE_BLOCKED') { throw 'DELETE guard marker is missing.' }

$unsafePatterns = @(
    '(?is)Invoke-RestMethod.{0,300}-Method\s+[''\"]?Delete',
    '(?is)Invoke-WebRequest.{0,300}-Method\s+[''\"]?Delete',
    '(?i)\.DeleteAsync\s*\('
)
$unsafeHits = [System.Collections.Generic.List[string]]::new()
foreach ($file in Get-ChildItem -LiteralPath $runtime -Recurse -File -Include '*.ps1', '*.psm1') {
    $text = Get-Content -LiteralPath $file.FullName -Raw
    foreach ($pattern in $unsafePatterns) {
        if ($text -match $pattern) { $unsafeHits.Add($file.FullName); break }
    }
}
if ($unsafeHits.Count -gt 0) {
    throw "Found a direct HTTP DELETE path outside the guarded Graph method: $($unsafeHits -join ', ')"
}

Import-Module $graphPath -Force
$blocked = $false
try {
    Invoke-YardstickGraphRequest -Resource 'deviceAppManagement/mobileApps/00000000-0000-0000-0000-000000000000' -Method Delete
} catch {
    if ($_.Exception.Message -like '*ONEOFF_DELETE_BLOCKED*') { $blocked = $true } else { throw }
}
if (-not $blocked) { throw 'DELETE guard did not block a synthetic request.' }

[pscustomobject]@{
    RuntimePath  = $runtime
    Safe         = $true
    Proof        = 'Synthetic Graph DELETE blocked before authentication.'
    YardstickDevPath = $manifest.sourcePath
    SourceCommit = $manifest.sourceCommit
}

