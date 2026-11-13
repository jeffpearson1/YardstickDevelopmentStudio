<#
.SYNOPSIS
Builds, tests or runs Yardstick Development Studio with the Rust toolchain on PATH.

.EXAMPLE
.\build.ps1             # release build -> target\release\yardstick-studio.exe
.\build.ps1 -Task test  # run the unit tests
.\build.ps1 -Task run   # build and launch (debug)
.\build.ps1 -Task installer  # release build + MSI -> target\installer\
.\build.ps1 -Task plugins    # plugin zips + SHA-256 -> target\plugins\
.\build.ps1 -Task plugin-test  # Pester tests of the plugins (needs Pester 5)
#>
param(
    [ValidateSet('build', 'test', 'run', 'check', 'clippy', 'installer', 'plugins', 'plugin-test')]
    [string]$Task = 'build',
    [switch]$Debug,
    # Extra arguments passed through to cargo, e.g. -CargoArgs '--', '--ignored'
    [string[]]$CargoArgs = @()
)
$ErrorActionPreference = 'Stop'

# Plugins are scripts and text, so these tasks need no Rust toolchain.
if ($Task -eq 'plugins') {
    & (Join-Path $PSScriptRoot 'plugins\Build-Plugins.ps1') -OutputDirectory (Join-Path $PSScriptRoot 'target\plugins')
    exit $LASTEXITCODE
}
if ($Task -eq 'plugin-test') {
    if (-not (Get-Module -ListAvailable Pester | Where-Object { $_.Version.Major -ge 5 })) {
        throw 'Pester 5 or later is needed: Install-Module Pester -MinimumVersion 5.0 -Scope CurrentUser'
    }
    Import-Module Pester -MinimumVersion 5.0
    $tests = @(Get-ChildItem (Join-Path $PSScriptRoot 'plugins') -Directory -ErrorAction SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName 'tests' } | Where-Object { Test-Path $_ })
    if (-not $tests) { Write-Host 'No plugin tests.'; exit 0 }
    $result = Invoke-Pester -Path $tests -Output Detailed -PassThru
    exit ([int]($result.FailedCount -gt 0 -or $result.FailedContainersCount -gt 0))
}

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found. Install Rust from https://rustup.rs (the x86_64-pc-windows-gnu host works without Visual Studio).'
}
# The GNU host links windows-sys with raw-dylib, which needs MinGW's dlltool and
# assembler. rustup's self-contained dlltool only looks for `as` beside itself
# (and none ships), so point rustc at a full MinGW-w64 instead
# (winget install BrechtSanders.WinLibs.POSIX.MSVCRT --scope user).
$hostTriple = (& rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()
if ($hostTriple -like '*windows-gnu*') {
    $mingwBin = Get-Command as.exe -ErrorAction SilentlyContinue | ForEach-Object { Split-Path $_.Source } | Select-Object -First 1
    if (-not $mingwBin) {
        $mingwBin = Get-ChildItem (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages') -Directory -Filter 'BrechtSanders.WinLibs*' -ErrorAction SilentlyContinue |
            ForEach-Object { Join-Path $_.FullName 'mingw64\bin' } |
            Where-Object { Test-Path (Join-Path $_ 'as.exe') } |
            Select-Object -First 1
    }
    if (-not $mingwBin) {
        throw 'The windows-gnu Rust toolchain needs MinGW-w64 binutils. Run: winget install BrechtSanders.WinLibs.POSIX.MSVCRT --scope user'
    }
    $env:PATH = "$mingwBin;$env:PATH"
    $env:RUSTFLAGS = "-C dlltool=$(Join-Path $mingwBin 'dlltool.exe') $env:RUSTFLAGS".Trim()
}
Push-Location $PSScriptRoot
try {
    [string[]]$profileArgs = if ($Debug -or $Task -eq 'run') { @() } else { @('--release') }
    switch ($Task) {
        'build'  { cargo build @profileArgs @CargoArgs }
        'test'   { cargo test --workspace @CargoArgs }
        'run'    { cargo run -p ystudio @CargoArgs }
        'check'  { cargo check --workspace @CargoArgs }
        'clippy' { cargo clippy --workspace --all-targets @CargoArgs -- -D warnings }
        'installer' {
            cargo build --release @CargoArgs
            if ($LASTEXITCODE) { exit $LASTEXITCODE }
            & (Join-Path $PSScriptRoot 'installer\build-installer.ps1')
        }
    }
    exit $LASTEXITCODE
}
finally { Pop-Location }
