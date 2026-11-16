#Requires -Version 7.0
BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\StudioPlugin.psm1') -Force
}

Describe 'Write-StudioResult' {
    It 'writes the shape the Studio reads' {
        $path = Join-Path ([IO.Path]::GetTempPath()) "result-$([guid]::NewGuid()).json"
        Write-StudioResult -Path $path -Status warn -Summary 'Two warnings' -Outputs @{ a = 'x.json' } -Rows @(@{ name = 'n' })
        $r = Get-Content $path -Raw | ConvertFrom-Json
        $r.status | Should -Be 'warn'
        $r.summary | Should -Be 'Two warnings'
        $r.outputs.a | Should -Be 'x.json'
        @($r.rows).Count | Should -Be 1
        @($r.artifacts).Count | Should -Be 0
        Remove-Item $path
    }

    It 'rejects a status the Studio does not know' {
        { Write-StudioResult -Path 'x' -Status maybe -Summary 's' } | Should -Throw
    }
}

Describe 'Resolve-WorkItemPath and Test-PathWithin' {
    BeforeAll { $item = 'C:\Work\2026-11-13-test' }

    It 'resolves a path inside the work item' {
        Resolve-WorkItemPath -WorkItem $item -Relative 'recipes\a.yaml' | Should -Be 'C:\Work\2026-11-13-test\recipes\a.yaml'
        Resolve-WorkItemPath -WorkItem $item -Relative 'intune-upload/live-change-set.json' | Should -Be 'C:\Work\2026-11-13-test\intune-upload\live-change-set.json'
    }

    It 'refuses a path that leaves it' {
        foreach ($bad in '..\x', 'a\..\..\x', 'C:\Windows\x', '\\server\share\x', '/etc/passwd') {
            { Resolve-WorkItemPath -WorkItem $item -Relative $bad } | Should -Throw -Because $bad
        }
    }

    It 'compares whole path components' {
        Test-PathWithin -Path 'C:\Work\Item\a' -Root 'c:\work\item' | Should -BeTrue
        Test-PathWithin -Path 'C:\Work\Items' -Root 'C:\Work\Item' | Should -BeFalse
    }
}

Describe 'Assert-StudioApproval' {
    BeforeEach {
        $t = New-TestWorkItem
        $gate = 'intune-upload/live-change-set.json'
        New-Item -ItemType Directory -Path (Join-Path $t.WorkItem 'intune-upload') | Out-Null
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'intune-upload\live-change-set.json') -Value '{"apps":[]}'
        $sha = Hash (Join-Path $t.WorkItem 'intune-upload\live-change-set.json')
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'accepts the approval of the file as it is' {
        $in = [pscustomobject]@{ approval = [pscustomobject]@{ file = $gate; sha256 = $sha.ToLowerInvariant() } }
        (Assert-StudioApproval -StudioInput $in -WorkItem $t.WorkItem -GateFile $gate).sha256 | Should -Be $sha.ToLowerInvariant()
    }

    It 'refuses without an approval' {
        { Assert-StudioApproval -StudioInput ([pscustomobject]@{ approval = $null }) -WorkItem $t.WorkItem -GateFile $gate } | Should -Throw '*approval*'
        { Assert-StudioApproval -StudioInput ([pscustomobject]@{}) -WorkItem $t.WorkItem -GateFile $gate } | Should -Throw '*approval*'
    }

    It 'refuses an approval of another file' {
        $in = [pscustomobject]@{ approval = [pscustomobject]@{ file = 'other.json'; sha256 = $sha } }
        { Assert-StudioApproval -StudioInput $in -WorkItem $t.WorkItem -GateFile $gate } | Should -Throw "*not '$gate'*"
    }

    It 'refuses when the file changed after the approval' {
        $in = [pscustomobject]@{ approval = [pscustomobject]@{ file = $gate; sha256 = $sha } }
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'intune-upload\live-change-set.json') -Value '{"apps":[1]}'
        { Assert-StudioApproval -StudioInput $in -WorkItem $t.WorkItem -GateFile $gate } | Should -Throw '*changed after it was approved*'
    }
}

Describe 'the scripts' {
    It 'all parse' {
        $problems = Get-ChildItem (Join-Path (Get-PluginRoot) 'scripts') -Include *.ps1, *.psm1 -Recurse | ForEach-Object {
            $errors = $null
            [void][System.Management.Automation.Language.Parser]::ParseFile($_.FullName, [ref]$null, [ref]$errors)
            foreach ($e in $errors) { "$($_.Name):$($e.Extent.StartLineNumber): $($e.Message)" }
        }
        @($problems) | Should -BeNullOrEmpty
    }
}
