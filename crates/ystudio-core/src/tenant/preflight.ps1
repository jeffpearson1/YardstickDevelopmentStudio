# Yardstick Development Studio: tenant validation preflight.
#
# READ-ONLY. It inspects the clone's preferences and scheduled tasks, signs in to
# Microsoft Graph with the clone's own Intune credential and issues GET requests
# only (the organization and the configured test groups). It never creates,
# changes or deletes anything, in the tenant or on disk apart from -ResultPath.
#
# Studio runs it as: pwsh -File Studio-TenantPreflight.ps1 -InputPath in.json -ResultPath out.json
# in.json: { root, tenantId, tenantName, groups: [string] }
param(
    [Parameter(Mandatory)][string]$InputPath,
    [Parameter(Mandatory)][string]$ResultPath
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$in = Get-Content -LiteralPath $InputPath -Raw | ConvertFrom-Json
$root = [string]$in.root
$checks = [System.Collections.Generic.List[object]]::new()
$groupsOut = [System.Collections.Generic.List[object]]::new()
$tenantOut = $null

function Add-Check {
    param([string]$Id, [ValidateSet('Pass', 'Warn', 'Fail', 'Skip')][string]$Level, [string]$Detail)
    $checks.Add([ordered]@{ id = $Id; level = $Level; detail = $Detail })
    Write-Host ('[{0}] {1}: {2}' -f $Level, $Id, $Detail)
}

function Get-Pref {
    param($Prefs, [string]$Key)
    if ($null -eq $Prefs) { return $null }
    if ($Prefs.Contains($Key)) { return $Prefs[$Key] }
    $null
}

$graphConnected = $false
try {
    # ---- preferences -------------------------------------------------------
    $prefs = $null
    $prefsPath = Join-Path $root 'Local\preferences.yaml'
    if (-not (Test-Path -LiteralPath $prefsPath -PathType Leaf)) {
        Add-Check 'preferences' 'Fail' "Local\preferences.yaml does not exist, so this clone has not been set up. Use 'Connect this clone to Intune' first."
    } elseif (-not (Get-Module -ListAvailable -Name powershell-yaml)) {
        Add-Check 'preferences' 'Fail' 'The PowerShell module powershell-yaml is not installed.'
    } else {
        Import-Module powershell-yaml -ErrorAction Stop
        $prefs = ConvertFrom-Yaml -Yaml (Get-Content -LiteralPath $prefsPath -Raw)
        if ($null -eq $prefs) { $prefs = @{} }
        Add-Check 'preferences' 'Pass' 'Local\preferences.yaml was read.'
    }

    if ($null -ne $prefs) {
        if ((Get-Pref $prefs 'developmentOnly') -eq $true) {
            Add-Check 'development-only' 'Fail' "developmentOnly is true, so Yardstick refuses to run here. Use 'Connect this clone to Intune'."
        } else {
            Add-Check 'development-only' 'Pass' 'This clone is a full Yardstick instance.'
        }

        if ((Get-Pref $prefs 'syncSharedContent') -eq $true) {
            Add-Check 'shared-sync' 'Fail' 'syncSharedContent is on. A tenant run stages a temporary entry in Local\recipeOverrides.yaml, and that file syncs both ways with the shared overrides. Turn syncSharedContent off for this clone.'
        } else {
            Add-Check 'shared-sync' 'Pass' 'Shared content sync is off, so a temporary override cannot leave this PC.'
        }
    }

    # ---- scheduled task ----------------------------------------------------
    try {
        $rootPattern = [regex]::Escape($root.TrimEnd('\'))
        $tasks = @(Get-ScheduledTask -ErrorAction Stop | Where-Object {
                $text = ($_.Actions | ForEach-Object { '{0} {1} {2}' -f $_.Execute, $_.Arguments, $_.WorkingDirectory }) -join ' '
                $text -match $rootPattern
            })
        if ($tasks.Count -gt 0) {
            Add-Check 'scheduled-task' 'Fail' ("A scheduled task runs Yardstick from this clone ({0}). It would deploy every recipe here, including Recipes\Development, on its schedule. Remove the task or use a clone without one." -f (($tasks | ForEach-Object { $_.TaskName }) -join ', '))
        } else {
            Add-Check 'scheduled-task' 'Pass' 'No scheduled task runs Yardstick from this clone.'
        }
    } catch {
        Add-Check 'scheduled-task' 'Warn' ('Scheduled tasks could not be listed: {0}' -f $_.Exception.Message)
    }

    # ---- credential and tenant ---------------------------------------------
    $credential = $null
    foreach ($module in 'YardstickCredential', 'YardstickGraph', 'YardstickIntune') {
        $modulePath = Join-Path $root "Modules\$module.psm1"
        if (-not (Test-Path -LiteralPath $modulePath -PathType Leaf)) { throw "Modules\$module.psm1 is missing from this clone." }
        Import-Module $modulePath -Force -ErrorAction Stop
    }
    $target = [string](Get-Pref $prefs 'credentialTarget')
    if (-not $target) { $target = 'Yardstick:IntuneGraph' }
    $credential = Get-YardstickIntuneCredential -Target $target
    if (-not $credential) {
        Add-Check 'credential' 'Fail' ("No Yardstick Intune credential named '{0}' is stored for this Windows account. Use 'Connect this clone to Intune' to run Install-Yardstick." -f $target)
    } elseif ([string]$credential.TenantID -ne [string]$in.tenantId) {
        Add-Check 'credential' 'Fail' 'The stored credential belongs to a different tenant than the one set in Studio.'
        $credential = $null
    } else {
        Add-Check 'credential' 'Pass' ("Credential '{0}' belongs to the configured tenant." -f $target)
    }

    if ($credential) {
        $Global:TenantID = $credential.TenantID
        $Global:ClientID = $credential.ClientID
        $Global:ClientSecret = $credential.ClientSecret
        Connect-YardstickGraph | Out-Null
        $graphConnected = $true
        $org = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource 'organization?$select=id,displayName' -MaxAttempts 2)
        if ($org.Count -ne 1 -or [string]$org[0].id -ne [string]$in.tenantId) {
            Add-Check 'tenant' 'Fail' 'The connected organization is not the tenant set in Studio.'
        } elseif ($in.tenantName -and [string]$org[0].displayName -ne [string]$in.tenantName) {
            Add-Check 'tenant' 'Fail' ("The tenant is called '{0}', not '{1}'." -f $org[0].displayName, $in.tenantName)
        } else {
            $tenantOut = [ordered]@{ id = [string]$org[0].id; name = [string]$org[0].displayName }
            Add-Check 'tenant' 'Pass' ("Connected to '{0}'." -f $org[0].displayName)
        }

        # ---- test groups (GET only) -----------------------------------------
        if ($null -ne $tenantOut) {
            $wanted = @($in.groups | Where-Object { $_ })
            if ($wanted.Count -eq 0) {
                Add-Check 'groups' 'Skip' 'No test group is configured.'
            }
            foreach ($name in $wanted) {
                $found = @()
                if ($name -match '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') {
                    $found = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource ('groups/{0}?$select=id,displayName,securityEnabled,membershipRule' -f $name) -MaxAttempts 2)
                } else {
                    $filter = [uri]::EscapeDataString(("displayName eq '{0}'" -f $name.Replace("'", "''")))
                    $found = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource ('groups?$filter={0}&$select=id,displayName,securityEnabled,membershipRule' -f $filter) -MaxAttempts 2)
                }
                if ($found.Count -ne 1) {
                    Add-Check "group:$name" 'Fail' ("{0} group(s) match '{1}'; exactly one is needed." -f $found.Count, $name)
                    continue
                }
                $group = $found[0]
                $groupsOut.Add([ordered]@{ input = $name; id = [string]$group.id; name = [string]$group.displayName })
                if ($group.membershipRule) {
                    Add-Check "group:$name" 'Warn' ("'{0}' is a dynamic group, so who receives the app can change on its own." -f $group.displayName)
                } else {
                    Add-Check "group:$name" 'Pass' ("'{0}' resolves to {1}." -f $group.displayName, $group.id)
                }
            }
        }
    }
} catch {
    Add-Check 'preflight' 'Fail' $_.Exception.Message
} finally {
    if ($graphConnected) { try { Disconnect-YardstickGraph } catch { } }
}

$ok = -not ($checks | Where-Object { $_.level -eq 'Fail' })
$result = [ordered]@{ ok = [bool]$ok; tenant = $tenantOut; groups = @($groupsOut); checks = @($checks) }
$json = $result | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText($ResultPath, $json, [System.Text.UTF8Encoding]::new($false))
if ($ok) { exit 0 } else { exit 1 }
