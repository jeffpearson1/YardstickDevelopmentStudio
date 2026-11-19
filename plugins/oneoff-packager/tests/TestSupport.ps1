#Requires -Version 7.0
# Helpers shared by the tests: a temporary work item, a fake runtime, a fake Graph and a change set.
Set-StrictMode -Version Latest

$script:Plugin = Split-Path -Parent $PSScriptRoot

function Get-PluginRoot { $script:Plugin }

function New-TestWorkItem {
    $root = Join-Path ([IO.Path]::GetTempPath()) ("oneoff-test-" + [guid]::NewGuid().ToString('N'))
    $item = Join-Path $root 'work\2026-11-13-test'
    foreach ($dir in 'recipes', 'icons', 'artifacts', 'inventory', 'runtime\Modules') { New-Item -ItemType Directory -Path (Join-Path $item $dir) -Force | Out-Null }
    [pscustomobject]@{ Root = $root; WorkItem = $item; WorkRoot = (Split-Path $item -Parent) }
}

function Remove-TestWorkItem { param($Item) Remove-Item -LiteralPath $Item.Root -Recurse -Force -ErrorAction SilentlyContinue }

function Set-TestJson { param([string]$Path, $Value, [int]$Depth = 15)
    $parent = Split-Path $Path -Parent
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    ConvertTo-Json -InputObject $Value -Depth $Depth | Set-Content -LiteralPath $Path -Encoding utf8NoBOM
}

function Hash { param($Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }

function Install-FakeRuntimeModules {
    # The two runtime modules the packaging code imports, with just the functions it calls.
    param($Item)
    $modules = Join-Path $Item.WorkItem 'runtime\Modules'
    Set-Content -LiteralPath (Join-Path $modules 'YardstickSupport.psm1') -Encoding utf8NoBOM -Value @'
function Add-NativeBitnessGuard { param([string]$ScriptContent) "# guarded`n$ScriptContent" }
Export-ModuleMember -Function Add-NativeBitnessGuard
'@
    Set-Content -LiteralPath (Join-Path $modules 'YardstickIntune.psm1') -Encoding utf8NoBOM -Value @'
function New-YardstickWin32AppPackage {
    param([string]$SourceFolder, [string]$SetupFile, [string]$OutputFolder, [switch]$Force, [int]$TimeoutMinutes)
    $path = Join-Path $OutputFolder "$([IO.Path]::GetFileNameWithoutExtension($SetupFile)).intunewin"
    Set-Content -LiteralPath $path -Value "package of $SetupFile with $((Get-ChildItem $SourceFolder -Name) -join ',')"
    [pscustomobject]@{ Name = (Split-Path $path -Leaf); Path = $path }
}
Export-ModuleMember -Function New-YardstickWin32AppPackage
'@
}

function New-TestRecipe {
    param($Item, [string]$Id, [string]$Version = '1.0', [string]$Extra = '')
    $path = Join-Path $Item.WorkItem "recipes\$Id.yaml"
    Set-Content -LiteralPath $path -Encoding utf8NoBOM -Value @"
id: $Id
version: '$Version'
displayName: Test App $Id
publisher: Test Publisher
description: A test application.
installScript: <filename> /S
powerShellUninstallScript: |
  Start-Process -FilePath "`$env:ProgramFiles\Test\unins.exe" -ArgumentList '/S' -Wait
detectionType: script
detectionScript: |
  if (Test-Path "`$env:ProgramFiles\Test\app.exe") { 'Detected'; exit 0 } else { exit 1 }
architecture: x64
minOSVersion: W10_20H2
$Extra
"@
    $path
}

function New-TestFullRun {
    # A harness run: invocation.json plus a summary with one result per recipe.
    param($Item, [string]$Mode, [string]$RunId, [object[]]$Recipes, [string]$Overall = 'Pass', [hashtable]$Installer = @{})
    $run = Join-Path $Item.WorkItem "artifacts\package-tests\$RunId"
    $summaryDir = Join-Path $run '_install-tests\20261113-120000'
    New-Item -ItemType Directory -Path $summaryDir -Force | Out-Null
    $results = foreach ($recipe in $Recipes) {
        $id = [IO.Path]::GetFileNameWithoutExtension($recipe)
        $file = Join-Path $summaryDir "$id-setup.exe"
        Set-Content -LiteralPath $file -Value "installer for $id"
        [ordered]@{
            Id = $id; Version = '1.0'; FileName = "$id-setup.exe"; Installer = $file; SHA256 = (Hash $file)
            SignatureStatus = 'Valid'; ProductCode = $null; InstallCommand = "$id-setup.exe /S"; UninstallCommand = 'uninstall'
            Overall = $Overall
        }
    }
    $summaryPath = Join-Path $summaryDir 'summary.json'
    Set-TestJson -Path $summaryPath -Value ([ordered]@{ Results = @($results) })
    Set-TestJson -Path (Join-Path $run 'invocation.json') -Value ([ordered]@{
        schemaVersion = 1; runId = $RunId; mode = $Mode
        recipes = @($Recipes | ForEach-Object { [ordered]@{ path = $_; sha256 = (Hash $_) } })
        summaryPaths = @($summaryPath)
    })
    $summaryPath
}

# ---- fake Graph -----------------------------------------------------------------------

function New-FakeGraph {
    param([string]$TenantId = '11111111-1111-1111-1111-111111111111', [string]$GroupId = '22222222-2222-2222-2222-222222222222')
    $global:FakeGraph = [ordered]@{
        Tenant = [pscustomobject]@{ id = $TenantId; displayName = 'Contoso' }
        Groups = @{ $GroupId = [pscustomobject]@{ id = $GroupId; displayName = 'Testing Group'; groupTypes = @(); securityEnabled = $true; membershipRule = $null } }
        Apps = [ordered]@{}
        Assignments = @{}
        Calls = [System.Collections.Generic.List[string]]::new()
        FailUploadFor = $null
        RestartBehaviorReported = $null
    }
    $global:FakeGraph
}

function Register-FakeGraphMocks {
    # Pester 5 mocks of the Yardstick commands as the OneOffPackager module sees them.
    $handler = {
        param($Method, $Resource, $ApiVersion, $MaxAttempts)
        $g = $global:FakeGraph
        $g.Calls.Add("$Method $Resource")
        if ($Method -eq 'Delete') { throw 'A DELETE reached Graph.' }
        if ($Resource -match '^groups/([0-9a-fA-F-]+)\?') { return $g.Groups[$Matches[1]] }
        if ($Resource -like 'organization*') { return @($g.Tenant) }
        if ($Resource -match '^deviceAppManagement/mobileApps\?') {
            return @($g.Apps.Values | ForEach-Object { [pscustomobject]@{ id = $_.id; displayName = $_.displayName; publisher = $_.publisher; isAssigned = $false } })
        }
        if ($Resource -match '^deviceAppManagement/mobileApps/([^/?]+)/assignments') { return @($g.Assignments[$Matches[1]]) | Where-Object { $_ } }
        if ($Resource -match '/relationships') { return @() }
        if ($Resource -match '/files') { return @([pscustomobject]@{ uploadState = 'commitFileSuccess'; isCommitted = $true }) }
        if ($Resource -match '^deviceAppManagement/mobileApps/([^/?]+)$') {
            $app = $g.Apps[$Matches[1]]
            if (-not $app) { throw "No app $($Matches[1])" }
            return $app
        }
        throw "Unexpected Graph request: $Method $Resource"
    }
    Mock -ModuleName OneOffPackager Invoke-YardstickGraphRequest $handler
    Mock -ModuleName OneOffPackager Add-YardstickWin32App {
        $g = $global:FakeGraph
        if ($g.FailUploadFor -and $DisplayName -eq $g.FailUploadFor) { throw 'Upload failed.' }
        $id = [guid]::NewGuid().ToString()
        $restart = if ($g.RestartBehaviorReported) { $g.RestartBehaviorReported } else { $RestartBehavior }
        $g.Apps[$id] = [pscustomobject]@{
            id = $id; displayName = $DisplayName; displayVersion = $AppVersion; publisher = $Publisher
            installCommandLine = $InstallCommandLine; uninstallCommandLine = $UninstallCommandLine
            allowAvailableUninstall = [bool]$AllowAvailableUninstall
            installExperience = [pscustomobject]@{ runAsAccount = $InstallExperience; deviceRestartBehavior = $restart }
            allowedArchitectures = $RequirementRule.allowedArchitectures
            minimumSupportedWindowsRelease = $RequirementRule.minimumSupportedWindowsRelease
            publishingState = 'published'; uploadState = 1; committedContentVersion = '1'
            detectionRules = @($DetectionRule | ForEach-Object { [pscustomobject]$_ })
        }
        $g.Calls.Add("UPLOAD $DisplayName")
        [pscustomobject]@{ id = $id }
    }
    Mock -ModuleName OneOffPackager Add-YardstickWin32AppAssignmentGroup {
        $g = $global:FakeGraph
        $g.Calls.Add("ASSIGN $ID $GroupID $Intent")
        $assignment = [pscustomobject]@{
            id = [guid]::NewGuid().ToString(); intent = $Intent
            target = [pscustomobject]@{ '@odata.type' = '#microsoft.graph.groupAssignmentTarget'; groupId = $GroupID; deviceAndAppManagementAssignmentFilterType = 'none'; deviceAndAppManagementAssignmentFilterId = $null }
            settings = [pscustomobject]@{ notifications = $Notification }
        }
        $g.Assignments[$ID] = @($g.Assignments[$ID]) + $assignment | Where-Object { $_ }
        $assignment
    }
}

function New-TestChangeSet {
    # A consistent approved change: files, hashes, parameters and the change set itself.
    param($Item, [string[]]$Ids = @('app-a'), [string]$TenantId = '11111111-1111-1111-1111-111111111111', [string]$GroupId = '22222222-2222-2222-2222-222222222222', [string]$GroupName = 'Testing Group')
    $output = Join-Path $Item.WorkItem 'intune-upload'
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    $apps = foreach ($id in $Ids) {
        $files = @{}
        foreach ($rel in "local-packages\$id\1.0\$id-setup.intunewin", "recipes\$id.yaml", "artifacts\$id-summary.json") {
            $full = Join-Path $Item.WorkItem $rel
            New-Item -ItemType Directory -Path (Split-Path $full -Parent) -Force | Out-Null
            Set-Content -LiteralPath $full -Value "$id $rel"
            $files[$rel] = $full
        }
        $detection = Join-Path $output "$id-detection.ps1"
        Set-Content -LiteralPath $detection -Value "Write-Output 'Detected for $id'"
        $parameters = [ordered]@{
            FilePath = "local-packages\$id\1.0\$id-setup.intunewin"
            DisplayName = "Test App $id"; Description = 'A test application.'; Publisher = 'Test Publisher'; AppVersion = '1.0'
            InstallCommandLine = "$id-setup.exe /S"; UninstallCommandLine = 'powershell.exe -file .\uninstall.ps1'
            DetectionRule = [ordered]@{ '@odata.type' = '#microsoft.graph.win32LobAppPowerShellScriptDetection'; enforceSignatureCheck = $false; runAs32Bit = $false; scriptContent = 'QUJD' }
            RequirementRule = [ordered]@{ allowedArchitectures = 'x64'; applicableArchitectures = 'none'; minimumSupportedWindowsRelease = '2H20' }
            InstallExperience = 'system'; RestartBehavior = 'suppress'; MaximumInstallationTimeInMinutes = 60; AllowAvailableUninstall = $true
        }
        $parameterPath = Join-Path $output "$id-upload-parameters.json"
        Set-TestJson -Path $parameterPath -Value $parameters
        [ordered]@{
            recipeId = $id; displayName = "Test App $id"; version = '1.0'; publisher = 'Test Publisher'
            searchPatterns = @("test\s*app\s*$id")
            sourceSha256 = 'AA'; packageFile = "local-packages\$id\1.0\$id-setup.intunewin"; packageSha256 = (Hash $files["local-packages\$id\1.0\$id-setup.intunewin"])
            recipeFile = "recipes\$id.yaml"; recipeSha256 = (Hash $files["recipes\$id.yaml"])
            testEvidence = "artifacts\$id-summary.json"; testEvidenceSha256 = (Hash $files["artifacts\$id-summary.json"]); testOutcome = 'Pass'
            parameters = "intune-upload\$id-upload-parameters.json"; parametersSha256 = (Hash $parameterPath)
            detectionScript = "intune-upload\$id-detection.ps1"; detectionScriptSha256 = (Hash $detection)
            installCommand = "$id-setup.exe /S"; uninstallCommand = 'powershell.exe -file .\uninstall.ps1'; warning = $null
            assignment = [ordered]@{ intent = 'available'; targetType = '#microsoft.graph.groupAssignmentTarget'; groupId = $GroupId; groupDisplayName = $GroupName; groupType = 'static security group'; filters = @(); exclusions = @() }
        }
    }
    $change = [ordered]@{
        preparedUtc = '2026-11-13T00:00:00Z'; tenantId = $TenantId; tenantName = 'Contoso'; intent = 'available'; notifications = 'showAll'
        allowAvailableUninstall = $true; noDelete = $true; apps = @($apps)
    }
    $changePath = Join-Path $output 'live-change-set.json'
    Set-TestJson -Path $changePath -Value $change
    $changePath
}

function Invoke-TestStep {
    # Runs a step script the way the Studio does and returns its exit code and result.
    param($Item, [string]$Script, [string]$Step = 'step', [hashtable]$Params = @{}, [hashtable]$Settings = @{}, $Approval = $null, [string]$Dir)
    $run = Join-Path $Item.Root ("run-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $run | Out-Null
    $inputPath = Join-Path $run 'input.json'
    $resultPath = Join-Path $run 'result.json'
    $workItem = if ($Dir) { $Dir } else { $Item.WorkItem }
    Set-TestJson -Path $inputPath -Value ([ordered]@{
        schema = 1; plugin = @{ id = 'oneoff-packager' }; step = $Step; workItem = $workItem; workRoot = $Item.WorkRoot
        settings = $Settings; params = $Params; studio = @{}; approval = $Approval
    })
    $pwsh = (Get-Process -Id $PID).Path
    $output = & $pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File (Join-Path (Get-PluginRoot) "scripts\$Script") -StudioInput $inputPath -StudioResult $resultPath 2>&1
    [pscustomobject]@{
        ExitCode = $LASTEXITCODE
        Output = ($output | Out-String)
        Result = if (Test-Path $resultPath) { Get-Content $resultPath -Raw | ConvertFrom-Json } else { $null }
    }
}

function New-FakeYardstickDev {
    # A git checkout with just what New-OneOffYardstickRuntime copies, and a Graph module with
    # the marker line the guard is inserted before.
    param([string]$Path)
    foreach ($dir in 'Modules', 'Config', 'Templates', 'Recipes', 'Icons', 'Tests\RecipeHarness') { New-Item -ItemType Directory -Path (Join-Path $Path $dir) -Force | Out-Null }
    foreach ($file in 'Yardstick.ps1', 'Yardstick.psd1', 'Yardstick.Project.psd1', 'ProjectLayout.ps1', 'RecipeGroups.yaml') { Set-Content -LiteralPath (Join-Path $Path $file) -Value "# $file" }
    Set-Content -LiteralPath (Join-Path $Path 'Config\settings.yaml') -Value 'a: 1'
    # Git keeps no empty folders, and the runtime is a clone, so each folder holds a file.
    foreach ($dir in 'Templates', 'Recipes', 'Icons', 'Tests\RecipeHarness') { Set-Content -LiteralPath (Join-Path $Path "$dir\.gitkeep") -Value '' }
    Set-Content -LiteralPath (Join-Path $Path 'Modules\YardstickGraph.psm1') -Encoding utf8NoBOM -Value @'
function Get-YardstickGraphTokenExpiry { Get-Date }
function Invoke-YardstickGraphRequest {
    [CmdletBinding()]
    param([string]$Resource, [string]$Method = 'Get', [string]$ApiVersion, [int]$MaxAttempts)
    $expiresOn = Get-YardstickGraphTokenExpiry
    throw "would call Graph: $Method $Resource"
}
Export-ModuleMember -Function Invoke-YardstickGraphRequest
'@
    & git -C $Path init -q 2>&1 | Out-Null
    & git -C $Path -c user.name=t -c user.email=t@t add -A 2>&1 | Out-Null
    & git -C $Path -c user.name=t -c user.email=t@t commit -q -m init 2>&1 | Out-Null
}
