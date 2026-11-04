<#
.SYNOPSIS
Builds the Yardstick Development Studio MSI from an existing release build.

.DESCRIPTION
Called by `.\build.ps1 -Task installer`, which builds the release executable first.
Uses WiX Toolset 5.0.2 (MS-RL). If wix.exe 5.x is not on PATH, the pinned WiX CLI MSI
is downloaded, its SHA-256 checked, and it is unpacked into .tools\ with an
administrative extract (no admin rights, nothing installed system-wide).
WiX 6 and later carry the Open Source Maintenance Fee, so the version is pinned.

.OUTPUTS
target\installer\YardstickDevelopmentStudio-<version>-x64.msi
#>
[CmdletBinding()]
param(
    # Use a specific wix.exe instead of locating or downloading one.
    [string]$WixPath
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$wixVersion = '5.0.2'
$wixMsiUrl = "https://github.com/wixtoolset/wix/releases/download/v$wixVersion/wix-cli-x64.msi"
$wixMsiSha256 = '097383B2773BDD3D76A6A2CD3F4DE6357BAE59172E1B6FA5AD87CE062074E8D0'
$toolsDir = Join-Path $root ".tools\wix-$wixVersion"

function Find-Wix {
    if ($WixPath) { return (Resolve-Path $WixPath).Path }
    $onPath = Get-Command wix.exe -ErrorAction SilentlyContinue
    if ($onPath -and ((& $onPath.Source --version) -like '5.*')) { return $onPath.Source }
    $local = Get-ChildItem $toolsDir -Recurse -Filter wix.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($local) { return $local.FullName }

    Write-Host "Downloading WiX Toolset $wixVersion..."
    $download = Join-Path $root '.tools\download'
    New-Item -ItemType Directory -Force $download | Out-Null
    $msi = Join-Path $download "wix-cli-$wixVersion-x64.msi"
    Invoke-WebRequest $wixMsiUrl -OutFile $msi -UseBasicParsing
    $hash = (Get-FileHash $msi -Algorithm SHA256).Hash
    if ($hash -ne $wixMsiSha256) {
        Remove-Item $msi
        throw "WiX download hash mismatch: expected $wixMsiSha256, got $hash."
    }
    New-Item -ItemType Directory -Force $toolsDir | Out-Null
    $p = Start-Process msiexec.exe -ArgumentList '/a', "`"$msi`"", '/qn', "TARGETDIR=`"$toolsDir`"" -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "Extracting WiX failed (msiexec exit $($p.ExitCode))." }
    $local = Get-ChildItem $toolsDir -Recurse -Filter wix.exe | Select-Object -First 1
    if (-not $local) { throw 'wix.exe not found after extracting the WiX MSI.' }
    return $local.FullName
}

function ConvertTo-Rtf([string]$text) {
    $escaped = $text -replace '\\', '\\' -replace '\{', '\{' -replace '\}', '\}'
    $paragraphs = ($escaped -replace "`r`n", "`n").Trim() -split "`n`n"
    $body = ($paragraphs | ForEach-Object { ($_ -replace "`n", ' ') + '\par' }) -join "`r`n\par`r`n"
    return "{\rtf1\ansi\deff0{\fonttbl{\f0 Segoe UI;}}\viewkind4\uc1\pard\f0\fs18`r`n$body`r`n}"
}

Push-Location $root
try {
    $exe = Join-Path $root 'target\release\yardstick-studio.exe'
    if (-not (Test-Path $exe)) { throw "Release build not found at $exe. Run .\build.ps1 -Task installer." }

    $cargo = Get-Content (Join-Path $root 'Cargo.toml') -Raw
    if ($cargo -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(\d+)\.(\d+)\.(\d+)') {
        throw 'Could not read [workspace.package] version from Cargo.toml.'
    }
    $version = "$($Matches[1]).$($Matches[2]).$($Matches[3])"

    $wix = Find-Wix
    Write-Host "Using $wix ($(& $wix --version))"
    # Extensions resolve from .wix\extensions under the working directory.
    if (-not ((& $wix extension list) -match "WixToolset.UI.wixext $wixVersion")) {
        & $wix extension add "WixToolset.UI.wixext/$wixVersion" | Out-Host
        if ($LASTEXITCODE) { throw 'Could not add the WiX UI extension.' }
    }

    $outDir = Join-Path $root 'target\installer'
    $objDir = Join-Path $outDir 'obj'
    New-Item -ItemType Directory -Force $objDir | Out-Null
    $licenseRtf = Join-Path $objDir 'License.rtf'
    Set-Content $licenseRtf (ConvertTo-Rtf (Get-Content (Join-Path $root 'LICENSE') -Raw)) -Encoding ascii

    $msi = Join-Path $outDir "YardstickDevelopmentStudio-$version-x64.msi"
    & $wix build (Join-Path $PSScriptRoot 'Product.wxs') `
        -arch x64 `
        -ext WixToolset.UI.wixext `
        -d "Version=$version" `
        -d "Exe=$exe" `
        -d "RepoRoot=$root" `
        -d "Assets=$(Join-Path $root 'assets\branding')" `
        -d "InstallerDir=$PSScriptRoot" `
        -d "LicenseRtf=$licenseRtf" `
        -intermediateFolder $objDir `
        -o $msi | Out-Host
    if ($LASTEXITCODE) { throw "wix build failed ($LASTEXITCODE)." }
    Remove-Item (Join-Path $outDir '*.wixpdb') -ErrorAction SilentlyContinue

    $size = '{0:N1} MB' -f ((Get-Item $msi).Length / 1MB)
    Write-Host "Built $msi ($size)"
    Write-Host "SHA-256 $((Get-FileHash $msi -Algorithm SHA256).Hash)"
}
finally { Pop-Location }
