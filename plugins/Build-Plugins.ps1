<#
.SYNOPSIS
Packs each plugin under plugins\ into <id>-<version>.zip with a .sha256 beside it.

.DESCRIPTION
The same folder always gives the same zip: files are added in name order with a fixed
timestamp and text files use LF line endings, so a checkout with CRLF endings packs the
same as one with LF. The zip is what the Studio installs and what a release publishes.
Tests, dot-folders and the install record are left out.

Release: tag plugin-<id>-v<version> and attach the zip, with
    gh release create plugin-<id>-v<version> target\plugins\<id>-<version>.zip --latest=false
--latest=false matters: the Studio's own update check reads the latest release and
expects an MSI there.
#>
param(
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\target\plugins')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$textExtensions = '.ps1', '.psm1', '.psd1', '.md', '.json', '.txt', '.yaml', '.yml', '.xml'
$allowed = $textExtensions + '.png', '.ico', '.svg'
$stamp = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)

function Get-ManifestValue([string]$Manifest, [string]$Key) {
    $m = [regex]::Match((Get-Content -LiteralPath $Manifest -Raw), "(?m)^${Key}:\s*(\S+)\s*$")
    if (-not $m.Success) { throw "$Manifest has no top-level '${Key}:'." }
    $m.Groups[1].Value.Trim('"', "'")
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path
$built = @()
foreach ($dir in Get-ChildItem -LiteralPath $PSScriptRoot -Directory | Sort-Object Name) {
    $manifest = Join-Path $dir.FullName 'plugin.yaml'
    if (-not (Test-Path -LiteralPath $manifest)) { continue }
    $id = Get-ManifestValue $manifest 'id'
    $version = Get-ManifestValue $manifest 'version'
    if ($id -ne $dir.Name) { throw "plugins\$($dir.Name) holds plugin '$id'; the folder must be named for the id." }

    $root = $dir.FullName.TrimEnd('\')
    $files = Get-ChildItem -LiteralPath $root -Recurse -File | ForEach-Object {
        $rel = $_.FullName.Substring($root.Length + 1).Replace('\', '/')
        [pscustomobject]@{ Path = $_.FullName; Name = $rel }
    } | Where-Object {
        $top = $_.Name.Split('/')[0]
        $top -ne 'tests' -and -not $top.StartsWith('.') -and $_.Name -ne '.installed.json'
    } | Sort-Object { $_.Name } -CaseSensitive

    foreach ($f in $files) {
        $ext = [IO.Path]::GetExtension($f.Name).ToLowerInvariant()
        if ($allowed -notcontains $ext) { throw "plugins\$id\$($f.Name): the Studio only installs $($allowed -join ' ') files." }
    }

    $zipPath = Join-Path $OutputDirectory "$id-$version.zip"
    Remove-Item -LiteralPath $zipPath -ErrorAction SilentlyContinue
    $fs = [IO.File]::Open($zipPath, [IO.FileMode]::CreateNew)
    try {
        $zip = [IO.Compression.ZipArchive]::new($fs, [IO.Compression.ZipArchiveMode]::Create)
        try {
            foreach ($f in $files) {
                $bytes = [IO.File]::ReadAllBytes($f.Path)
                if ($textExtensions -contains [IO.Path]::GetExtension($f.Name).ToLowerInvariant()) {
                    $bytes = [Text.Encoding]::GetEncoding(28591).GetBytes(
                        [Text.Encoding]::GetEncoding(28591).GetString($bytes).Replace("`r`n", "`n"))
                }
                $entry = $zip.CreateEntry($f.Name, [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = $stamp
                $out = $entry.Open()
                try { $out.Write($bytes, 0, $bytes.Length) } finally { $out.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $fs.Dispose() }

    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash *$([IO.Path]::GetFileName($zipPath))" | Set-Content -LiteralPath "$zipPath.sha256" -Encoding ascii
    Write-Host "$id $version -> $zipPath ($($files.Count) files)"
    Write-Host "  SHA-256 $hash"
    $built += $zipPath
}
if (-not $built) { Write-Host 'No plugins found under plugins\.' }
