#Requires -Version 7.0
BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    $scripts = Join-Path (Get-PluginRoot) 'scripts'
}

Describe 'the isolated runtime' {
    BeforeEach {
        $t = New-TestWorkItem
        $source = Join-Path $t.Root 'YardstickDev'
        New-FakeYardstickDev -Path $source
        $runtime = Join-Path $t.Root 'runtime'
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'is copied from a clean checkout with a guard that blocks DELETE before authentication' {
        $made = & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath $runtime
        $made.DeleteGuardInstalled | Should -BeTrue
        (Get-Content (Join-Path $runtime 'Modules\YardstickGraph.psm1') -Raw) | Should -Match 'ONEOFF_DELETE_BLOCKED'
        $proof = & (Join-Path $scripts 'Test-OneOffRuntimeSafety.ps1') -RuntimePath $runtime
        $proof.Safe | Should -BeTrue
        $proof.Proof | Should -Match 'DELETE blocked before authentication'
        $manifest = Get-Content (Join-Path $runtime '.oneoff-runtime.json') -Raw | ConvertFrom-Json
        $manifest.sourceWasClean | Should -BeTrue
        $manifest.sourceCommit | Should -Be (& git -C $source rev-parse HEAD)
        # The source was never touched.
        @(& git -C $source status --porcelain).Count | Should -Be 0
    }

    It 'refuses a dirty checkout' {
        Add-Content -LiteralPath (Join-Path $source 'Yardstick.ps1') -Value '# local edit'
        { & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath $runtime } | Should -Throw '*clean working tree*'
        Test-Path $runtime | Should -BeFalse
    }

    It 'refuses a destination inside the source, and one that is not empty' {
        { & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath (Join-Path $source 'runtime') } | Should -Throw '*must not be the Yardstick source*'
        New-Item -ItemType Directory -Path $runtime | Out-Null
        Set-Content -LiteralPath (Join-Path $runtime 'x.txt') -Value 'x'
        { & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath $runtime } | Should -Throw '*not empty*'
    }

    It 'refuses a source whose Graph module has no place for the guard' {
        Set-Content -LiteralPath (Join-Path $source 'Modules\YardstickGraph.psm1') -Value 'function Invoke-YardstickGraphRequest { }'
        & git -C $source -c user.name=t -c user.email=t@t commit -qam 'no marker' 2>&1 | Out-Null
        { & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath $runtime } | Should -Throw '*refusing to create an unguarded runtime*'
    }

    It 'fails the safety proof when the guard was removed or a DELETE path was added' {
        & (Join-Path $scripts 'New-OneOffYardstickRuntime.ps1') -YardstickDevPath $source -DestinationPath $runtime | Out-Null
        $graph = Join-Path $runtime 'Modules\YardstickGraph.psm1'
        $backup = "$graph.bak"
        Copy-Item -LiteralPath $graph -Destination $backup
        $original = [IO.File]::ReadAllText($graph)
        [IO.File]::WriteAllText($graph, $original.Replace('ONEOFF_DELETE_BLOCKED', 'something else'))
        { & (Join-Path $scripts 'Test-OneOffRuntimeSafety.ps1') -RuntimePath $runtime } | Should -Throw '*changed after the safety manifest*'
        Move-Item -LiteralPath $backup -Destination $graph -Force
        (Get-FileHash $graph).Hash | Should -Be (Get-Content (Join-Path $runtime '.oneoff-runtime.json') -Raw | ConvertFrom-Json).graphModuleHash
        Set-Content -LiteralPath (Join-Path $runtime 'Modules\Extra.psm1') -Value 'Invoke-RestMethod -Uri $u -Method Delete' -Encoding utf8NoBOM
        { & (Join-Path $scripts 'Test-OneOffRuntimeSafety.ps1') -RuntimePath $runtime } | Should -Throw '*direct HTTP DELETE path*'
    }
}

Describe 'the step protocol' {
    BeforeEach { $t = New-TestWorkItem }
    AfterEach { Remove-TestWorkItem $t }

    It 'reports a failure as a failed result and a non-zero exit' {
        $r = Invoke-TestStep -Item $t -Script 'Step-ParseTicket.ps1'
        $r.ExitCode | Should -Be 1
        $r.Result.status | Should -Be 'fail'
        $r.Result.summary | Should -Match 'no ticket.md'
    }

    It 'extracts links and targets from the ticket' {
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'ticket.md') -Value 'Hi, software is Zoom and Mplus. See [x](https://www.dropbox.com/s/a/Zoom.msi?dl=0) for NC-4214805.'
        $r = Invoke-TestStep -Item $t -Script 'Step-ParseTicket.ps1'
        $r.ExitCode | Should -Be 0
        $r.Result.status | Should -Be 'ok'
        $r.Result.summary | Should -Be '1 link(s), 1 Dropbox, 1 asset tag(s).'
        $parsed = Get-Content (Join-Path $t.WorkItem 'parsed-ticket.json') -Raw | ConvertFrom-Json
        $parsed.TargetStrings | Should -Be @('NC-4214805')
        $r.Result.artifacts[0].path | Should -Be 'parsed-ticket.json'
        $r.Result.artifacts[0].sha256 | Should -Be (Hash (Join-Path $t.WorkItem 'parsed-ticket.json'))
    }

    It 'reads the request of an SCCM migration from its export' {
        New-Item -ItemType Directory -Path (Join-Path $t.WorkItem 'intake') | Out-Null
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'intake\export.md') -Value '# Migrate Zoom 6.1 from Configuration Manager to Intune. Content: \\server\share\Zoom.'
        $r = Invoke-TestStep -Item $t -Script 'Step-ParseTicket.ps1'
        $r.ExitCode | Should -Be 0 -Because $r.Output
        (Get-Content (Join-Path $t.WorkItem 'ticket.md') -Raw) | Should -Match 'Migrate Zoom 6.1'
        Test-Path (Join-Path $t.WorkItem 'parsed-ticket.json') | Should -BeTrue
    }

    It 'needs the plugin settings before it touches the tenant' {
        $r = Invoke-TestStep -Item $t -Script 'Step-Inventory.ps1'
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'Tenant ID'
        $r = Invoke-TestStep -Item $t -Script 'Step-Runtime.ps1'
        $r.Result.summary | Should -Match 'YardstickDev folder'
    }
}

Describe 'the live-change step' {
    BeforeEach { $t = New-TestWorkItem; $changePath = New-TestChangeSet -Item $t -Ids @('app-a') }
    AfterEach { Remove-TestWorkItem $t }

    It 'will not run without the Studio''s approval, or with the wrong one' {
        $settings = @{ tenantId = '11111111-1111-1111-1111-111111111111' }
        $r = Invoke-TestStep -Item $t -Script 'Step-LiveChange.ps1' -Settings $settings
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'needs your approval'

        $wrong = @{ file = 'intune-upload/live-change-set.json'; sha256 = ('0' * 64) }
        $r = Invoke-TestStep -Item $t -Script 'Step-LiveChange.ps1' -Settings $settings -Approval $wrong
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'changed after it was approved'
        @(Get-ChildItem (Join-Path $t.WorkItem 'intune-upload') -Filter '*-created.json').Count | Should -Be 0
    }
}

Describe 'the install-test request and the install test' {
    BeforeEach {
        $t = New-TestWorkItem
        $recipe = New-TestRecipe -Item $t -Id 'app-a'
        New-TestFullRun -Item $t -Mode Prepare -RunId '20261113-100000-001' -Recipes @($recipe) | Out-Null
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'writes the request the user approves, naming this PC and the exact recipe' {
        $r = Invoke-TestStep -Item $t -Script 'Step-FullTestRequest.ps1'
        $r.ExitCode | Should -Be 0 -Because $r.Output
        $request = Get-Content (Join-Path $t.WorkItem 'artifacts\full-test-request.json') -Raw | ConvertFrom-Json
        $request.host | Should -Be $env:COMPUTERNAME
        $request.recipes[0].id | Should -Be 'app-a'
        $request.recipes[0].sha256 | Should -Be (Hash $recipe)
        $request.recipes[0].installCommand | Should -Be 'app-a-setup.exe /S'
        (Get-Content (Join-Path $t.WorkItem 'artifacts\full-test-request.md') -Raw) | Should -Match 'installs, detects and uninstalls'
    }

    It 'needs a Prepare test of the recipe as it is now' {
        Add-Content -LiteralPath $recipe -Value '# edited'
        $r = Invoke-TestStep -Item $t -Script 'Step-FullTestRequest.ps1'
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'changed after its prepare test'
    }

    It 'will not install without the approval of this request, or after the recipe changed' {
        Invoke-TestStep -Item $t -Script 'Step-FullTestRequest.ps1' | Out-Null
        $gate = 'artifacts/full-test-request.json'
        $settings = @{ yardstickDevPath = $t.Root }
        $r = Invoke-TestStep -Item $t -Script 'Step-PackageTest.ps1' -Params @{ mode = 'Full' } -Settings $settings
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'needs your approval'

        $approval = @{ file = $gate; sha256 = (Hash (Join-Path $t.WorkItem 'artifacts\full-test-request.json')) }
        Add-Content -LiteralPath $recipe -Value '# edited after approval'
        $r = Invoke-TestStep -Item $t -Script 'Step-PackageTest.ps1' -Params @{ mode = 'Full' } -Settings $settings -Approval $approval
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'changed after the install test was approved'
        Test-Path (Join-Path $t.WorkItem 'artifacts\package-tests\*') | Should -BeTrue
        @(Get-ChildItem (Join-Path $t.WorkItem 'artifacts\package-tests') -Directory).Count | Should -Be 1 -Because 'no new harness run started'
    }
}

Describe 'reading a ServiceNow export' {
    BeforeEach { $t = New-TestWorkItem }
    AfterEach { Remove-TestWorkItem $t }

    It 'reads the packaging handoff, the raw export and a csv' {
        $handoff = Join-Path $t.Root 'handoff.json'
        Set-TestJson -Path $handoff -Value @{ items = @(
            @{ number = 'TASK1'; software = 'Raptor'; short_description = 'Intune - Package Raptor'; description_verbatim = 'Add Raptor.'; supplied_urls = @('https://v.example/raptor.msi') },
            @{ number = 'TASK2'; software = ''; short_description = 'Intune - Package Zoom'; description_verbatim = 'Add Zoom.'; supplied_urls = @() }
        ) }
        $r = Invoke-TestStep -Item $t -Script 'Step-ImportServiceNow.ps1' -Params @{ file = $handoff } -Dir $t.WorkRoot
        $r.ExitCode | Should -Be 0 -Because $r.Output
        @($r.Result.rows).Count | Should -Be 2
        $r.Result.rows[0].key | Should -Be 'TASK1'
        $r.Result.rows[0].title | Should -Be 'Raptor'
        $r.Result.rows[0].text | Should -Match 'Add Raptor\.'
        $r.Result.rows[0].text | Should -Match 'https://v.example/raptor.msi'
        $r.Result.rows[1].title | Should -Be 'Intune - Package Zoom'

        $raw = Join-Path $t.Root 'raw.json'
        Set-TestJson -Path $raw -Value @{ records = @(@{ number = 'TASK9'; short_description = 'Intune - Package Printer drivers'; description = 'Need drivers.' }) }
        $r = Invoke-TestStep -Item $t -Script 'Step-ImportServiceNow.ps1' -Params @{ file = $raw } -Dir $t.WorkRoot
        $r.Result.rows[0].reference | Should -Be 'TASK9'
        $r.Result.rows[0].text | Should -Match 'Need drivers'

        $csv = Join-Path $t.Root 'list.csv'
        Set-Content -LiteralPath $csv -Value "number,short_description,description`nTASK5,Package 7-Zip,Please add 7-Zip"
        $r = Invoke-TestStep -Item $t -Script 'Step-ImportServiceNow.ps1' -Params @{ file = $csv } -Dir $t.WorkRoot
        $r.Result.rows[0].title | Should -Be 'Package 7-Zip'
    }

    It 'says so when there is nothing to import' {
        $empty = Join-Path $t.Root 'empty.json'
        Set-TestJson -Path $empty -Value @{ items = @() }
        $r = Invoke-TestStep -Item $t -Script 'Step-ImportServiceNow.ps1' -Params @{ file = $empty } -Dir $t.WorkRoot
        $r.ExitCode | Should -Be 1
        $r.Result.summary | Should -Match 'No tickets'
    }
}
