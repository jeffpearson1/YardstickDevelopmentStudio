#Requires -Version 7.0
[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)][string]$YardstickDevPath,
    [Parameter(Mandatory)][string]$DestinationPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$source = (Resolve-Path -LiteralPath $YardstickDevPath -ErrorAction Stop).Path.TrimEnd('\')
try {
    $insideWorkTree = (& git -C $source rev-parse --is-inside-work-tree 2>$null).Trim()
    if ($LASTEXITCODE -ne 0 -or $insideWorkTree -ne 'true') { throw 'Not a Git working tree.' }
    $sourceCommit = (& git -C $source rev-parse HEAD 2>$null).Trim()
    $sourceBranch = (& git -C $source branch --show-current 2>$null).Trim()
    $sourceStatus = @(& git -C $source status --porcelain --untracked-files=all 2>$null)
    if ($LASTEXITCODE -ne 0) { throw 'Could not read Git status.' }
} catch {
    throw "YardstickDevPath must be a readable Git working tree: $($_.Exception.Message)"
}
if ($sourceStatus.Count -gt 0) {
    throw "YardstickDevPath must have a clean working tree before runtime creation: $($sourceStatus -join '; ')"
}
$destination = [IO.Path]::GetFullPath($DestinationPath).TrimEnd('\')
if ($destination.Equals($source, [StringComparison]::OrdinalIgnoreCase) -or
    $destination.StartsWith($source + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Destination must not be the Yardstick source or a child of it.'
}
if (Test-Path -LiteralPath $destination) {
    if (@(Get-ChildItem -LiteralPath $destination -Force).Count -gt 0) {
        throw "Destination already exists and is not empty: $destination"
    }
} elseif ($PSCmdlet.ShouldProcess($destination, 'Create isolated Yardstick runtime directory')) {
    New-Item -ItemType Directory -Path $destination | Out-Null
}

$required = @(
    'Yardstick.ps1', 'Yardstick.psd1', 'Yardstick.Project.psd1', 'ProjectLayout.ps1',
    'RecipeGroups.yaml', 'Modules', 'Config', 'Templates',
    'Recipes', 'Icons', 'Tests\RecipeHarness'
)
$optional = @('preferences.yaml', 'Local\preferences.yaml', 'Branding', 'Docs')

foreach ($relative in $required) {
    $item = Join-Path $source $relative
    if (-not (Test-Path -LiteralPath $item)) { throw "Required Yardstick input is missing: $item" }
}

foreach ($relative in ($required + $optional)) {
    $item = Join-Path $source $relative
    if (-not (Test-Path -LiteralPath $item)) { continue }
    $target = Join-Path $destination $relative
    $parent = Split-Path -Parent $target
    if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    if ($PSCmdlet.ShouldProcess($item, "Copy to isolated runtime $destination")) {
        Copy-Item -LiteralPath $item -Destination $parent -Recurse -Force
    }
}

$graphModule = Join-Path $destination 'Modules\YardstickGraph.psm1'
$content = Get-Content -LiteralPath $graphModule -Raw
$functionIndex = $content.IndexOf('function Invoke-YardstickGraphRequest', [StringComparison]::Ordinal)
if ($functionIndex -lt 0) { throw 'Could not find Invoke-YardstickGraphRequest in the copied Graph module.' }
$marker = '$expiresOn = Get-YardstickGraphTokenExpiry'
$insertIndex = $content.IndexOf($marker, $functionIndex, [StringComparison]::Ordinal)
if ($insertIndex -lt 0) { throw 'Could not locate the Graph request function body; refusing to create an unguarded runtime.' }
$guard = @"
if (`$Method -eq 'Delete') {
        throw 'ONEOFF_DELETE_BLOCKED: HTTP DELETE is prohibited by the Intune Ticket Packager runtime.'
    }

    
"@
$content = $content.Insert($insertIndex, $guard)
Set-Content -LiteralPath $graphModule -Value $content -Encoding utf8NoBOM

foreach ($preferenceFile in @(
    (Join-Path $destination 'preferences.yaml'),
    (Join-Path $destination 'Local\preferences.yaml')
)) {
    if (Test-Path -LiteralPath $preferenceFile) {
        $preferenceText = Get-Content -LiteralPath $preferenceFile -Raw
        $preferenceText = $preferenceText.Replace($source, $destination)
        Set-Content -LiteralPath $preferenceFile -Value $preferenceText -Encoding utf8NoBOM
    }
}

$manifest = [ordered]@{
    createdUtc      = [DateTime]::UtcNow.ToString('o')
    sourceKind      = 'YardstickDev'
    sourcePath      = $source
    sourceBranch    = $sourceBranch
    sourceCommit    = $sourceCommit
    sourceWasClean  = $true
    graphModuleHash = (Get-FileHash -LiteralPath $graphModule -Algorithm SHA256).Hash
    deleteGuard     = 'ONEOFF_DELETE_BLOCKED'
}
$manifest | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $destination '.oneoff-runtime.json') -Encoding utf8NoBOM

[pscustomobject]@{ RuntimePath = $destination; YardstickDevPath = $source; SourceCommit = $sourceCommit; DeleteGuardInstalled = $true }





