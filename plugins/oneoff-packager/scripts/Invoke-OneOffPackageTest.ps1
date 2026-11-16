#Requires -Version 7.0
<#
.SYNOPSIS
Validates one-off Yardstick recipes through the YardstickDev RecipeHarness.

.DESCRIPTION
Prepare mode is the default. It performs schema, download, signature, metadata,
script-generation, and pre-detection checks, but suppresses install/uninstall.

Full mode performs the real install, detection, uninstall, post-uninstall
detection, and residue cycle. It requires -ApproveLocalInstallUninstall.

The wrapper never passes -Force or -SkipUninstall, never contacts Intune, and
writes immutable invocation evidence beneath the selected ticket directory.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$TicketRoot,

    [Parameter(Mandatory)]
    [string[]]$RecipePath,

    [Parameter(Mandatory)]
    [string]$WorkspaceRoot,

    [Parameter(Mandatory)]
    [string]$YardstickDevPath,

    [ValidateSet('Prepare', 'Full')]
    [string]$Mode = 'Prepare',

    [switch]$ApproveLocalInstallUninstall,
    [switch]$IncludeReinstall,
    [switch]$KeepInstaller,

    [ValidateRange(1, 240)]
    [int]$InstallTimeoutMinutes = 60,

    [ValidateRange(0, 900)]
    [int]$DetectionSettleSeconds = 180,

    [ValidateRange(0, 900)]
    [int]$ResidueSettleSeconds = 60,

    [string]$RecipesPath,
    [string]$IconsPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Test-PathWithin {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Root)
    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $boundary = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    return $candidate.Equals($boundary, [StringComparison]::OrdinalIgnoreCase) -or
        $candidate.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Get-FileEvidence {
    param([Parameter(Mandatory)][string]$Path)
    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    [ordered]@{
        path        = $item.FullName
        length      = $item.Length
        lastWriteUtc = $item.LastWriteTimeUtc.ToString('o')
        sha256      = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash
    }
}

$workspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot -ErrorAction Stop).Path.TrimEnd('\')
$ticket = (Resolve-Path -LiteralPath $TicketRoot -ErrorAction Stop).Path
if (-not (Test-PathWithin -Path $ticket -Root $workspaceRoot)) {
    throw "TicketRoot must be inside '$workspaceRoot'."
}

$yardstickDev = (Resolve-Path -LiteralPath $YardstickDevPath -ErrorAction Stop).Path
$harnessPath = Join-Path $yardstickDev 'Tests\RecipeHarness\Invoke-RecipeInstallTest.ps1'
$schemaPath = Join-Path $yardstickDev 'Tests\RecipeHarness\Test-RecipeSchema.ps1'
$supportPath = Join-Path $yardstickDev 'Tests\RecipeHarness\RecipeTestSupport.psm1'
$yardstickSupportPath = Join-Path $yardstickDev 'Modules\YardstickSupport.psm1'
foreach ($requiredPath in @($harnessPath, $schemaPath, $supportPath, $yardstickSupportPath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required YardstickDev harness input is missing: $requiredPath"
    }
}

$recipes = @($RecipePath | ForEach-Object {
    $resolved = (Resolve-Path -LiteralPath $_ -ErrorAction Stop).Path
    if ([IO.Path]::GetExtension($resolved) -notin '.yaml', '.yml') {
        throw "Recipe must be a YAML file: $resolved"
    }
    if (-not (Test-PathWithin -Path $resolved -Root $ticket)) {
        throw "Recipe must be inside the ticket workspace '$ticket': $resolved"
    }
    $resolved
})
if ($recipes.Count -eq 0) { throw 'At least one recipe is required.' }
if (@($recipes | Sort-Object -Unique).Count -ne $recipes.Count) { throw 'RecipePath contains duplicates.' }

if ($RecipesPath) {
    $recipeLookup = (Resolve-Path -LiteralPath $RecipesPath -ErrorAction Stop).Path
} else {
    $ticketRecipes = Join-Path $ticket 'recipes'
    $recipeLookup = if (Test-Path -LiteralPath $ticketRecipes -PathType Container) {
        (Resolve-Path -LiteralPath $ticketRecipes).Path
    } else {
        Split-Path -Parent $recipes[0]
    }
}
if (-not (Test-PathWithin -Path $recipeLookup -Root $ticket)) {
    throw 'RecipesPath must be inside the ticket workspace.'
}
foreach ($recipe in $recipes) {
    if (-not (Test-PathWithin -Path $recipe -Root $recipeLookup)) {
        throw "Every selected recipe must be inside RecipesPath '$recipeLookup': $recipe"
    }
}

if ($IconsPath) {
    $icons = (Resolve-Path -LiteralPath $IconsPath -ErrorAction Stop).Path
    if (-not (Test-PathWithin -Path $icons -Root $ticket) -and
        -not $icons.Equals((Join-Path $yardstickDev 'Icons'), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'IconsPath must be the ticket icons folder or YardstickDev\Icons.'
    }
} else {
    $ticketIcons = Join-Path $ticket 'icons'
    $icons = if (Test-Path -LiteralPath $ticketIcons -PathType Container) {
        (Resolve-Path -LiteralPath $ticketIcons).Path
    } else {
        (Resolve-Path -LiteralPath (Join-Path $yardstickDev 'Icons') -ErrorAction Stop).Path
    }
}

if ($Mode -eq 'Full' -and -not $ApproveLocalInstallUninstall) {
    throw 'Full mode installs and uninstalls software locally. Re-run with -ApproveLocalInstallUninstall after explicit user approval for this test host and recipe set.'
}
if ($Mode -eq 'Prepare' -and $ApproveLocalInstallUninstall) {
    Write-Warning '-ApproveLocalInstallUninstall has no effect in Prepare mode.'
}
if ($Mode -eq 'Full') {
    # An idle Windows Installer service may remain running after a completed transaction.
    # Exclude only its positively identified /V service process, never an unknown client.
    foreach ($transactionKey in @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\InProgress',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Installer\InProgress'
    )) {
        if (Test-Path -LiteralPath $transactionKey -ErrorAction Stop) {
            throw 'Windows Installer has an InProgress transaction. Wait before starting a live package test.'
        }
    }
    $installerMutex = $null
    try {
        $installerMutex = [Threading.Mutex]::OpenExisting('Global\_MSIExecute')
        throw 'Windows Installer transaction mutex exists. Wait before starting a live package test.'
    } catch [Threading.WaitHandleCannotBeOpenedException] {
        # A nonexistent mutex is the only accepted absence result; access failures block.
    } finally {
        if ($installerMutex) { $installerMutex.Dispose() }
    }
    $idleServicePid = 0
    $installerService = Get-CimInstance -ClassName Win32_Service -Filter "Name = 'msiserver'" -ErrorAction Stop
    if ($installerService -and $installerService.State -eq 'Running' -and [long]$installerService.ProcessId -gt 0) {
        $serviceProcess = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $([long]$installerService.ProcessId)" -ErrorAction Stop
        $expectedServiceImage = Join-Path $env:windir 'System32\msiexec.exe'
        $serviceModePattern = '^\s*"?' + [regex]::Escape($expectedServiceImage) + '"?\s+/V\s*$'
        if ($serviceProcess -and $serviceProcess.Name -ieq 'msiexec.exe' -and
            (-not $serviceProcess.ExecutablePath -or $serviceProcess.ExecutablePath -ieq $expectedServiceImage) -and
            (-not $serviceProcess.CommandLine -or $serviceProcess.CommandLine -imatch $serviceModePattern) -and
            $installerService.PathName -imatch $serviceModePattern) {
            $idleServicePid = [long]$installerService.ProcessId
        }
    }
    $activeInstaller = @(Get-Process -Name msiexec -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $idleServicePid })
    if ($activeInstaller.Count -gt 0) {
        throw "Windows Installer is already active (PID(s): $($activeInstaller.Id -join ', ')). Wait for it to finish before starting a live package test."
    }
}

$runId = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$runRoot = Join-Path $ticket "artifacts\package-tests\$runId"
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
$recipeListPath = Join-Path $runRoot 'recipe-paths.txt'
$recipes | Set-Content -LiteralPath $recipeListPath -Encoding utf8NoBOM

$yardstickCommit = $null
$yardstickStatus = @()
try {
    $yardstickCommit = (& git -C $yardstickDev rev-parse HEAD 2>$null).Trim()
    $yardstickStatus = @(& git -C $yardstickDev status --short 2>$null)
} catch {
    $yardstickCommit = $null
    $yardstickStatus = @('Git provenance unavailable')
}

$manifestPath = Join-Path $runRoot 'invocation.json'
$manifest = [ordered]@{
    schemaVersion = 1
    runId = $runId
    startedUtc = [DateTime]::UtcNow.ToString('o')
    mode = $Mode
    approvedLocalInstallUninstall = [bool]$ApproveLocalInstallUninstall
    includeReinstall = [bool]$IncludeReinstall
    discardInstaller = -not [bool]$KeepInstaller
    ticketRoot = $ticket
    recipesPath = $recipeLookup
    iconsPath = $icons
    recipes = @($recipes | ForEach-Object { Get-FileEvidence -Path $_ })
    yardstickDev = [ordered]@{
        path = $yardstickDev
        commit = $yardstickCommit
        workingTreeStatus = $yardstickStatus
        harness = Get-FileEvidence -Path $harnessPath
        schema = Get-FileEvidence -Path $schemaPath
        recipeTestSupport = Get-FileEvidence -Path $supportPath
        yardstickSupport = Get-FileEvidence -Path $yardstickSupportPath
    }
    settings = [ordered]@{
        installTimeoutMinutes = $InstallTimeoutMinutes
        detectionSettleSeconds = $DetectionSettleSeconds
        residueSettleSeconds = $ResidueSettleSeconds
    }
    forbiddenHarnessSwitches = @('Force', 'SkipUninstall')
    completedUtc = $null
    harnessExitCode = $null
    summaryPaths = @()
}
$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8NoBOM

$childArgs = [Collections.Generic.List[string]]::new()
$childArgs.AddRange([string[]]@(
    '-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $harnessPath,
    '-RecipeListFile', $recipeListPath,
    '-YardstickDevPath', $yardstickDev,
    '-RecipesPath', $recipeLookup,
    '-IconsPath', $icons,
    '-ArtifactsPath', $runRoot,
    '-InstallTimeoutMinutes', [string]$InstallTimeoutMinutes,
    '-DetectionSettleSeconds', [string]$DetectionSettleSeconds,
    '-ResidueSettleSeconds', [string]$ResidueSettleSeconds
))
if ($Mode -eq 'Prepare') {
    $childArgs.Add('-WhatIf')
    $childArgs.Add('-NoElevate')
}
if ($IncludeReinstall) { $childArgs.Add('-IncludeReinstall') }
if (-not $KeepInstaller) { $childArgs.Add('-DiscardInstaller') }

$hostPath = (Get-Process -Id $PID).Path
Write-Host "Package validation mode: $Mode" -ForegroundColor Cyan
Write-Host "Recipes: $($recipes -join ', ')"
Write-Host "Evidence: $runRoot"
& $hostPath @childArgs
$exitCode = $LASTEXITCODE

$summaryPaths = @(Get-ChildItem -LiteralPath $runRoot -Recurse -File -Filter 'summary.json' -ErrorAction SilentlyContinue |
    Sort-Object FullName |
    Select-Object -ExpandProperty FullName)
$manifest.completedUtc = [DateTime]::UtcNow.ToString('o')
$manifest.harnessExitCode = $exitCode
$manifest.summaryPaths = $summaryPaths
$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8NoBOM

$result = [pscustomobject]@{
    RunId = $runId
    Mode = $Mode
    ExitCode = $exitCode
    Passed = ($exitCode -eq 0)
    EvidenceRoot = $runRoot
    InvocationManifest = $manifestPath
    SummaryPaths = $summaryPaths
}
$result
if ($exitCode -ne 0) { exit $exitCode }




