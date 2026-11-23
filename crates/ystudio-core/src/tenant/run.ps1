# Yardstick Development Studio: run one recipe through Yardstick on the tenant.
#
# THIS UPLOADS AN APP TO INTUNE. Studio starts it only after the user confirmed
# this exact run, and has already staged a temporary entry for the recipe in
# Local\recipeOverrides.yaml that renames the app (so it cannot touch a real
# app's versions or assignments), forces the test groups and switches
# supersedence and auto-update off. Studio restores that file afterwards.
#
# It runs exactly one command against Yardstick:
#   Yardstick.ps1 -ApplicationId <id> -NoEmail -NoDelete [-Force]
# and afterwards reads the created app back with GET requests. It never deletes
# anything and never runs more than the one recipe.
#
# Studio runs it as: pwsh -File Studio-TenantRun.ps1 -InputPath in.json -ResultPath out.json
# in.json: { root, tenantId, tenantName, recipeId, testDisplayName, groups: [string], assign, force, pwsh }
param(
    [Parameter(Mandatory)][string]$InputPath,
    [Parameter(Mandatory)][string]$ResultPath
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$in = Get-Content -LiteralPath $InputPath -Raw | ConvertFrom-Json
$root = [string]$in.root
$recipeId = [string]$in.recipeId
$testName = [string]$in.testDisplayName
$problems = [System.Collections.Generic.List[string]]::new()
$result = [ordered]@{
    status = 'Failed'; recipeId = $recipeId; exitCode = $null; startedUtc = ''
    tenant = $null; app = $null; groups = @(); problems = @()
}
$graphConnected = $false

function Write-Result {
    param([string]$Status)
    $result.status = $Status
    $result.problems = @($problems)
    $json = $result | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText($ResultPath, $json, [System.Text.UTF8Encoding]::new($false))
}

function Stop-Refused {
    param([string]$Why)
    $problems.Add($Why)
    Write-Host "[Refused] $Why"
    Write-Result 'Refused'
    exit 2
}

function Get-Pref {
    param($Prefs, [string]$Key)
    if ($null -eq $Prefs) { return $null }
    if ($Prefs.Contains($Key)) { return $Prefs[$Key] }
    $null
}

try {
    # ---- guards: nothing is uploaded unless every one holds --------------------
    $prefsPath = Join-Path $root 'Local\preferences.yaml'
    if (-not (Test-Path -LiteralPath $prefsPath -PathType Leaf)) { Stop-Refused 'Local\preferences.yaml does not exist.' }
    Import-Module powershell-yaml -ErrorAction Stop
    $prefs = ConvertFrom-Yaml -Yaml (Get-Content -LiteralPath $prefsPath -Raw)
    if ($null -eq $prefs) { $prefs = @{} }
    if ((Get-Pref $prefs 'developmentOnly') -eq $true) { Stop-Refused 'This clone is development-only.' }
    if ((Get-Pref $prefs 'syncSharedContent') -eq $true) { Stop-Refused 'syncSharedContent is on, so the temporary override could sync to the share.' }

    $recipePath = Join-Path $root "Recipes\Development\$recipeId.yaml"
    if (-not (Test-Path -LiteralPath $recipePath -PathType Leaf)) { Stop-Refused "Recipes\Development\$recipeId.yaml does not exist." }
    $yardstick = Join-Path $root 'Yardstick.ps1'
    if (-not (Test-Path -LiteralPath $yardstick -PathType Leaf)) { Stop-Refused 'Yardstick.ps1 is missing from this clone.' }

    # The isolation override must be in place, or this could rename-collide with a real app.
    $overridesPath = Join-Path $root 'Local\recipeOverrides.yaml'
    if (-not $in.testPrefix -or -not $testName.StartsWith([string]$in.testPrefix)) { Stop-Refused 'The test app name does not carry the test prefix.' }
    if (-not (Test-Path -LiteralPath $overridesPath -PathType Leaf) -or -not (Select-String -LiteralPath $overridesPath -SimpleMatch -Pattern ([string]$in.testPrefix) -Quiet)) {
        Stop-Refused 'The temporary test-app override is not in place, so nothing was run.'
    }

    foreach ($module in 'YardstickCredential', 'YardstickGraph', 'YardstickIntune') {
        $modulePath = Join-Path $root "Modules\$module.psm1"
        if (-not (Test-Path -LiteralPath $modulePath -PathType Leaf)) { Stop-Refused "Modules\$module.psm1 is missing from this clone." }
        Import-Module $modulePath -Force -ErrorAction Stop
    }
    $target = [string](Get-Pref $prefs 'credentialTarget')
    if (-not $target) { $target = 'Yardstick:IntuneGraph' }
    $credential = Get-YardstickIntuneCredential -Target $target
    if (-not $credential) { Stop-Refused "No Yardstick Intune credential named '$target' is stored for this Windows account." }
    if ([string]$credential.TenantID -ne [string]$in.tenantId) { Stop-Refused 'The stored credential belongs to a different tenant than the one set in Studio.' }
    $Global:TenantID = $credential.TenantID
    $Global:ClientID = $credential.ClientID
    $Global:ClientSecret = $credential.ClientSecret
    Connect-YardstickGraph | Out-Null
    $graphConnected = $true
    $org = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource 'organization?$select=id,displayName' -MaxAttempts 2)
    if ($org.Count -ne 1 -or [string]$org[0].id -ne [string]$in.tenantId) { Stop-Refused 'The connected organization is not the tenant set in Studio.' }
    if ($in.tenantName -and [string]$org[0].displayName -ne [string]$in.tenantName) { Stop-Refused "The tenant is called '$($org[0].displayName)', not '$($in.tenantName)'." }
    $result.tenant = [ordered]@{ id = [string]$org[0].id; name = [string]$org[0].displayName }
    Write-Host ("[Pass] Connected to '{0}'." -f $org[0].displayName)

    $expectedGroups = [System.Collections.Generic.List[object]]::new()
    if ($in.assign) {
        foreach ($name in @($in.groups | Where-Object { $_ })) {
            if ($name -match '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') {
                $found = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource ('groups/{0}?$select=id,displayName' -f $name) -MaxAttempts 2)
            } else {
                $filter = [uri]::EscapeDataString(("displayName eq '{0}'" -f $name.Replace("'", "''")))
                $found = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource ('groups?$filter={0}&$select=id,displayName' -f $filter) -MaxAttempts 2)
            }
            if ($found.Count -ne 1) { Stop-Refused "$($found.Count) group(s) match '$name'; exactly one is needed." }
            $expectedGroups.Add([ordered]@{ id = [string]$found[0].id; name = [string]$found[0].displayName })
        }
        if ($expectedGroups.Count -eq 0) { Stop-Refused 'No test group is configured for an assigned run.' }
    }
    $result.groups = @($expectedGroups)

    # ---- the one Yardstick run ---------------------------------------------------
    $started = [DateTime]::UtcNow
    $result.startedUtc = $started.ToString('o')
    $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $yardstick, '-ApplicationId', $recipeId, '-NoEmail', '-NoDelete')
    if ($in.force) { $arguments += '-Force' }
    Write-Host ("[Run] Yardstick.ps1 -ApplicationId {0} -NoEmail -NoDelete{1}" -f $recipeId, $(if ($in.force) { ' -Force' } else { '' }))
    $pwshExe = if ($in.pwsh) { [string]$in.pwsh } else { (Get-Process -Id $PID).Path }
    & $pwshExe @arguments
    $result.exitCode = $LASTEXITCODE
    Write-Host ("[Run] Yardstick finished with exit code {0}." -f $result.exitCode)

    # ---- read back what was created (GET only) -----------------------------------
    $quoted = [uri]::EscapeDataString($testName.Replace("'", "''"))
    $apps = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion beta -Resource ("deviceAppManagement/mobileApps?`$filter=isof('microsoft.graph.win32LobApp') and displayName eq '{0}'" -f $quoted) -MaxAttempts 2)
    # Graph timestamps parse as local-kind DateTime in PowerShell 7; compare in UTC.
    $fresh = @($apps | Where-Object { ([DateTime]$_.createdDateTime).ToUniversalTime() -ge $started.AddMinutes(-1) } | Sort-Object { ([DateTime]$_.createdDateTime).ToUniversalTime() } -Descending)
    if ($fresh.Count -eq 0) {
        $problems.Add("No app named '$testName' was created by this run.")
    } else {
        $app = Invoke-YardstickGraphRequest -Method Get -ApiVersion beta -Resource ('deviceAppManagement/mobileApps/{0}' -f $fresh[0].id) -MaxAttempts 2
        $assignments = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion beta -Resource ('deviceAppManagement/mobileApps/{0}/assignments' -f $app.id) -MaxAttempts 2)
        $assigned = foreach ($a in $assignments) {
            $groupId = if ($a.target.PSObject.Properties['groupId']) { [string]$a.target.groupId } else { $null }
            $groupName = $null
            if ($groupId) {
                try { $groupName = [string](Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource ('groups/{0}?$select=displayName' -f $groupId) -MaxAttempts 2).displayName } catch { }
            }
            [ordered]@{ intent = [string]$a.intent; targetType = [string]$a.target.'@odata.type'; groupId = $groupId; groupName = $groupName }
        }
        $assigned = @($assigned)
        $result.app = [ordered]@{
            id = [string]$app.id; displayName = [string]$app.displayName; displayVersion = [string]$app.displayVersion
            publishingState = [string]$app.publishingState; createdDateTime = [string]$app.createdDateTime
            assignments = $assigned
        }
        Write-Host ("[Found] {0} {1} ({2}), {3} assignment(s)." -f $app.displayName, $app.displayVersion, $app.id, $assigned.Count)
        if ([string]$app.publishingState -ne 'published') { $problems.Add("The app's publishing state is '$($app.publishingState)', not 'published'.") }
        if ($in.assign) {
            $expectedIds = @($expectedGroups | ForEach-Object { $_.id })
            $actualIds = @($assigned | ForEach-Object { $_.groupId } | Where-Object { $_ })
            foreach ($id in $expectedIds) { if ($actualIds -notcontains $id) { $problems.Add("The app is not assigned to test group $id.") } }
            if (@($assigned | Where-Object { -not $_.groupId -or $expectedIds -notcontains $_.groupId }).Count -gt 0) {
                $problems.Add('The app has an assignment outside the test groups. Check it in Intune before leaving it.')
            }
        } elseif ($assigned.Count -gt 0) {
            $problems.Add('The run was meant to be unassigned but the app has assignments. Check it in Intune before leaving it.')
        }
    }
    if ($result.exitCode -ne 0) { $problems.Add("Yardstick exited with code $($result.exitCode).") }

    $status = if ($problems.Count -eq 0) { 'Passed' } else { 'Failed' }
    Write-Result $status
    if ($status -eq 'Passed') { exit 0 } else { exit 1 }
} catch {
    $problems.Add($_.Exception.Message)
    Write-Host "ERROR: $($_.Exception.Message)"
    Write-Result 'Failed'
    exit 1
} finally {
    if ($graphConnected) { try { Disconnect-YardstickGraph } catch { } }
}
