#Requires -Version 7.0
# The One-Off Packager's logic. Nothing here is specific to a tenant, group or ticket:
# every such value comes in as a parameter, and every path is inside the work item.
Set-StrictMode -Version Latest

$script:Here = $PSScriptRoot
$script:AcceptedOutcomes = @('Pass', 'Pass with warnings')
$script:GroupSelect = '$select=id,displayName,groupTypes,mailEnabled,securityEnabled,membershipRule,membershipRuleProcessingState'
$script:AppSelect = '$select=id,displayName,publisher,isAssigned,createdDateTime,lastModifiedDateTime&$top=999'

Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1')

# ------------------------------------------------------------------ small helpers

function Get-Prop {
    # A property or key if present, else $Default. Safe under strict mode.
    param($Object, [Parameter(Mandatory)][string]$Name, $Default = $null)
    if ($null -eq $Object) { return $Default }
    if ($Object -is [System.Collections.IDictionary]) {
        if ($Object.Contains($Name) -and $null -ne $Object[$Name]) { return $Object[$Name] }
        return $Default
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($property -and $null -ne $property.Value) { return $property.Value }
    $Default
}

function Write-Json {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][AllowNull()][AllowEmptyCollection()]$Value, [int]$Depth = 15)
    $parent = Split-Path -Parent $Path
    if ($parent -and -not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    ConvertTo-Json -InputObject $Value -Depth $Depth | Set-Content -LiteralPath $Path -Encoding utf8NoBOM
}

function Read-Json {
    param([Parameter(Mandatory)][string]$Path)
    [IO.File]::ReadAllText($Path) | ConvertFrom-Json -Depth 30
}

function Get-WorkItemProducts {
    param([Parameter(Mandatory)][string]$WorkItem)
    $path = Join-Path $WorkItem 'products.json'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'products.json is missing. Run the triage task first.' }
    $document = Read-Json -Path $path
    @(Get-Prop $document 'products' @())
}

function Get-RecipeValue {
    param([Parameter(Mandatory)]$Recipe, [Parameter(Mandatory)][string]$Name, $Default = $null)
    if ($Recipe.Contains($Name) -and $null -ne $Recipe[$Name] -and "$($Recipe[$Name])" -ne '') { return $Recipe[$Name] }
    $Default
}

function Read-Recipe {
    param([Parameter(Mandatory)][string]$Path)
    Import-Module powershell-yaml -MinimumVersion 0.4.12 -ErrorAction Stop
    [IO.File]::ReadAllText($Path) | ConvertFrom-Yaml
}

function Resolve-RecipePlaceholders {
    # Fills <filename>, <version> and <productcode>, then refuses what is left.
    param([string]$Text, [Parameter(Mandatory)][string]$FileName, [Parameter(Mandatory)][string]$Version, [string]$ProductCode, [string]$What = 'script')
    if (-not $Text) { return $Text }
    $out = $Text.Replace('<filename>', $FileName).Replace('<version>', $Version)
    if ($ProductCode) { $out = $out.Replace('<productcode>', $ProductCode) }
    if ($out -match '<(filename|version|productcode)>') { throw "The $What still has an unresolved '<$($Matches[1])>' placeholder." }
    $out
}

# ------------------------------------------------------------------ inventory

function Get-OneOffSearchPatterns {
    # The regexes that find a product's possible existing Intune apps: those the
    # triage wrote, or its name with spaces made flexible.
    param([Parameter(Mandatory)][object[]]$Products)
    $patterns = [System.Collections.Generic.List[string]]::new()
    foreach ($product in $Products) {
        $own = @(Get-Prop $product 'searchPatterns' @()) | Where-Object { $_ }
        if (-not $own) {
            $name = [string](Get-Prop $product 'name' '')
            if (-not $name.Trim()) { continue }
            $own = @(([regex]::Escape($name.Trim()) -replace '\\ ', '\s*'))
        }
        foreach ($pattern in $own) {
            try { [void][regex]::new([string]$pattern) } catch { throw "Search pattern '$pattern' for '$(Get-Prop $product 'name' '?')' is not a valid regular expression." }
            if (-not $patterns.Contains([string]$pattern)) { $patterns.Add([string]$pattern) }
        }
    }
    @($patterns)
}

function Select-InventoryCandidates {
    # Apps whose "name publisher" matches any pattern, with the patterns that matched.
    param([Parameter(Mandatory)][AllowEmptyCollection()][object[]]$Index, [Parameter(Mandatory)][string[]]$Patterns)
    foreach ($app in $Index) {
        $text = "$(Get-Prop $app 'displayName' '') $(Get-Prop $app 'publisher' '')"
        $hit = @($Patterns | Where-Object { $text -match "(?i)$_" })
        if ($hit.Count -gt 0) { [pscustomobject]@{ App = $app; MatchedPatterns = $hit } }
    }
}

function Get-OneOffGroup {
    # A fresh read of an Entra group, and whether it can be an Available target.
    param([Parameter(Mandatory)][string]$GroupId)
    $group = Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource "groups/${GroupId}?$script:GroupSelect" -MaxAttempts 2
    if (-not $group -or [string]$group.id -ne $GroupId) { throw "Group $GroupId was not found." }
    $problems = @()
    if ($group.securityEnabled -ne $true) { $problems += 'it is not a security group' }
    if (Get-Prop $group 'membershipRule') { $problems += 'it has a dynamic membership rule' }
    [pscustomobject]@{
        Id = [string]$group.id; DisplayName = [string]$group.displayName
        Type = 'static security group'; Suitable = ($problems.Count -eq 0); Problems = $problems; Raw = $group
    }
}

function Get-OneOffInventoryData {
    # Read-only. Assumes a connected Graph session.
    param([Parameter(Mandatory)][string[]]$Patterns, [string[]]$GroupIds = @())
    $index = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps?$script:AppSelect" -MaxAttempts 2)
    $matching = foreach ($hit in @(Select-InventoryCandidates -Index $index -Patterns $Patterns)) {
        $id = [string]$hit.App.id
        $details = Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$id" -MaxAttempts 2
        $assignments = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$id/assignments" -MaxAttempts 2)
        [ordered]@{
            id = $details.id; displayName = $details.displayName; publisher = $details.publisher
            displayVersion = Get-Prop $details 'displayVersion'; uploadState = Get-Prop $details 'uploadState'
            publishingState = Get-Prop $details 'publishingState'; isAssigned = Get-Prop $details 'isAssigned'
            createdDateTime = Get-Prop $details 'createdDateTime'; lastModifiedDateTime = Get-Prop $details 'lastModifiedDateTime'
            installCommandLine = Get-Prop $details 'installCommandLine'; uninstallCommandLine = Get-Prop $details 'uninstallCommandLine'
            allowAvailableUninstall = Get-Prop $details 'allowAvailableUninstall'
            detectionRules = @(Get-Prop $details 'detectionRules' @())
            matchedPatterns = @($hit.MatchedPatterns)
            assignments = @($assignments | ForEach-Object {
                [ordered]@{ id = $_.id; intent = $_.intent; targetType = $_.target.'@odata.type'; groupId = Get-Prop $_.target 'groupId' }
            })
        }
    }
    $groups = foreach ($groupId in ($GroupIds | Where-Object { $_ } | Select-Object -Unique)) {
        $group = Get-OneOffGroup -GroupId $groupId
        [ordered]@{ id = $group.Id; displayName = $group.DisplayName; type = $group.Type; suitable = $group.Suitable; problems = @($group.Problems) }
    }
    [ordered]@{ totalAppsScanned = $index.Count; matchingApps = @($matching); groups = @($groups) }
}

function Connect-OneOffTenant {
    # The safety proof, the stored credential, and a connection that is checked
    # against the tenant the plugin is set up for.
    param(
        [Parameter(Mandatory)][string]$Runtime,
        [Parameter(Mandatory)][string]$TenantId,
        [string]$TenantName,
        [string]$CredentialTarget = 'Yardstick:IntuneGraph'
    )
    $safety = & (Join-Path $script:Here 'Test-OneOffRuntimeSafety.ps1') -RuntimePath $Runtime
    if (-not $safety.Safe) { throw 'The isolated runtime DELETE guard did not pass.' }
    foreach ($module in 'YardstickCredential', 'YardstickGraph', 'YardstickIntune') {
        Import-Module (Join-Path $Runtime "Modules\$module.psm1") -Force -ErrorAction Stop
    }
    $credential = Get-YardstickIntuneCredential -Target $CredentialTarget
    if (-not $credential) { throw "The Yardstick Intune credential '$CredentialTarget' is not available on this PC." }
    if ([string]$credential.TenantID -ne $TenantId) { throw 'The stored credential belongs to a different tenant than the plugin is set up for.' }
    $Global:TenantID = $credential.TenantID
    $Global:ClientID = $credential.ClientID
    $Global:ClientSecret = $credential.ClientSecret
    Connect-YardstickGraph | Out-Null
    $tenant = @(Invoke-YardstickGraphRequest -Method Get -ApiVersion v1.0 -Resource 'organization?$select=id,displayName' -MaxAttempts 2)
    if ($tenant.Count -ne 1 -or [string]$tenant[0].id -ne $TenantId) { throw 'The connected organization is not the configured tenant.' }
    if ($TenantName -and [string]$tenant[0].displayName -ne $TenantName) {
        throw "The tenant's name is '$($tenant[0].displayName)', not the configured '$TenantName'."
    }
    [pscustomobject]@{ Id = [string]$tenant[0].id; DisplayName = [string]$tenant[0].displayName; Safety = $safety }
}

function New-OneOffInventory {
    param(
        [Parameter(Mandatory)][string]$WorkItem,
        [Parameter(Mandatory)][string]$TenantId,
        [string]$TenantName,
        [string]$DefaultGroupId,
        [string]$CredentialTarget = 'Yardstick:IntuneGraph'
    )
    $products = @(Get-WorkItemProducts -WorkItem $WorkItem)
    if ($products.Count -eq 0) { throw 'products.json lists no products.' }
    $patterns = @(Get-OneOffSearchPatterns -Products $products)
    $groupIds = @($products | ForEach-Object { Get-Prop $_ 'requestedGroupId' } | Where-Object { $_ }) + @($DefaultGroupId) | Where-Object { $_ }
    $session = Connect-OneOffTenant -Runtime (Join-Path $WorkItem 'runtime') -TenantId $TenantId -TenantName $TenantName -CredentialTarget $CredentialTarget
    try {
        $data = Get-OneOffInventoryData -Patterns $patterns -GroupIds $groupIds
    } finally {
        Disconnect-YardstickGraph
    }
    $inventory = [ordered]@{
        capturedUtc = [DateTime]::UtcNow.ToString('o')
        tenant = [ordered]@{ id = $session.Id; displayName = $session.DisplayName }
        patterns = $patterns
        totalAppsScanned = $data.totalAppsScanned
        matchingApps = $data.matchingApps
        groups = $data.groups
        defaultGroupId = $DefaultGroupId
        deleteGuardPassed = $true
        sourceCommit = $session.Safety.SourceCommit
    }
    $path = Join-Path $WorkItem 'inventory\intune-inventory.json'
    Write-Json -Path $path -Value $inventory
    [pscustomobject]@{ Path = $path; Inventory = $inventory }
}

# ------------------------------------------------------------------ full-test evidence and packaging

function Find-TestResult {
    # The newest run of this mode that tested this recipe, with the recipe exactly
    # as it is now. A recipe edited since its test is refused.
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)][string]$RecipePath, [ValidateSet('Prepare', 'Full')][string]$Mode = 'Full')
    $id = [IO.Path]::GetFileNameWithoutExtension($RecipePath)
    $recipeHash = Get-FileSha256 -Path $RecipePath
    $runs = Get-ChildItem -LiteralPath (Join-Path $WorkItem 'artifacts\package-tests') -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
    $edited = $false
    foreach ($run in $runs) {
        $invocationPath = Join-Path $run.FullName 'invocation.json'
        if (-not (Test-Path -LiteralPath $invocationPath)) { continue }
        $invocation = Read-Json -Path $invocationPath
        if ((Get-Prop $invocation 'mode') -ne $Mode) { continue }
        $tested = @(Get-Prop $invocation 'recipes' @()) | Where-Object { [IO.Path]::GetFileNameWithoutExtension([string]$_.path) -eq $id }
        if (-not $tested) { continue }
        foreach ($summaryPath in @(Get-Prop $invocation 'summaryPaths' @())) {
            if (-not (Test-Path -LiteralPath $summaryPath)) { continue }
            $summary = Read-Json -Path $summaryPath
            $result = @(Get-Prop $summary 'Results' @()) | Where-Object { $_.Id -eq $id } | Select-Object -First 1
            if (-not $result) { continue }
            if (([string]$tested[0].sha256).ToUpperInvariant() -ne $recipeHash) { $edited = $true; continue }
            return [pscustomobject]@{ Result = $result; SummaryPath = $summaryPath; RunId = $run.Name }
        }
    }
    if ($edited) { throw "Recipe '$id' was changed after its $($Mode.ToLower()) test. Run the test again." }
    $null
}

function Find-FullTestResult {
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)][string]$RecipePath)
    Find-TestResult -WorkItem $WorkItem -RecipePath $RecipePath -Mode Full
}

function New-OneOffLocalPackages {
    param([Parameter(Mandatory)][string]$WorkItem, [string[]]$Recipes)
    $runtime = Join-Path $WorkItem 'runtime'
    foreach ($module in 'YardstickSupport', 'YardstickIntune') {
        Import-Module (Join-Path $runtime "Modules\$module.psm1") -Force -ErrorAction Stop
    }
    $recipeFiles = @(if ($Recipes) { $Recipes | ForEach-Object { Resolve-WorkItemPath -WorkItem $WorkItem -Relative $_ } }
        else { Get-ChildItem -LiteralPath (Join-Path $WorkItem 'recipes') -Filter *.yaml -File | Select-Object -ExpandProperty FullName })
    if ($recipeFiles.Count -eq 0) { throw 'There are no recipes to package.' }

    $outputRoot = Join-Path $WorkItem 'local-packages'
    $manifestPath = Join-Path $outputRoot 'package-manifest.json'
    $existing = @(if (Test-Path -LiteralPath $manifestPath) { (Read-Json -Path $manifestPath).packages })
    $records = [System.Collections.Generic.List[object]]::new()
    $skipped = [System.Collections.Generic.List[string]]::new()
    foreach ($recipeFile in $recipeFiles) {
        $id = [IO.Path]::GetFileNameWithoutExtension($recipeFile)
        $found = Find-FullTestResult -WorkItem $WorkItem -RecipePath $recipeFile
        if (-not $found -or $found.Result.Overall -notin $script:AcceptedOutcomes) {
            $why = if (-not $found) { 'no install test' } else { "its install test was '$($found.Result.Overall)'" }
            $skipped.Add("$id ($why)")
            continue
        }
        $result = $found.Result
        $recipe = Read-Recipe -Path $recipeFile
        $version = [string](Get-RecipeValue $recipe 'version' '')
        if ([string](Get-RecipeValue $recipe 'id' '') -ne $id -or ($version -and $version -ne [string]$result.Version)) {
            throw "Recipe '$id' does not match its install test (id or version)."
        }
        $installer = [string]$result.Installer
        if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
            throw "The installer tested for '$id' was not kept ($installer). Run the install test again."
        }
        $sourceHash = Get-FileSha256 -Path $installer
        if ($sourceHash -ne ([string]$result.SHA256).ToUpperInvariant()) { throw "The kept installer for '$id' no longer matches the tested hash." }
        $packageDirectory = Join-Path $outputRoot "$id\$($result.Version)"
        if (Test-Path -LiteralPath $packageDirectory) {
            throw "A package for $id $($result.Version) already exists; refusing to overwrite it. Remove local-packages\$id to rebuild."
        }
        $stage = Join-Path $outputRoot "staging\$id\$($result.Version)"
        New-Item -ItemType Directory -Path $stage, $packageDirectory -Force | Out-Null
        $setupFile = [IO.Path]::GetFileName($installer)
        Copy-Item -LiteralPath $installer -Destination (Join-Path $stage $setupFile)
        $productCode = [string](Get-Prop $result 'ProductCode' '')
        foreach ($kind in 'Install', 'Uninstall') {
            $text = [string](Get-RecipeValue $recipe "powerShell${kind}Script" '')
            if ($text) {
                $text = Resolve-RecipePlaceholders -Text $text -FileName $setupFile -Version ([string]$result.Version) -ProductCode $productCode -What "$id PowerShell $($kind.ToLower()) script"
                Set-Content -LiteralPath (Join-Path $stage "$($kind.ToLower()).ps1") -Value (Add-NativeBitnessGuard -ScriptContent $text) -Encoding utf8NoBOM
            }
        }
        $package = New-YardstickWin32AppPackage -SourceFolder $stage -SetupFile $setupFile -OutputFolder $packageDirectory -Force -TimeoutMinutes 30
        $packagePath = [string]$package.Path
        if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) { throw "The package for $id was not created." }
        $records.Add([ordered]@{
            id = $id
            version = [string]$result.Version
            setupFile = $setupFile
            sourceSha256 = $sourceHash
            signatureStatus = [string](Get-Prop $result 'SignatureStatus' '')
            productCode = $productCode
            recipeFile = ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $recipeFile
            recipeSha256 = Get-FileSha256 -Path $recipeFile
            testEvidence = ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $found.SummaryPath
            testEvidenceSha256 = Get-FileSha256 -Path $found.SummaryPath
            testOutcome = [string]$result.Overall
            packageFile = ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $packagePath
            packageSha256 = Get-FileSha256 -Path $packagePath
        })
    }
    if ($records.Count -eq 0) { throw "Nothing to package: $($skipped -join '; ')." }
    $builtIds = @($records | ForEach-Object { $_['id'] })
    $kept = @($existing | Where-Object { $_.id -notin $builtIds })
    Write-Json -Path $manifestPath -Value ([ordered]@{ createdUtc = [DateTime]::UtcNow.ToString('o'); packages = @($kept) + @($records) }) -Depth 8
    [pscustomobject]@{ ManifestPath = $manifestPath; Packages = @($records); Skipped = @($skipped) }
}

# ------------------------------------------------------------------ the change set

function Get-RecipeAppSettings {
    # What the Intune app is made from: the recipe, with Yardstick's own defaults.
    param([Parameter(Mandatory)]$Recipe, [Parameter(Mandatory)]$Package)
    $setup = [string]$Package.setupFile
    $version = [string]$Package.version
    $productCode = [string](Get-Prop $Package 'productCode' '')
    $resolve = { param($text, $what) Resolve-RecipePlaceholders -Text $text -FileName $setup -Version $version -ProductCode $productCode -What $what }
    $install = [string](Get-RecipeValue $Recipe 'installScript' '')
    $uninstall = [string](Get-RecipeValue $Recipe 'uninstallScript' '')
    if (-not $install -and (Get-RecipeValue $Recipe 'powerShellInstallScript')) { $install = 'powershell.exe -noprofile -executionpolicy bypass -file .\install.ps1' }
    if (-not $uninstall -and (Get-RecipeValue $Recipe 'powerShellUninstallScript')) { $uninstall = 'powershell.exe -noprofile -executionpolicy bypass -file .\uninstall.ps1' }
    if (-not $install) { throw "Recipe '$($Package.id)' has no install command." }
    if (-not $uninstall) { throw "Recipe '$($Package.id)' has no uninstall command." }
    foreach ($field in 'displayName', 'publisher', 'description') {
        if (-not (Get-RecipeValue $Recipe $field)) { throw "Recipe '$($Package.id)' has no $field." }
    }
    [ordered]@{
        displayName = [string]$Recipe['displayName']
        publisher = [string]$Recipe['publisher']
        description = [string]$Recipe['description']
        version = $version
        installCommand = (& $resolve $install "$($Package.id) install command")
        uninstallCommand = (& $resolve $uninstall "$($Package.id) uninstall command")
        architecture = [string](Get-RecipeValue $Recipe 'architecture' 'x64')
        minOSVersion = [string](Get-RecipeValue $Recipe 'minOSVersion' 'W10_20H2')
        installExperience = [string](Get-RecipeValue $Recipe 'installExperience' 'system')
        restartBehavior = [string](Get-RecipeValue $Recipe 'restartBehavior' 'basedOnReturnCode')
        maximumInstallationTimeInMinutes = [int](Get-RecipeValue $Recipe 'maximumInstallationTimeInMinutes' 60)
        detectionType = [string](Get-RecipeValue $Recipe 'detectionType' 'script')
        iconFile = [string](Get-RecipeValue $Recipe 'iconFile' '')
    }
}

function New-OneOffDetectionRule {
    # script rules are written to a file in the change folder so they can be hashed;
    # msi, file and registry rules come from Yardstick's own builder.
    param([Parameter(Mandatory)]$Recipe, [Parameter(Mandatory)]$Package, [Parameter(Mandatory)]$Settings, [Parameter(Mandatory)][string]$OutputFolder)
    $setup = [string]$Package.setupFile
    $productCode = [string](Get-Prop $Package 'productCode' '')
    if ($Settings.detectionType -eq 'script') {
        $text = Resolve-RecipePlaceholders -Text ([string](Get-RecipeValue $Recipe 'detectionScript' '')) -FileName $setup -Version $Settings.version -ProductCode $productCode -What "$($Package.id) detection script"
        if (-not $text) { throw "Recipe '$($Package.id)' has no detection script." }
        $path = Join-Path $OutputFolder "$($Package.id)-detection.ps1"
        Set-Content -LiteralPath $path -Value $text -Encoding utf8NoBOM
        $enforce = [bool](Get-RecipeValue $Recipe 'detectionScriptEnforceSignatureCheck' $false)
        $as32 = [bool](Get-RecipeValue $Recipe 'detectionScriptRunAs32Bit' $false)
        return [pscustomobject]@{ Rule = (New-YardstickWin32AppDetectionRuleScript -ScriptFile $path -EnforceSignatureCheck $enforce -RunAs32Bit $as32); ScriptPath = $path }
    }
    if ($Settings.detectionType -eq 'msi' -and -not $productCode) { throw "Recipe '$($Package.id)' uses MSI detection but its install test recorded no product code." }
    $spec = [ordered]@{ detectionType = $Settings.detectionType }
    foreach ($key in 'fileDetectionMethod', 'fileDetectionPath', 'fileDetectionName', 'fileDetectionOperator', 'fileDetectionVersion', 'fileDetectionDateTime', 'fileDetectionValue',
        'registryDetectionMethod', 'registryDetectionKey', 'registryDetectionValueName', 'registryDetectionValue', 'registryDetectionOperator') {
        if ($Recipe.Contains($key)) { $spec[$key] = $Recipe[$key] }
    }
    $rules = @(New-YardstickDetectionRuleFromSpec -Spec $spec -ProductCode $productCode -DefaultVersion $Settings.version -Is32BitApp ([bool](Get-RecipeValue $Recipe 'is32BitApp' $false)))
    foreach ($extra in @(Get-RecipeValue $Recipe 'additionalDetectionRules' @())) {
        $rules += New-YardstickDetectionRuleFromSpec -Spec $extra -ProductCode $productCode -DefaultVersion $Settings.version -Is32BitApp ([bool](Get-RecipeValue $Recipe 'is32BitApp' $false))
    }
    [pscustomobject]@{ Rule = $rules; ScriptPath = $null }
}

function New-OneOffChangeSummary {
    param([Parameter(Mandatory)]$Change)
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add('# Change to approve')
    $lines.Add('')
    $lines.Add("Tenant: **$($Change.tenantName)** (``$($Change.tenantId)``)")
    $lines.Add('')
    $lines.Add("Each app is created, checked, then made **$($Change.intent)** in Company Portal to its group. Notifications: $($Change.notifications). 'Allow users to uninstall': **$($Change.allowAvailableUninstall)**.")
    $lines.Add('')
    foreach ($app in $Change.apps) {
        $lines.Add("## $($app.displayName) $($app.version)")
        $lines.Add('')
        $lines.Add("- Publisher: $($app.publisher)")
        $lines.Add("- Target group: **$($app.assignment.groupDisplayName)** (``$($app.assignment.groupId)``, $($app.assignment.groupType))")
        $lines.Add("- Install: ``$($app.installCommand)``")
        $lines.Add("- Uninstall: ``$($app.uninstallCommand)``")
        $lines.Add("- Installer SHA-256: ``$($app.sourceSha256)``")
        $lines.Add("- Package SHA-256: ``$($app.packageSha256)``")
        $lines.Add("- Install test: $($app.testOutcome) (``$($app.testEvidence)``)")
        if ($app.warning) { $lines.Add("- **Warning:** $($app.warning)") }
        $lines.Add('')
    }
    $lines.Add('## Limits')
    $lines.Add('')
    $lines.Add("- $($Change.incompleteUploadRisk)")
    $lines.Add("- $($Change.deleteGuard)")
    $lines.Add("- Inventory taken $($Change.inventoryCapturedUtc): $($Change.appsScanned) apps scanned, $($Change.duplicateCandidates) possible duplicate(s).")
    $lines -join "`n"
}

function New-OneOffLiveChange {
    param(
        [Parameter(Mandatory)][string]$WorkItem,
        [Parameter(Mandatory)][string]$TenantId,
        [string]$TenantName,
        [string]$DefaultGroupId,
        [string]$CredentialTarget = 'Yardstick:IntuneGraph'
    )
    $output = Join-Path $WorkItem 'intune-upload'
    if (Test-Path -LiteralPath $output) { throw 'intune-upload already exists; refusing to overwrite a change that may already be approved or applied.' }
    $manifestPath = Join-Path $WorkItem 'local-packages\package-manifest.json'
    $inventoryPath = Join-Path $WorkItem 'inventory\intune-inventory.json'
    foreach ($path in $manifestPath, $inventoryPath) { if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$(Split-Path -Leaf $path) is missing. Run the earlier steps first." } }
    $manifest = Read-Json -Path $manifestPath
    $inventory = Read-Json -Path $inventoryPath
    $products = @(Get-WorkItemProducts -WorkItem $WorkItem)
    if ([string]$inventory.tenant.id -ne $TenantId) { throw 'The inventory was taken from a different tenant than the plugin is set up for.' }

    $session = Connect-OneOffTenant -Runtime (Join-Path $WorkItem 'runtime') -TenantId $TenantId -TenantName $TenantName -CredentialTarget $CredentialTarget
    try {
        New-Item -ItemType Directory -Path $output -Force | Out-Null
        $groups = @{}
        $apps = foreach ($package in @($manifest.packages)) {
            $product = $products | Where-Object { (Get-Prop $_ 'id') -eq $package.id } | Select-Object -First 1
            $own = @(Get-Prop $product 'searchPatterns' @())
            if (-not $own -and $product) { $own = @(Get-OneOffSearchPatterns -Products @($product)) }
            $duplicates = @(@($inventory.matchingApps) | Where-Object { $hit = $_; @($hit.matchedPatterns | Where-Object { $_ -in $own }).Count -gt 0 })
            if ($duplicates.Count -gt 0) {
                throw "Intune already has $($duplicates.Count) app(s) that may be '$($package.id)': $((@($duplicates | ForEach-Object { $_.displayName }) -join ', ')). Review them and set the product's status before preparing a change."
            }
            foreach ($check in @(
                @{ Path = $package.packageFile; Hash = $package.packageSha256; Label = 'package' },
                @{ Path = $package.recipeFile; Hash = $package.recipeSha256; Label = 'recipe' },
                @{ Path = $package.testEvidence; Hash = $package.testEvidenceSha256; Label = 'test evidence' }
            )) {
                $full = Resolve-WorkItemPath -WorkItem $WorkItem -Relative $check.Path
                if ((Get-FileSha256 -Path $full) -ne ([string]$check.Hash).ToUpperInvariant()) { throw "$($package.id) $($check.Label) changed since it was packaged." }
            }
            $recipe = Read-Recipe -Path (Resolve-WorkItemPath -WorkItem $WorkItem -Relative $package.recipeFile)
            $settings = Get-RecipeAppSettings -Recipe $recipe -Package $package
            $detection = New-OneOffDetectionRule -Recipe $recipe -Package $package -Settings $settings -OutputFolder $output

            $groupId = [string](Get-Prop $product 'requestedGroupId' $DefaultGroupId)
            if (-not $groupId) { throw "No target group for '$($package.id)'. Name one in products.json (requestedGroupId) or set the default group in the plugin settings." }
            if (-not $groups.ContainsKey($groupId)) { $groups[$groupId] = Get-OneOffGroup -GroupId $groupId }
            $group = $groups[$groupId]
            if (-not $group.Suitable) { throw "Group '$($group.DisplayName)' cannot be an Available target: $($group.Problems -join ', ')." }

            $parameters = [ordered]@{
                FilePath = $package.packageFile
                DisplayName = $settings.displayName
                Description = $settings.description
                Publisher = $settings.publisher
                AppVersion = $settings.version
                InstallCommandLine = $settings.installCommand
                UninstallCommandLine = $settings.uninstallCommand
                DetectionRule = $detection.Rule
                RequirementRule = New-YardstickWin32AppRequirementRule -Architecture $settings.architecture -MinimumSupportedWindowsRelease $settings.minOSVersion
                InstallExperience = $settings.installExperience
                RestartBehavior = $settings.restartBehavior
                MaximumInstallationTimeInMinutes = $settings.maximumInstallationTimeInMinutes
                AllowAvailableUninstall = $true
            }
            $icon = if ($settings.iconFile) { Join-Path $WorkItem "icons\$($settings.iconFile)" }
            if ($icon -and (Test-Path -LiteralPath $icon -PathType Leaf)) { $parameters['Icon'] = New-YardstickWin32AppIcon -FilePath $icon }
            $parameterPath = Join-Path $output "$($package.id)-upload-parameters.json"
            Write-Json -Path $parameterPath -Value $parameters
            $warning = $null
            if ($package.signatureStatus -and $package.signatureStatus -ne 'Valid') { $warning = "The installer's signature status is '$($package.signatureStatus)'. Its exact SHA-256 is pinned and the lifecycle test used it." }
            if ($package.testOutcome -eq 'Pass with warnings') { $warning = (@($warning, 'The install test passed with warnings; read its summary.') | Where-Object { $_ }) -join ' ' }

            [ordered]@{
                recipeId = $package.id
                displayName = $settings.displayName
                version = $settings.version
                publisher = $settings.publisher
                searchPatterns = @($own)
                sourceSha256 = $package.sourceSha256
                packageFile = $package.packageFile
                packageSha256 = $package.packageSha256
                recipeFile = $package.recipeFile
                recipeSha256 = $package.recipeSha256
                testEvidence = $package.testEvidence
                testEvidenceSha256 = $package.testEvidenceSha256
                testOutcome = $package.testOutcome
                parameters = ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $parameterPath
                parametersSha256 = Get-FileSha256 -Path $parameterPath
                detectionScript = if ($detection.ScriptPath) { ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $detection.ScriptPath }
                detectionScriptSha256 = if ($detection.ScriptPath) { Get-FileSha256 -Path $detection.ScriptPath }
                installCommand = $settings.installCommand
                uninstallCommand = $settings.uninstallCommand
                warning = $warning
                assignment = [ordered]@{
                    intent = 'available'
                    targetType = '#microsoft.graph.groupAssignmentTarget'
                    groupId = $group.Id
                    groupDisplayName = $group.DisplayName
                    groupType = $group.Type
                    filters = @()
                    exclusions = @()
                }
            }
        }
        if (@($apps).Count -eq 0) { throw 'No packaged apps to prepare.' }
        $change = [ordered]@{
            preparedUtc = [DateTime]::UtcNow.ToString('o')
            tenantId = $session.Id
            tenantName = $session.DisplayName
            inventoryCapturedUtc = $inventory.capturedUtc
            appsScanned = $inventory.totalAppsScanned
            duplicateCandidates = 0
            intent = 'available'
            notifications = 'showAll'
            allowAvailableUninstall = $true
            noDelete = $true
            deleteGuard = $session.Safety.Proof
            incompleteUploadRisk = 'A failed upload can leave an incomplete Intune object for manual review, because Graph DELETE is blocked.'
            apps = @($apps)
        }
        $changePath = Join-Path $output 'live-change-set.json'
        Write-Json -Path $changePath -Value $change
        $summaryPath = Join-Path $output 'live-change-summary.md'
        Set-Content -LiteralPath $summaryPath -Value (New-OneOffChangeSummary -Change (ConvertTo-Json -InputObject $change -Depth 15 | ConvertFrom-Json -Depth 30)) -Encoding utf8NoBOM
    } finally {
        Disconnect-YardstickGraph
    }
    [pscustomobject]@{ ChangePath = $changePath; SummaryPath = $summaryPath; Change = $change }
}

# ------------------------------------------------------------------ the approved change

function Get-ChangeSetAppFacts {
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)]$App)
    $parametersPath = Resolve-WorkItemPath -WorkItem $WorkItem -Relative $App.parameters
    $parameters = Read-Json -Path $parametersPath
    [pscustomobject]@{ Parameters = $parameters; ParametersPath = $parametersPath }
}

function Assert-ChangeInputsUnchanged {
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)]$App)
    $inputs = @(
        @{ Path = $App.packageFile; Hash = $App.packageSha256; Label = 'package' },
        @{ Path = $App.recipeFile; Hash = $App.recipeSha256; Label = 'recipe' },
        @{ Path = $App.testEvidence; Hash = $App.testEvidenceSha256; Label = 'test evidence' },
        @{ Path = $App.parameters; Hash = $App.parametersSha256; Label = 'parameters' }
    )
    if (Get-Prop $App 'detectionScript') { $inputs += @{ Path = $App.detectionScript; Hash = $App.detectionScriptSha256; Label = 'detection script' } }
    foreach ($input in $inputs) {
        $full = Resolve-WorkItemPath -WorkItem $WorkItem -Relative $input.Path
        if ((Get-FileSha256 -Path $full) -ne ([string]$input.Hash).ToUpperInvariant()) { throw "$($App.recipeId) $($input.Label) changed after approval." }
    }
}

function Test-DetectionRulesMatch {
    # Every property the upload set must be what Intune now holds.
    param([object[]]$Expected, [object[]]$Actual)
    if (@($Expected).Count -ne @($Actual).Count) { return $false }
    for ($i = 0; $i -lt @($Expected).Count; $i++) {
        $want = @($Expected)[$i]
        $have = @($Actual)[$i]
        foreach ($property in $want.PSObject.Properties) {
            if ($null -eq $property.Value -or "$($property.Value)" -eq '') { continue }
            $name = $property.Name
            $got = Get-Prop $have $name
            if ("$got" -ne "$($property.Value)") { return $false }
        }
    }
    $true
}

function Test-AppMetadata {
    # A fresh read of an uploaded app against what was approved. Returns the app and its content file.
    param([Parameter(Mandatory)]$App, [Parameter(Mandatory)]$Parameters, [Parameter(Mandatory)][string]$AppId, [bool]$AllowAvailableUninstall = $true)
    $actual = Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$AppId" -MaxAttempts 2
    $requirement = $Parameters.RequirementRule
    $problems = @()
    if ([string]$actual.id -ne $AppId) { $problems += 'app id' }
    if ($actual.displayName -ne $App.displayName) { $problems += 'display name' }
    if ($actual.displayVersion -ne $App.version) { $problems += 'version' }
    if ($actual.publisher -ne $App.publisher) { $problems += 'publisher' }
    if ($actual.installCommandLine -ne $App.installCommand) { $problems += 'install command' }
    if ($actual.uninstallCommandLine -ne $App.uninstallCommand) { $problems += 'uninstall command' }
    if ($actual.allowAvailableUninstall -ne $AllowAvailableUninstall) { $problems += 'allow available uninstall' }
    if ($actual.installExperience.runAsAccount -ne $Parameters.InstallExperience) { $problems += 'install experience' }
    if ($actual.installExperience.deviceRestartBehavior -ne $Parameters.RestartBehavior) { $problems += 'restart behavior' }
    if ($requirement.allowedArchitectures -and $actual.allowedArchitectures -ne $requirement.allowedArchitectures) { $problems += 'architecture' }
    if ($actual.minimumSupportedWindowsRelease -ne $requirement.minimumSupportedWindowsRelease) { $problems += 'minimum Windows release' }
    if ($actual.publishingState -ne 'published' -or [int]$actual.uploadState -ne 1 -or -not $actual.committedContentVersion) { $problems += 'publishing state' }
    if (-not (Test-DetectionRulesMatch -Expected @($Parameters.DetectionRule) -Actual @($actual.detectionRules))) { $problems += 'detection rules' }
    if ($problems.Count -gt 0) { throw "$($App.displayName) was not uploaded as approved: $($problems -join ', ')." }
    $relationships = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$AppId/relationships" -MaxAttempts 2)
    if ($relationships.Count -ne 0) { throw "$($App.displayName) unexpectedly has dependency or supersedence relationships." }
    $files = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$AppId/microsoft.graph.win32LobApp/contentVersions/$($actual.committedContentVersion)/files" -MaxAttempts 2)
    if ($files.Count -ne 1 -or $files[0].uploadState -ne 'commitFileSuccess' -or $files[0].isCommitted -ne $true) { throw "$($App.displayName) content commit verification failed." }
    [pscustomobject]@{ App = $actual; File = $files[0] }
}

function Test-AppAssignment {
    # The one Available assignment that was approved, and nothing else.
    param([Parameter(Mandatory)]$App, [Parameter(Mandatory)][string]$AppId, [Parameter(Mandatory)][string]$Notifications)
    $assignments = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$AppId/assignments" -MaxAttempts 2)
    if ($assignments.Count -ne 1) { throw "$($App.displayName) should have exactly one assignment but has $($assignments.Count)." }
    $one = $assignments[0]
    if ($one.intent -ne 'available' -or $one.target.'@odata.type' -ne '#microsoft.graph.groupAssignmentTarget' -or
        $one.target.groupId -ne $App.assignment.groupId -or $one.target.deviceAndAppManagementAssignmentFilterType -ne 'none' -or
        (Get-Prop $one.target 'deviceAndAppManagementAssignmentFilterId') -or $one.settings.notifications -ne $Notifications) {
        throw "$($App.displayName) assignment properties verification failed."
    }
    $one
}

function Get-CreatedRecord {
    param([Parameter(Mandatory)][string]$Output, [Parameter(Mandatory)][string]$RecipeId, [Parameter(Mandatory)][string]$ChangeSetSha256)
    $path = Join-Path $Output "$RecipeId-created.json"
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    $record = Read-Json -Path $path
    if (([string]$record.changeSetSha256).ToUpperInvariant() -ne $ChangeSetSha256.ToUpperInvariant()) {
        throw "$RecipeId was created under a different change set. Review it in Intune before going on."
    }
    $record
}

function Invoke-OneOffLiveChangeCore {
    # Creates, verifies and assigns. Assumes a connected session. Safe to run again
    # under the same approval: apps already created are verified, not uploaded twice.
    param(
        [Parameter(Mandatory)][string]$WorkItem,
        [Parameter(Mandatory)]$Change,
        [Parameter(Mandatory)][string]$ChangeSetSha256,
        [Parameter(Mandatory)][string]$TenantId
    )
    $output = Join-Path $WorkItem 'intune-upload'
    if ([string]$Change.tenantId -ne $TenantId) { throw 'The change set is for a different tenant.' }
    if ($Change.intent -ne 'available' -or $Change.noDelete -ne $true) { throw 'The change set intent or safety properties are unexpected.' }
    if (@($Change.apps).Count -eq 0) { throw 'The change set has no apps.' }
    $allowUninstall = [bool](Get-Prop $Change 'allowAvailableUninstall' $true)
    $notifications = [string](Get-Prop $Change 'notifications' 'showAll')

    $groups = @{}
    foreach ($app in $Change.apps) {
        if ($app.assignment.targetType -ne '#microsoft.graph.groupAssignmentTarget' -or $app.assignment.intent -ne 'available') {
            throw "$($app.displayName): only an Available assignment to a group is allowed."
        }
        if (-not $groups.ContainsKey($app.assignment.groupId)) {
            $groups[$app.assignment.groupId] = Get-OneOffGroup -GroupId $app.assignment.groupId
        }
        $group = $groups[$app.assignment.groupId]
        if (-not $group.Suitable -or $group.DisplayName -ne $app.assignment.groupDisplayName) {
            throw "Target group '$($app.assignment.groupDisplayName)' no longer matches what was approved."
        }
        Assert-ChangeInputsUnchanged -WorkItem $WorkItem -App $app
    }

    # The duplicate scan again, now. Apps this change already created don't count.
    $created = @{}
    foreach ($app in $Change.apps) {
        $record = Get-CreatedRecord -Output $output -RecipeId $app.recipeId -ChangeSetSha256 $ChangeSetSha256
        if ($record) { $created[$app.recipeId] = [string]$record.appId }
    }
    $patterns = @($Change.apps | ForEach-Object { @($_.searchPatterns) } | Where-Object { $_ } | Select-Object -Unique)
    if ($patterns.Count -gt 0) {
        $index = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps?$script:AppSelect" -MaxAttempts 2)
        $others = @(Select-InventoryCandidates -Index $index -Patterns $patterns | Where-Object { [string]$_.App.id -notin $created.Values })
        Write-Json -Path (Join-Path $output 'preupload-family-candidates.json') -Value @($others | ForEach-Object { $_.App })
        if ($others.Count -gt 0) { throw "A matching app appeared in Intune since the inventory: $((@($others | ForEach-Object { $_.App.displayName }) -join ', ')). Stopped." }
    }

    $createdRecords = [System.Collections.Generic.List[object]]::new()
    $assignedRecords = [System.Collections.Generic.List[object]]::new()
    try {
        # Stage 1: every app is created, or found already created, and verified.
        foreach ($app in $Change.apps) {
            $facts = Get-ChangeSetAppFacts -WorkItem $WorkItem -App $app
            if ($created.ContainsKey($app.recipeId)) {
                $appId = $created[$app.recipeId]
                Write-Host "$($app.displayName) $($app.version) was already created ($appId); verifying it."
            } else {
                Write-Host "Uploading $($app.displayName) $($app.version) as a new unassigned Win32 app."
                $parameters = [IO.File]::ReadAllText($facts.ParametersPath) | ConvertFrom-Json -AsHashtable -Depth 30
                $parameters['FilePath'] = Resolve-WorkItemPath -WorkItem $WorkItem -Relative $app.packageFile
                $new = Add-YardstickWin32App @parameters
                if (-not $new.id) { throw "$($app.displayName) upload returned no app ID." }
                $appId = [string]$new.id
                Write-Json -Path (Join-Path $output "$($app.recipeId)-created.json") -Value ([ordered]@{
                    recipeId = $app.recipeId; appId = $appId; createdUtc = [DateTime]::UtcNow.ToString('o'); changeSetSha256 = $ChangeSetSha256
                })
                $deadline = [DateTime]::UtcNow.AddMinutes(15)
                do {
                    $state = Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$appId" -MaxAttempts 2
                    if ($state.publishingState -eq 'published' -and [int]$state.uploadState -eq 1 -and $state.committedContentVersion) { break }
                    if ([DateTime]::UtcNow -gt $deadline) { throw "$($app.displayName) did not reach the published state in time." }
                    Start-Sleep -Seconds 5
                } while ($true)
            }
            $verified = Test-AppMetadata -App $app -Parameters $facts.Parameters -AppId $appId -AllowAvailableUninstall $allowUninstall
            Write-Json -Path (Join-Path $output "$($app.recipeId)-verified.json") -Value $verified.App
            $createdRecords.Add([pscustomobject]@{ recipeId = $app.recipeId; appId = $appId; displayName = $app.displayName })
        }

        # Stage 2: only now, once every upload is verified, the approved assignments.
        foreach ($record in $createdRecords) {
            $app = @($Change.apps | Where-Object { $_.recipeId -eq $record.recipeId })[0]
            $existing = @(Invoke-YardstickGraphRequest -Method Get -Resource "deviceAppManagement/mobileApps/$($record.appId)/assignments" -MaxAttempts 2)
            if ($existing.Count -eq 0) {
                Write-Host "Making $($record.displayName) Available to $($app.assignment.groupDisplayName)."
                $made = Add-YardstickWin32AppAssignmentGroup -Include -ID $record.appId -GroupID $app.assignment.groupId -Intent available -Notification $notifications
                if (-not $made.id) { throw "$($record.displayName) assignment returned no assignment ID." }
            }
            $one = Test-AppAssignment -App $app -AppId $record.appId -Notifications $notifications
            $assignedRecords.Add([pscustomobject]@{ recipeId = $record.recipeId; appId = $record.appId; assignmentId = $one.id; groupId = $one.target.groupId })
        }

        # A last fresh read after every write.
        $final = foreach ($record in $createdRecords) {
            $app = @($Change.apps | Where-Object { $_.recipeId -eq $record.recipeId })[0]
            $facts = Get-ChangeSetAppFacts -WorkItem $WorkItem -App $app
            $verified = Test-AppMetadata -App $app -Parameters $facts.Parameters -AppId $record.appId -AllowAvailableUninstall $allowUninstall
            $one = Test-AppAssignment -App $app -AppId $record.appId -Notifications $notifications
            [ordered]@{
                recipeId = $record.recipeId; appId = $record.appId; displayName = $verified.App.displayName; version = $verified.App.displayVersion
                publishingState = $verified.App.publishingState; committedContentVersion = $verified.App.committedContentVersion
                allowAvailableUninstall = $verified.App.allowAvailableUninstall
                assignment = [ordered]@{ id = $one.id; intent = $one.intent; groupId = $one.target.groupId; notifications = $one.settings.notifications }
                status = 'assigned'
            }
        }
        $result = [ordered]@{
            verifiedUtc = [DateTime]::UtcNow.ToString('o'); tenantId = $TenantId; approvedChangeSetSha256 = $ChangeSetSha256
            noDelete = $true; apps = @($final)
        }
        Write-Json -Path (Join-Path $output 'live-change-result.json') -Value $result
        $result
    } catch {
        Write-Json -Path (Join-Path $output 'live-change-failure.json') -Value ([ordered]@{
            failedUtc = [DateTime]::UtcNow.ToString('o'); error = $_.Exception.Message; deleteAttemptAllowed = $false
            createdRecords = @($createdRecords); assignedRecords = @($assignedRecords)
            howToResume = 'Run the step again under the same approval: apps already created are verified rather than uploaded again.'
        })
        throw
    }
}

function Invoke-OneOffLiveChange {
    param(
        [Parameter(Mandatory)][string]$WorkItem,
        [Parameter(Mandatory)][string]$ChangeSetSha256,
        [Parameter(Mandatory)][string]$TenantId,
        [string]$TenantName,
        [string]$CredentialTarget = 'Yardstick:IntuneGraph'
    )
    $changePath = Join-Path $WorkItem 'intune-upload\live-change-set.json'
    if ((Get-FileSha256 -Path $changePath) -ne $ChangeSetSha256.ToUpperInvariant()) { throw 'The change set is not the one that was approved.' }
    $change = Read-Json -Path $changePath
    Connect-OneOffTenant -Runtime (Join-Path $WorkItem 'runtime') -TenantId $TenantId -TenantName $TenantName -CredentialTarget $CredentialTarget | Out-Null
    try {
        Invoke-OneOffLiveChangeCore -WorkItem $WorkItem -Change $change -ChangeSetSha256 $ChangeSetSha256.ToUpperInvariant() -TenantId $TenantId
    } finally {
        Disconnect-YardstickGraph
    }
}

Export-ModuleMember -Function Get-OneOffSearchPatterns, Select-InventoryCandidates, Get-OneOffGroup, Get-OneOffInventoryData, Connect-OneOffTenant,
    New-OneOffInventory, Find-TestResult, Find-FullTestResult, New-OneOffLocalPackages, Get-RecipeAppSettings, New-OneOffDetectionRule, New-OneOffChangeSummary,
    New-OneOffLiveChange, Test-DetectionRulesMatch, Test-AppMetadata, Test-AppAssignment, Invoke-OneOffLiveChangeCore, Invoke-OneOffLiveChange,
    Resolve-RecipePlaceholders, Get-WorkItemProducts
