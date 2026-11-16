#Requires -Version 7.0
BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\StudioPlugin.psm1') -Force
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\OneOffPackager.psm1') -Force
}

Describe 'search patterns' {
    It 'uses the triage patterns, and a flexible form of the name when there are none' {
        $products = @(
            [pscustomobject]@{ name = 'FastStone Image Viewer'; searchPatterns = @('fast\s*stone', 'fsviewer') },
            [pscustomobject]@{ name = 'Mplus Combo' }
        )
        Get-OneOffSearchPatterns -Products $products | Should -Be @('fast\s*stone', 'fsviewer', 'Mplus\s*Combo')
    }

    It 'names the product whose pattern is not a regular expression' {
        { Get-OneOffSearchPatterns -Products @([pscustomobject]@{ name = 'Broken'; searchPatterns = @('(unclosed') }) } | Should -Throw "*'Broken'*not a valid regular expression*"
    }

    It 'matches the name and publisher, case-insensitively, and says which patterns hit' {
        $index = @(
            [pscustomobject]@{ id = '1'; displayName = 'FastStone Image Viewer'; publisher = 'FastStone Corporation' },
            [pscustomobject]@{ id = '2'; displayName = 'Zoom Workplace'; publisher = 'Zoom' },
            [pscustomobject]@{ id = '3'; displayName = 'Photo tool'; publisher = 'IDPhotoCapture' }
        )
        $hits = @(Select-InventoryCandidates -Index $index -Patterns @('fast\s*stone', 'idphoto'))
        $hits.App.id | Should -Be @('1', '3')
        $hits[1].MatchedPatterns | Should -Be @('idphoto')
        @(Select-InventoryCandidates -Index @() -Patterns @('x')).Count | Should -Be 0
    }
}

Describe 'Resolve-RecipePlaceholders' {
    It 'fills the placeholders' {
        Resolve-RecipePlaceholders -Text '<filename> /S /v<version> /x <productcode>' -FileName 'a.exe' -Version '2.1' -ProductCode '{GUID}' | Should -Be 'a.exe /S /v2.1 /x {GUID}'
    }
    It 'refuses a placeholder it cannot fill' {
        { Resolve-RecipePlaceholders -Text 'msiexec /x <productcode>' -FileName 'a.msi' -Version '1' -What 'uninstall command' } | Should -Throw "*uninstall command*'<productcode>'*"
    }
}

Describe 'Find-TestResult' {
    BeforeEach { $t = New-TestWorkItem; $recipe = New-TestRecipe -Item $t -Id 'app-a' }
    AfterEach { Remove-TestWorkItem $t }

    It 'finds the newest run of the right mode for the recipe as it is' {
        New-TestFullRun -Item $t -Mode Prepare -RunId '20261113-100000-001' -Recipes @($recipe) | Out-Null
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-110000-001' -Recipes @($recipe) -Overall 'Fail' | Out-Null
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($recipe) -Overall 'Pass with warnings' | Out-Null
        $found = Find-TestResult -WorkItem $t.WorkItem -RecipePath $recipe -Mode Full
        $found.RunId | Should -Be '20261113-120000-001'
        $found.Result.Overall | Should -Be 'Pass with warnings'
        (Find-TestResult -WorkItem $t.WorkItem -RecipePath $recipe -Mode Prepare).RunId | Should -Be '20261113-100000-001'
    }

    It 'returns nothing when the recipe was never tested in that mode' {
        New-TestFullRun -Item $t -Mode Prepare -RunId '20261113-100000-001' -Recipes @($recipe) | Out-Null
        Find-TestResult -WorkItem $t.WorkItem -RecipePath $recipe -Mode Full | Should -BeNullOrEmpty
    }

    It 'refuses a recipe edited after its test' {
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($recipe) | Out-Null
        Add-Content -LiteralPath $recipe -Value '# edited'
        { Find-TestResult -WorkItem $t.WorkItem -RecipePath $recipe -Mode Full } | Should -Throw '*changed after its full test*'
    }
}

Describe 'Get-RecipeAppSettings' {
    BeforeAll { Import-Module powershell-yaml }
    BeforeEach { $t = New-TestWorkItem }
    AfterEach { Remove-TestWorkItem $t }

    It 'takes the app from the recipe, with Yardstick defaults' {
        $recipe = ConvertFrom-Yaml (Get-Content (New-TestRecipe -Item $t -Id 'app-a') -Raw)
        $package = [pscustomobject]@{ id = 'app-a'; version = '1.0'; setupFile = 'app-a-setup.exe'; productCode = '' }
        $s = Get-RecipeAppSettings -Recipe $recipe -Package $package
        $s.displayName | Should -Be 'Test App app-a'
        $s.installCommand | Should -Be 'app-a-setup.exe /S'
        $s.uninstallCommand | Should -Be 'powershell.exe -noprofile -executionpolicy bypass -file .\uninstall.ps1'
        ($s.architecture, $s.minOSVersion, $s.installExperience, $s.restartBehavior, $s.maximumInstallationTimeInMinutes) | Should -Be @('x64', 'W10_20H2', 'system', 'basedOnReturnCode', 60)
    }

    It 'refuses a recipe without an install command or description' {
        $recipe = ConvertFrom-Yaml (Get-Content (New-TestRecipe -Item $t -Id 'app-a') -Raw)
        $package = [pscustomobject]@{ id = 'app-a'; version = '1.0'; setupFile = 'x.exe'; productCode = '' }
        $noInstall = ConvertFrom-Yaml (Get-Content (New-TestRecipe -Item $t -Id 'app-b') -Raw); $noInstall.Remove('installScript')
        { Get-RecipeAppSettings -Recipe $noInstall -Package $package } | Should -Throw '*no install command*'
        $recipe.Remove('description')
        { Get-RecipeAppSettings -Recipe $recipe -Package $package } | Should -Throw '*no description*'
    }

    It 'refuses a command with a placeholder left' {
        $recipe = ConvertFrom-Yaml (Get-Content (New-TestRecipe -Item $t -Id 'app-a') -Raw)
        $recipe['uninstallScript'] = 'msiexec /x <productcode> /qn'
        $package = [pscustomobject]@{ id = 'app-a'; version = '1.0'; setupFile = 'x.msi'; productCode = '' }
        { Get-RecipeAppSettings -Recipe $recipe -Package $package } | Should -Throw "*'<productcode>'*"
    }
}

Describe 'New-OneOffLocalPackages' {
    BeforeEach {
        $t = New-TestWorkItem
        Install-FakeRuntimeModules -Item $t
        $a = New-TestRecipe -Item $t -Id 'app-a'
        $b = New-TestRecipe -Item $t -Id 'app-b'
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'packages the installer that passed the install test, and records everything' {
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($a) | Out-Null
        $built = New-OneOffLocalPackages -WorkItem $t.WorkItem -Recipes @('recipes\app-a.yaml')
        @($built.Packages).Count | Should -Be 1
        $record = $built.Packages[0]
        $record.id | Should -Be 'app-a'
        $record.packageFile | Should -Be 'local-packages\app-a\1.0\app-a-setup.intunewin'
        (Get-Content (Join-Path $t.WorkItem $record.packageFile) -Raw) | Should -Match 'uninstall.ps1'
        (Get-Content (Join-Path $t.WorkItem 'local-packages\staging\app-a\1.0\uninstall.ps1') -Raw) | Should -Match '^# guarded'
        $record.packageSha256 | Should -Be (Hash (Join-Path $t.WorkItem $record.packageFile))
        $manifest = Get-Content $built.ManifestPath -Raw | ConvertFrom-Json
        $manifest.packages[0].testOutcome | Should -Be 'Pass'
    }

    It 'skips a recipe without a passing install test, and says so' {
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($a) | Out-Null
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-130000-001' -Recipes @($b) -Overall 'Fail' | Out-Null
        $built = New-OneOffLocalPackages -WorkItem $t.WorkItem
        @($built.Packages).id | Should -Be @('app-a')
        $built.Skipped | Should -Be @("app-b (its install test was 'Fail')")
    }

    It 'fails when nothing can be packaged' {
        { New-OneOffLocalPackages -WorkItem $t.WorkItem } | Should -Throw '*Nothing to package*no install test*'
    }

    It 'refuses to overwrite a package and a tampered installer' {
        $summary = New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($a)
        New-OneOffLocalPackages -WorkItem $t.WorkItem | Out-Null
        { New-OneOffLocalPackages -WorkItem $t.WorkItem } | Should -Throw '*refusing to overwrite*'
        Remove-Item (Join-Path $t.WorkItem 'local-packages') -Recurse -Force
        $installer = (Get-Content $summary -Raw | ConvertFrom-Json).Results[0].Installer
        Add-Content -LiteralPath $installer -Value 'tampered'
        { New-OneOffLocalPackages -WorkItem $t.WorkItem } | Should -Throw '*no longer matches the tested hash*'
    }

    It 'keeps earlier packages in the manifest when it builds another' {
        New-TestFullRun -Item $t -Mode Full -RunId '20261113-120000-001' -Recipes @($a, $b) | Out-Null
        New-OneOffLocalPackages -WorkItem $t.WorkItem -Recipes @('recipes\app-a.yaml') | Out-Null
        New-OneOffLocalPackages -WorkItem $t.WorkItem -Recipes @('recipes\app-b.yaml') | Out-Null
        (Get-Content (Join-Path $t.WorkItem 'local-packages\package-manifest.json') -Raw | ConvertFrom-Json).packages.id | Should -Be @('app-a', 'app-b')
    }
}

Describe 'detection and the change summary' {
    It 'matches detection rules by every property that was set' {
        $expected = @([pscustomobject]@{ '@odata.type' = '#t'; scriptContent = 'QUJD'; enforceSignatureCheck = $false; productVersion = '' })
        Test-DetectionRulesMatch -Expected $expected -Actual @(@{ '@odata.type' = '#t'; scriptContent = 'QUJD'; enforceSignatureCheck = $false }) | Should -BeTrue
        Test-DetectionRulesMatch -Expected $expected -Actual @(@{ '@odata.type' = '#t'; scriptContent = 'XYZ'; enforceSignatureCheck = $false }) | Should -BeFalse
        Test-DetectionRulesMatch -Expected $expected -Actual @() | Should -BeFalse
    }

    It 'writes a summary with what the user must see' {
        $t = New-TestWorkItem
        $path = New-TestChangeSet -Item $t -Ids @('app-a')
        $change = Get-Content $path -Raw | ConvertFrom-Json
        $change | Add-Member -NotePropertyName deleteGuard -NotePropertyValue 'DELETE blocked.' -Force
        $change | Add-Member -NotePropertyName incompleteUploadRisk -NotePropertyValue 'May leave an object.' -Force
        $change | Add-Member -NotePropertyName inventoryCapturedUtc -NotePropertyValue 'now' -Force
        $change | Add-Member -NotePropertyName appsScanned -NotePropertyValue 12 -Force
        $change | Add-Member -NotePropertyName duplicateCandidates -NotePropertyValue 0 -Force
        $change.apps[0].warning = 'Unsigned installer.'
        $md = New-OneOffChangeSummary -Change $change
        $md | Should -Match 'Contoso'
        $md | Should -Match 'Testing Group'
        $md | Should -Match '22222222-2222-2222-2222-222222222222'
        $md | Should -Match 'Unsigned installer'
        $md | Should -Match 'available'
        Remove-TestWorkItem $t
    }
}

Describe 'Invoke-OneOffLiveChangeCore' {
    BeforeAll {
        # The Yardstick commands exist only in a runtime; stand-ins let Pester mock them.
        function global:Invoke-YardstickGraphRequest { param($Method, $Resource, $ApiVersion, $MaxAttempts) }
        function global:Add-YardstickWin32App { param($FilePath, $DisplayName, $Description, $Publisher, $AppVersion, $InstallCommandLine, $UninstallCommandLine, $DetectionRule, $RequirementRule, $InstallExperience, $RestartBehavior, $MaximumInstallationTimeInMinutes, [switch]$AllowAvailableUninstall, $Icon) }
        function global:Add-YardstickWin32AppAssignmentGroup { param([switch]$Include, $ID, $GroupID, $Intent, $Notification) }
        $tenant = '11111111-1111-1111-1111-111111111111'
    }
    AfterAll {
        Remove-Item function:global:Invoke-YardstickGraphRequest, function:global:Add-YardstickWin32App, function:global:Add-YardstickWin32AppAssignmentGroup -ErrorAction SilentlyContinue
        Remove-Variable FakeGraph -Scope Global -ErrorAction SilentlyContinue
    }
    BeforeEach {
        $t = New-TestWorkItem
        $graph = New-FakeGraph
        Register-FakeGraphMocks
        $changePath = New-TestChangeSet -Item $t -Ids @('app-a', 'app-b')
        $sha = Hash $changePath
        $run = { param($sha) Invoke-OneOffLiveChangeCore -WorkItem $t.WorkItem -Change (Get-Content $changePath -Raw | ConvertFrom-Json) -ChangeSetSha256 $sha -TenantId $tenant }
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'creates and verifies every app, then makes each Available to its group, and never deletes' {
        $result = & $run $sha
        @($result.apps).Count | Should -Be 2
        $result.apps.status | Should -Be @('assigned', 'assigned')
        $graph.Calls | Where-Object { $_ -like 'UPLOAD*' } | Should -HaveCount 2
        $assigns = @($graph.Calls | Where-Object { $_ -like 'ASSIGN*' })
        $assigns | Should -HaveCount 2
        $assigns | ForEach-Object { $_ | Should -Match '22222222-2222-2222-2222-222222222222 available$' }
        $graph.Calls | Where-Object { $_ -like 'Delete *' } | Should -BeNullOrEmpty
        # Uploads all happen before the first assignment.
        $last = ($graph.Calls | Select-String -Pattern '^UPLOAD' | Select-Object -Last 1).LineNumber
        $first = ($graph.Calls | Select-String -Pattern '^ASSIGN' | Select-Object -First 1).LineNumber
        $graph.Calls.IndexOf(($graph.Calls | Where-Object { $_ -like 'ASSIGN*' } | Select-Object -First 1)) | Should -BeGreaterThan $graph.Calls.IndexOf('UPLOAD Test App app-b')
        Test-Path (Join-Path $t.WorkItem 'intune-upload\live-change-result.json') | Should -BeTrue
        (Get-Content (Join-Path $t.WorkItem 'intune-upload\app-a-created.json') -Raw | ConvertFrom-Json).changeSetSha256 | Should -Be $sha.ToUpperInvariant()
    }

    It 'resumes under the same approval without uploading an app twice' {
        $graph.FailUploadFor = 'Test App app-b'
        { & $run $sha.ToUpperInvariant() } | Should -Throw '*Upload failed*'
        $failure = Get-Content (Join-Path $t.WorkItem 'intune-upload\live-change-failure.json') -Raw | ConvertFrom-Json
        $failure.deleteAttemptAllowed | Should -BeFalse
        @($failure.createdRecords).Count | Should -Be 1
        @($graph.Assignments.Keys).Count | Should -Be 0 -Because 'nothing is assigned until every upload is verified'

        $graph.FailUploadFor = $null
        $graph.Calls.Clear()
        $result = & $run $sha.ToUpperInvariant()
        @($result.apps).Count | Should -Be 2
        @($graph.Calls | Where-Object { $_ -like 'UPLOAD*' }) | Should -Be @('UPLOAD Test App app-b')
    }

    It 'can be run a third time after it finished, changing nothing' {
        & $run $sha.ToUpperInvariant() | Out-Null
        $graph.Calls.Clear()
        & $run $sha.ToUpperInvariant() | Out-Null
        $graph.Calls | Where-Object { $_ -like 'UPLOAD*' -or $_ -like 'ASSIGN*' } | Should -BeNullOrEmpty
    }

    It 'refuses a created record that belongs to a different change set' {
        & $run $sha | Out-Null
        { & $run ('F' * 64) } | Should -Throw '*different change set*'
    }

    It 'stops when an input changed after the approval' {
        Add-Content -LiteralPath (Join-Path $t.WorkItem 'intune-upload\app-a-upload-parameters.json') -Value ' '
        { & $run $sha } | Should -Throw '*app-a parameters changed after approval*'
        $graph.Calls | Where-Object { $_ -like 'UPLOAD*' } | Should -BeNullOrEmpty
    }

    It 'stops when the target group is no longer what was approved' {
        $graph.Groups['22222222-2222-2222-2222-222222222222'].displayName = 'Renamed Group'
        { & $run $sha } | Should -Throw '*no longer matches what was approved*'
        $graph.Groups['22222222-2222-2222-2222-222222222222'].displayName = 'Testing Group'
        $graph.Groups['22222222-2222-2222-2222-222222222222'].membershipRule = '(user.department -eq "x")'
        { & $run $sha } | Should -Throw '*no longer matches what was approved*'
    }

    It 'stops when a matching app appeared since the inventory' {
        $graph.Apps['x'] = [pscustomobject]@{ id = 'x'; displayName = 'Test App app-a (old)'; publisher = 'Somebody' }
        { & $run $sha } | Should -Throw '*matching app appeared*'
        $graph.Calls | Where-Object { $_ -like 'UPLOAD*' } | Should -BeNullOrEmpty
    }

    It 'refuses a change set for another tenant' {
        { Invoke-OneOffLiveChangeCore -WorkItem $t.WorkItem -Change (Get-Content $changePath -Raw | ConvertFrom-Json) -ChangeSetSha256 $sha -TenantId '99999999-9999-9999-9999-999999999999' } | Should -Throw '*different tenant*'
    }

    It 'refuses an assignment that is not an Available group assignment' {
        $change = Get-Content $changePath -Raw | ConvertFrom-Json
        $change.apps[0].assignment.targetType = '#microsoft.graph.allLicensedUsersAssignmentTarget'
        { Invoke-OneOffLiveChangeCore -WorkItem $t.WorkItem -Change $change -ChangeSetSha256 $sha -TenantId $tenant } | Should -Throw '*only an Available assignment to a group*'
        $change = Get-Content $changePath -Raw | ConvertFrom-Json
        $change.apps[0].assignment.intent = 'uninstall'
        { Invoke-OneOffLiveChangeCore -WorkItem $t.WorkItem -Change $change -ChangeSetSha256 $sha -TenantId $tenant } | Should -Throw '*only an Available assignment*'
    }

    It 'checks the restart behaviour in installExperience, where Intune keeps it' {
        $graph.RestartBehaviorReported = 'force'
        { & $run $sha } | Should -Throw '*restart behavior*'
        $graph.RestartBehaviorReported = $null
    }
}
