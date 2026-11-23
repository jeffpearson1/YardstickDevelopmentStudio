# Yardstick Development Studio: remove one tenant-validation test app.
#
# THIS DELETES ONE APP FROM INTUNE. It is the only delete the Studio ever sends,
# and it removes exactly one app: the id a Studio tenant run recorded. Before the
# delete it re-reads that app and refuses unless all of these hold:
#   - it is a Win32 app,
#   - its name still starts with the test prefix ("YDS TEST - "), and
#   - it was created when the run recorded it was.
# It never searches, never deletes by name and never deletes more than one app.
#
# Studio runs it as: pwsh -File Studio-TenantRemove.ps1 -InputPath in.json -ResultPath out.json
# in.json: { root, tenantId, tenantName, appId, appName, createdUtc, testPrefix }
param(
    [Parameter(Mandatory)][string]$InputPath,
    [Parameter(Mandatory)][string]$ResultPath
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$in = Get-Content -LiteralPath $InputPath -Raw | ConvertFrom-Json
$root = [string]$in.root
$appId = [string]$in.appId
$problems = [System.Collections.Generic.List[string]]::new()
$result = [ordered]@{ status = 'Failed'; appId = $appId; appName = [string]$in.appName; problems = @() }
$graphConnected = $false

function Write-Result {
    param([string]$Status)
    $result.status = $Status
    $result.problems = @($problems)
    $json = $result | ConvertTo-Json -Depth 4
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
    if ($appId -notmatch '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') { Stop-Refused 'The app id is not a GUID.' }
    $prefix = [string]$in.testPrefix
    if (-not $prefix -or -not ([string]$in.appName).StartsWith($prefix)) { Stop-Refused 'The recorded app name does not carry the test prefix.' }

    $prefsPath = Join-Path $root 'Local\preferences.yaml'
    if (-not (Test-Path -LiteralPath $prefsPath -PathType Leaf)) { Stop-Refused 'Local\preferences.yaml does not exist.' }
    Import-Module powershell-yaml -ErrorAction Stop
    $prefs = ConvertFrom-Yaml -Yaml (Get-Content -LiteralPath $prefsPath -Raw)
    if ($null -eq $prefs) { $prefs = @{} }
    if ((Get-Pref $prefs 'developmentOnly') -eq $true) { Stop-Refused 'This clone is development-only.' }

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
    Write-Host ("[Pass] Connected to '{0}'." -f $org[0].displayName)

    # ---- re-read the app and prove it is the test app this run recorded ---------
    $app = Invoke-YardstickGraphRequest -Method Get -ApiVersion beta -Resource ('deviceAppManagement/mobileApps/{0}' -f $appId) -MaxAttempts 2
    if (-not $app -or [string]$app.id -ne $appId) { Stop-Refused 'The app was not found. It may already have been removed.' }
    if ([string]$app.'@odata.type' -ne '#microsoft.graph.win32LobApp') { Stop-Refused 'The app is not a Win32 app.' }
    if (-not ([string]$app.displayName).StartsWith($prefix)) { Stop-Refused ("The app is now called '{0}', which is not a test app, so it was not removed." -f $app.displayName) }
    if ($in.createdUtc) {
        $recorded = ([DateTime]$in.createdUtc).ToUniversalTime()
        $actual = ([DateTime]$app.createdDateTime).ToUniversalTime()
        if ([Math]::Abs(($actual - $recorded).TotalSeconds) -gt 5) { Stop-Refused 'The app was created at a different time than the run recorded, so it is not the same app.' }
    }
    Write-Host ("[Found] {0} ({1})." -f $app.displayName, $app.id)

    # ---- the one delete ------------------------------------------------------------
    Invoke-YardstickGraphRequest -Method Delete -ApiVersion beta -Resource ('deviceAppManagement/mobileApps/{0}' -f $appId) -MaxAttempts 2 | Out-Null
    Write-Host '[Removed] The test app was removed from Intune.'
    Write-Result 'Removed'
    exit 0
} catch {
    $problems.Add($_.Exception.Message)
    Write-Host "ERROR: $($_.Exception.Message)"
    Write-Result 'Failed'
    exit 1
} finally {
    if ($graphConnected) { try { Disconnect-YardstickGraph } catch { } }
}
