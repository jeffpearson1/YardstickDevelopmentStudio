#Requires -Version 7.0
# Clones YardstickDev's committed state into a private runtime with a guard that blocks every
# Graph DELETE. Uncommitted changes in YardstickDev neither block this nor reach the ticket.
# The clone is the ticket's own: the Clean up stage (Remove-OneOffRuntime) deletes it.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$YardstickDevPath,
    [Parameter(Mandatory)][string]$DestinationPath,
    # Only Modules (a sparse checkout): enough for a guarded Graph session, and far quicker
    # where clean/smudge filters run once per file.
    [switch]$ModulesOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$source = (Resolve-Path -LiteralPath $YardstickDevPath -ErrorAction Stop).Path.TrimEnd('\')
try {
    $insideWorkTree = "$(& git -C $source rev-parse --is-inside-work-tree 2>$null)".Trim()
    if ($LASTEXITCODE -ne 0 -or $insideWorkTree -ne 'true') { throw 'Not a Git working tree.' }
    $sourceCommit = "$(& git -C $source rev-parse HEAD 2>$null)".Trim()
    if ($LASTEXITCODE -ne 0 -or $sourceCommit -notmatch '^[0-9a-f]{40}$') { throw 'It has no commit to clone.' }
    $sourceBranch = "$(& git -C $source branch --show-current 2>$null)".Trim()
    $sourceStatus = @(& git -C $source status --porcelain 2>$null)
} catch {
    throw "YardstickDevPath must be a readable Git working tree: $($_.Exception.Message)"
}
$destination = [IO.Path]::GetFullPath($DestinationPath).TrimEnd('\')
if ($destination.Equals($source, [StringComparison]::OrdinalIgnoreCase) -or
    $destination.StartsWith($source + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Destination must not be the Yardstick source or a child of it.'
}
if ((Test-Path -LiteralPath $destination) -and @(Get-ChildItem -LiteralPath $destination -Force).Count -gt 0) {
    throw "Destination already exists and is not empty: $destination"
}

# Git-ignored files the runtime still needs from the source: its preferences, the icon
# library and site-specific recipe data. Copied, never linked.
$localItems = @('preferences.yaml', 'Local\preferences.yaml', 'Icons\*', 'Recipes\*.common.yaml')
$required = @(
    'Yardstick.ps1', 'Yardstick.psd1', 'Yardstick.Project.psd1', 'ProjectLayout.ps1',
    'RecipeGroups.yaml', 'Modules', 'Config', 'Templates', 'Recipes', 'Tests\RecipeHarness'
)
if ($ModulesOnly) { $localItems = @(); $required = @('Modules') }

try {
    # A fresh, independent clone of exactly the checked-out commit.
    $out = & git -c advice.detachedHead=false clone --quiet --no-checkout --no-hardlinks -- $source $destination 2>&1
    if ($LASTEXITCODE -ne 0) { throw "git clone failed: $($out -join ' ')" }
    if ($ModulesOnly) {
        $out = & git -C $destination sparse-checkout set Modules 2>&1
        if ($LASTEXITCODE -ne 0) { throw "git sparse-checkout failed: $($out -join ' ')" }
    }
    $out = & git -C $destination -c advice.detachedHead=false checkout --quiet --detach $sourceCommit 2>&1
    if ($LASTEXITCODE -ne 0) { throw "git checkout of $sourceCommit failed: $($out -join ' ')" }

    foreach ($relative in $required) {
        if (-not (Test-Path -LiteralPath (Join-Path $destination $relative))) { throw "Required Yardstick input is missing from commit $($sourceCommit.Substring(0, 8)): $relative" }
    }

    $copied = [System.Collections.Generic.List[string]]::new()
    foreach ($pattern in $localItems) {
        foreach ($item in @(Get-Item -Path (Join-Path $source $pattern) -Force -ErrorAction SilentlyContinue)) {
            $relative = [IO.Path]::GetRelativePath($source, $item.FullName)
            $target = Join-Path $destination $relative
            if (Test-Path -LiteralPath $target) { continue }
            New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
            Copy-Item -LiteralPath $item.FullName -Destination $target -Recurse -Force
            $copied.Add($relative)
        }
    }
    if (-not $ModulesOnly) { New-Item -ItemType Directory -Path (Join-Path $destination 'Icons') -Force | Out-Null }

    $graphModule = Join-Path $destination 'Modules\YardstickGraph.psm1'
    $content = Get-Content -LiteralPath $graphModule -Raw
    $functionIndex = $content.IndexOf('function Invoke-YardstickGraphRequest', [StringComparison]::Ordinal)
    if ($functionIndex -lt 0) { throw 'Could not find Invoke-YardstickGraphRequest in the cloned Graph module.' }
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

    foreach ($preferenceFile in @((Join-Path $destination 'preferences.yaml'), (Join-Path $destination 'Local\preferences.yaml'))) {
        if (Test-Path -LiteralPath $preferenceFile) {
            $preferenceText = Get-Content -LiteralPath $preferenceFile -Raw
            Set-Content -LiteralPath $preferenceFile -Value $preferenceText.Replace($source, $destination) -Encoding utf8NoBOM
        }
    }

    $manifest = [ordered]@{
        createdUtc               = [DateTime]::UtcNow.ToString('o')
        sourceKind               = 'YardstickDev clone'
        sourcePath               = $source
        sourceBranch             = $sourceBranch
        sourceCommit             = $sourceCommit
        sourceUncommittedChanges = $sourceStatus.Count
        modulesOnly              = [bool]$ModulesOnly
        localFilesCopied         = @($copied)
        graphModuleHash          = (Get-FileHash -LiteralPath $graphModule -Algorithm SHA256).Hash
        deleteGuard              = 'ONEOFF_DELETE_BLOCKED'
    }
    $manifest | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $destination '.oneoff-runtime.json') -Encoding utf8NoBOM
} catch {
    # Never leave a half-made (and possibly unguarded) clone behind.
    if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Recurse -Force -ErrorAction SilentlyContinue }
    throw
}

[pscustomobject]@{
    RuntimePath = $destination; YardstickDevPath = $source; SourceCommit = $sourceCommit
    UncommittedChangesLeftOut = $sourceStatus.Count; DeleteGuardInstalled = $true
}
