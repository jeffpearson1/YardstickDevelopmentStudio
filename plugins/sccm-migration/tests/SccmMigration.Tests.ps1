#Requires -Version 7.0
BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\StudioPlugin.psm1') -Force
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\SccmMigration.psm1') -Force
}

Describe 'WQL' {
    It 'escapes backslash and quote in a literal' {
        ConvertTo-WqlLiteral -Text "O'Brien\Tools" | Should -Be "O\'Brien\\Tools"
    }
    It 'makes LIKE wildcards in the text literal and matches anywhere' {
        ConvertTo-WqlLike -Text '100%_[x]' | Should -Be '%100[%][_][[]x]%'
        ConvertTo-WqlLike -Text "it's" | Should -Be "%it\'s%"
    }
    It 'accepts only a three-character site code' {
        Test-SccmSiteCode 'ABC' | Should -BeTrue
        Test-SccmSiteCode 'AB' | Should -BeFalse
        Test-SccmSiteCode 'A B' | Should -BeFalse
        { Get-SccmNamespace -SiteCode "ABC'; DROP" } | Should -Throw '*three-character*'
        Get-SccmNamespace -SiteCode 'abc' | Should -Be 'root\SMS\site_ABC'
    }
}

Describe 'Format-CommandLine' {
    It 'puts a command on one line' {
        (Format-CommandLine -Command "`"x.exe`" /VERYSILENT `n/NORESTART").Text | Should -Be '"x.exe" /VERYSILENT /NORESTART'
    }
    It 'removes values that look like secrets and says so' {
        $r = Format-CommandLine -Command 'setup.exe /S /password=Hunter2 -licensekey ABCD-1234 /D=C:\App'
        $r.Text | Should -Be 'setup.exe /S /password=<redacted> -licensekey <redacted> /D=C:\App'
        $r.Redacted | Should -BeTrue
        (Format-CommandLine -Command 'msiexec /i a.msi /qn').Redacted | Should -BeFalse
    }
}

Describe 'Read-SccmApplicationXml' {
    It 'reads a script deployment type with registry detection, like the real Spectragryph application' {
        $dt = New-DeploymentTypeXml -LogicalName 'DeploymentType_1' -Title 'Silent Install for Spectragryph' -Technology Script `
            -InstallCommand "`"spectragryph_install_1.0_dist.exe`" /VERYSILENT `n/NORESTART" -UninstallCommand '"C:\Program Files (x86)\Spectragryph\unins000.exe" /SILENT' `
            -Location '\\files.example.edu\apps\Spectragryph\v1.0.7\' -Files @('spectragryph_install_1.0_dist.exe') -DetectionBody (Get-RealDetectionXml)
        $app = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($dt) -Title 'Spectragryph 1.0.7')
        $app.title | Should -Be 'Spectragryph 1.0.7'
        $app.publisher | Should -Be 'Test Publisher'
        $app.deploymentTypes.Count | Should -Be 1
        $t = $app.deploymentTypes[0]
        $t.title | Should -Be 'Silent Install for Spectragryph'
        $t.technology | Should -Be 'Script'
        $t.executionContext | Should -Be 'System'
        $t.installCommand | Should -Be '"spectragryph_install_1.0_dist.exe" /VERYSILENT /NORESTART'
        $t.uninstallCommand | Should -Be '"C:\Program Files (x86)\Spectragryph\unins000.exe" /SILENT'
        $t.contentLocation | Should -Be '\\files.example.edu\apps\Spectragryph\v1.0.7\'
        $t.contentFiles[0].name | Should -Be 'spectragryph_install_1.0_dist.exe'
        $t.maxExecuteMinutes | Should -Be '15'
        @($t.requirements).Count | Should -Be 1
        $t.detection.kind | Should -Be 'enhanced'
        $source = $t.detection.enhanced.settings['RegSetting_b917b099-7deb-4a1c-a66b-e0809c064451']
        $source.Key | Should -Match 'Uninstall\\\{5032C752-8E23-46C7-9D34-7D652A415164\}_is1'
        $source.ValueName | Should -Be 'DisplayVersion'
        $source.Is64Bit | Should -Be 'true'
        $t.detection.enhanced.expression.operator | Should -Be 'Equals'
        $t.detection.enhanced.expression.operands[1].value | Should -Be 'V 1.0.7'
    }

    It 'maps that real detection to an Intune registry rule' {
        $dt = New-DeploymentTypeXml -DetectionBody (Get-RealDetectionXml)
        $app = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($dt))
        $m = ConvertTo-IntuneMapping -DeploymentType $app.deploymentTypes[0]
        $m.status | Should -Be 'mapped'
        $rule = $m.detectionRules[0]
        $rule.type | Should -Be 'registry'
        $rule.keyPath | Should -Be 'HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{5032C752-8E23-46C7-9D34-7D652A415164}_is1'
        ($rule.valueName, $rule.detectionType, $rule.operator, $rule.value, $rule.check32BitOn64System) | Should -Be @('DisplayVersion', 'string', 'equal', 'V 1.0.7', $false)
        $m.installExperience | Should -Be 'system'
        $m.maxInstallMinutes | Should -Be 15
        $m.notes -join ' ' | Should -Match 'requirement rule'
    }

    It 'reads an MSI deployment type and its product code' {
        $dt = New-DeploymentTypeXml -Technology MSI -InstallCommand 'msiexec /i "app.msi" /q' -UninstallCommand 'msiexec /x "{AAAAAAAA-1111-2222-3333-444444444444}" /q' `
            -CustomData '<ProductCode>{AAAAAAAA-1111-2222-3333-444444444444}</ProductCode><ProductVersion>2.5.1</ProductVersion>'
        $t = (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($dt))).deploymentTypes[0]
        $t.technology | Should -Be 'MSI'
        $t.detection.kind | Should -Be 'msi'
        $m = ConvertTo-IntuneMapping -DeploymentType $t
        $m.status | Should -Be 'mapped'
        $m.detectionRules[0].productCode | Should -Be '{AAAAAAAA-1111-2222-3333-444444444444}'
    }

    It 'takes a PowerShell detection script as is, and refuses a VBScript one' {
        $ps = New-DeploymentTypeXml -DetectionBody 'if (Test-Path "C:\App\app.exe") { "installed" }' -ScriptType PowerShell
        $m = ConvertTo-IntuneMapping -DeploymentType (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($ps))).deploymentTypes[0]
        $m.status | Should -Be 'mapped'
        $m.detectionRules[0].type | Should -Be 'script'
        $m.detectionRules[0].script | Should -Match 'Test-Path'

        $vbs = New-DeploymentTypeXml -DetectionBody 'WScript.Echo "installed"' -ScriptType VBScript
        $m = ConvertTo-IntuneMapping -DeploymentType (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($vbs))).deploymentTypes[0]
        $m.status | Should -Be 'unmapped'
        $m.unmapped[0] | Should -Match 'VBScript.*PowerShell'
    }

    It 'maps rules that must all match, and refuses an OR' {
        $settings = (New-RegistrySetting -Name 'R1') + (New-FileSetting -Name 'F1')
        $reg = New-Comparison -Operator GreaterEquals -Setting R1 -SourceType Registry -DataType Version -Constant '2.0'
        $file = New-Comparison -Operator GreaterThan -Setting F1 -SourceType File -Method Count -DataType Int64 -Constant '0'
        $and = New-EnhancedDetectionXml -Settings $settings -Expression "<Operator>And</Operator><Operands><Expression>$reg</Expression><Expression>$file</Expression></Operands>"
        $m = ConvertTo-IntuneMapping -DeploymentType (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml -DetectionBody $and)))).deploymentTypes[0]
        $m.status | Should -Be 'mapped'
        @($m.detectionRules).Count | Should -Be 2
        $m.detectionRules[0].detectionType | Should -Be 'version'
        $m.detectionRules[0].operator | Should -Be 'greaterThanOrEqual'
        ($m.detectionRules[1].type, $m.detectionRules[1].detectionType, $m.detectionRules[1].fileOrFolderName) | Should -Be @('file', 'exists', 'app.exe')
        $m.detection | Should -Be 'registry + file'

        $or = New-EnhancedDetectionXml -Settings $settings -Expression "<Operator>Or</Operator><Operands><Expression>$reg</Expression><Expression>$file</Expression></Operands>"
        $m = ConvertTo-IntuneMapping -DeploymentType (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml -DetectionBody $or)))).deploymentTypes[0]
        $m.status | Should -Be 'unmapped'
        $m.unmapped[0] | Should -Match 'OR or NOT'
    }

    It 'keeps what it can map and names what it cannot' {
        $settings = (New-RegistrySetting -Name 'R1') + (New-FileSetting -Name 'F1')
        $reg = New-Comparison -Operator Equals -Setting R1 -SourceType Registry -Constant '1.0'
        $odd = New-Comparison -Operator BeginsWith -Setting R1 -SourceType Registry -Constant '1.'
        $missing = New-Comparison -Operator Equals -Setting F1 -SourceType File -Method Count -DataType Int64 -Constant '0'
        $xml = New-EnhancedDetectionXml -Settings $settings -Expression "<Operator>And</Operator><Operands><Expression>$reg</Expression><Expression>$odd</Expression><Expression>$missing</Expression></Operands>"
        $m = ConvertTo-IntuneMapping -DeploymentType (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml -DetectionBody $xml)))).deploymentTypes[0]
        $m.status | Should -Be 'partial'
        @($m.detectionRules).Count | Should -Be 1
        $m.unmapped.Count | Should -Be 2
        $m.unmapped -join ' ' | Should -Match "BeginsWith"
        $m.unmapped -join ' ' | Should -Match 'absent'
    }

    It 'flags a deployment type with no detection, user context and dependencies' {
        $dt = New-DeploymentTypeXml -Context User -RequiresLogOn OnlyWhenUserLoggedOn -Extra '<Dependencies><DeploymentTypeRule><DeploymentTypeApplicationReference AuthoringScopeId="ScopeId_X" LogicalName="Application_9" Version="2" /></DeploymentTypeRule></Dependencies>'
        $t = (Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($dt))).deploymentTypes[0]
        $m = ConvertTo-IntuneMapping -DeploymentType $t
        $m.status | Should -Be 'unmapped'
        $m.installExperience | Should -Be 'user'
        $m.unmapped[0] | Should -Match 'no detection'
        ($m.notes -join ' ') | Should -Match 'user context'
        ($m.notes -join ' ') | Should -Match 'dependencies'
    }

    It 'says which deployment type to migrate when there are several, and that supersedence is not migrated' {
        $a = New-DeploymentTypeXml -LogicalName 'DT_A' -Title 'Second' -Priority '2'
        $b = New-DeploymentTypeXml -LogicalName 'DT_B' -Title 'First' -Priority '1'
        $sup = '<Supersedes><DeploymentTypeRule><DeploymentTypeApplicationReference AuthoringScopeId="S" LogicalName="Application_old" Version="1" /></DeploymentTypeRule></Supersedes>'
        $parsed = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($a, $b) -Supersedes $sup)
        $doc = New-IntuneMappingDocument -Application ([pscustomobject]@{ name = 'X'; version = '1'; manufacturer = 'M'; ciUniqueId = 'K' }) -Parsed $parsed
        $doc.primaryDeploymentType | Should -Be 'DT_B'
        $doc.deploymentTypes.Count | Should -Be 2
        ($doc.notes -join ' ') | Should -Match '2 deployment types'
        ($doc.notes -join ' ') | Should -Match 'Supersedence is not migrated'
    }

    It 'redacts secrets in commands and counts them' {
        $dt = New-DeploymentTypeXml -InstallCommand 'setup.exe /S /serial=ABC-123'
        $parsed = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @($dt))
        $parsed.deploymentTypes[0].installCommand | Should -Be 'setup.exe /S /serial=<redacted>'
        $parsed.redactedCommands | Should -Be 1
    }

    It 'returns the icon' {
        $parsed = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml)) -Icon "iVBO`nRw0K")
        $parsed.iconBase64 | Should -Be 'iVBORw0K'
    }
}

Describe 'the export document' {
    It 'names the folder from the name and version' {
        Get-ExportFolderName -Row ([pscustomobject]@{ name = 'Spectragryph optical spectroscopy'; version = '1.2.16.1' }) | Should -Be 'spectragryph-optical-spectroscopy-1-2-16-1'
        Get-ExportFolderName -Row @{ name = 'A/B: C'; version = '' } | Should -Be 'a-b-c'
        Get-ExportFolderName -Row ([pscustomobject]@{ name = 'Spectragryph 1.0.7'; version = '1.0.7' }) | Should -Be 'spectragryph-1-0-7' -Because 'the name already carries the version'
        { Get-ExportFolderName -Row ([pscustomobject]@{ name = ''; version = '1' }) } | Should -Throw '*no name*'
        (Get-ExportFolderName -Row @{ name = ('x' * 200); version = '1' }).Length | Should -BeLessOrEqual 60
    }

    It 'writes what the packager needs to read' {
        $parsed = Read-SccmApplicationXml -Xml (New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml -DetectionBody (Get-RealDetectionXml))) -Title 'Spectragryph 1.0.7')
        $application = [pscustomobject]@{ name = 'Spectragryph 1.0.7'; version = '1.0.7'; manufacturer = 'Dr. Menges'; ciUniqueId = 'K'; dateLastModified = '2025-01-27' }
        $mapping = New-IntuneMappingDocument -Application $application -Parsed $parsed
        $export = [pscustomobject]@{
            site = 'ABC'; smsProvider = 'cm.example.edu'; application = $application; description = 'Spectral analysis.'
            deployments = @([pscustomobject]@{ purpose = 'Available'; collection = 'All Labs'; desiredConfig = 'Install' })
            deploymentTypes = ($parsed.deploymentTypes | ConvertTo-Json -Depth 20 | ConvertFrom-Json -Depth 30)
        }
        $md = New-ExportMarkdown -Export $export -Mapping ($mapping | ConvertTo-Json -Depth 20 | ConvertFrom-Json -Depth 30)
        $md | Should -Match '^# Migrate Spectragryph 1.0.7 from Configuration Manager to Intune'
        $md | Should -Not -Match '1.0.7 1.0.7'
        $md | Should -Match 'Available: All Labs'
        $md | Should -Match '"setup.exe" /S'
        $md | Should -Match 'Detection in Intune: \*\*mapped\*\*'
        $md | Should -Match 'DisplayVersion'
        $md | Should -Match 'one-off package'
        $md | Should -Not -Match 'Hunter2'
    }
}

Describe 'talking to the site' {
    BeforeAll {
        # Real CIM instances: the code hands one back to Get-CimInstance -InputObject.
        $make = { param($p) New-CimInstance -ClassName SMS_Application -Namespace 'root/SMS/site_ABC' -ClientOnly -Property $p }
        $applications = @(
            (& $make @{ CI_ID = 1; CI_UniqueID = 'ScopeId_X/Application_1/8'; LocalizedDisplayName = 'Zoom 6.1'; SoftwareVersion = '6.1'; Manufacturer = 'Zoom'; IsDeployed = $true; IsExpired = $false; IsSuperseded = $false; NumberOfDeploymentTypes = 1; DateLastModified = [datetime]'2026-03-04' }),
            (& $make @{ CI_ID = 2; CI_UniqueID = 'ScopeId_X/Application_2/3'; LocalizedDisplayName = 'Adobe Reader'; SoftwareVersion = '24'; Manufacturer = 'Adobe'; IsDeployed = $false; IsExpired = $true; IsSuperseded = $false; NumberOfDeploymentTypes = 2; DateLastModified = [datetime]'2025-01-01' })
        )
        $lazy = $make
    }
    BeforeEach { $global:SccmQueries = [System.Collections.Generic.List[string]]::new() }
    AfterAll { Remove-Variable SccmQueries -Scope Global -ErrorAction SilentlyContinue }

    It 'searches the latest revisions by name or manufacturer, with the text escaped' {
        Mock -ModuleName SccmMigration Get-CimInstance { $global:SccmQueries.Add($Query); $applications }
        $found = Find-SccmApplications -Session 'session' -Namespace 'root\SMS\site_ABC' -Text "o'brien 100%"
        $q = $global:SccmQueries[0]
        $q | Should -Match 'FROM SMS_Application WHERE IsLatest = 1 AND \(LocalizedDisplayName LIKE ''%o\\''brien 100\[%\]%'' OR Manufacturer LIKE'
        $found.Total | Should -Be 2
        $found.Rows[0].name | Should -Be 'Adobe Reader'
        $found.Rows[0].state | Should -Be 'retired'
        $found.Rows[1].state | Should -Be 'active'
        $found.Rows[1].modified | Should -Be '2026-03-04'
        $found.Rows[1].key | Should -Be 'ScopeId_X/Application_1/8'
    }

    It 'lists every application for an empty search, and shows a folder that was exported before' {
        Mock -ModuleName SccmMigration Get-CimInstance { $global:SccmQueries.Add($Query); $applications }
        $root = New-TestRoot
        New-Item -ItemType Directory -Path (Join-Path $root 'zoom-6-1') | Out-Null
        Set-Content (Join-Path $root 'zoom-6-1\sccm-export.json') '{}'
        $found = Find-SccmApplications -Session 's' -Namespace 'n' -Text '' -WorkRoot $root
        $global:SccmQueries[0] | Should -Not -Match 'LIKE'
        ($found.Rows | Where-Object name -eq 'Zoom 6.1').folder | Should -Be 'zoom-6-1'
        $found.Rows | Where-Object name -eq 'Adobe Reader' | ForEach-Object { $_.Contains('folder') | Should -BeFalse }
        Remove-Item $root -Recurse -Force
    }

    It 'confirms the site and says when the provider does not know it' {
        Mock -ModuleName SccmMigration Get-CimInstance { [pscustomobject]@{ SiteCode = 'ABC'; SiteName = 'Main site'; Version = '5.00.9132'; ServerName = 'cm1' } }
        (Test-SccmConnection -Session 's' -SiteCode 'abc').SiteName | Should -Be 'Main site'
        { Test-SccmConnection -Session 's' -SiteCode 'XYZ' } | Should -Throw '*site XYZ is not one it knows*'
    }

    It 'exports an application: the lazy XML is fetched from the instance itself' {
        $xml = New-DigestXml -DeploymentTypes @((New-DeploymentTypeXml -DetectionBody (Get-RealDetectionXml))) -Title 'Zoom 6.1' -Icon 'iVBORw0K'
        Mock -ModuleName SccmMigration Get-CimInstance {
            # Pester 5 mocks don't fill $PSBoundParameters; a bound parameter is simply a variable.
            if ($null -ne (Get-Variable InputObject -ErrorAction Ignore)) {
                $global:SccmQueries.Add('GET instance')
                return (& $lazy @{ CI_ID = 1; CI_UniqueID = 'ScopeId_X/Application_1/8'; LocalizedDisplayName = 'Zoom 6.1'; SoftwareVersion = '6.1'; Manufacturer = 'Zoom'; IsDeployed = $true; IsExpired = $false; IsSuperseded = $false; DateLastModified = [datetime]'2026-03-04'; SDMPackageXML = $xml })
            }
            $global:SccmQueries.Add($Query)
            if ($Query -like '*SMS_ApplicationAssignment*') { return (New-CimInstance -ClassName SMS_ApplicationAssignment -Namespace 'root/SMS/site_ABC' -ClientOnly -Property @{ CollectionName = 'All Labs'; OfferTypeID = 2; DesiredConfigType = 1; Enabled = $true }) }
            $applications[0]
        }
        $root = New-TestRoot
        $made = Export-SccmApplication -Session 's' -Namespace 'n' -SiteCode 'abc' -Provider 'cm1' -WorkRoot $root -Row ([pscustomobject]@{ key = 'ScopeId_X/Application_1/8' })
        Split-Path $made.Folder -Leaf | Should -Be 'zoom-6-1'
        foreach ($file in 'sccm-export.json', 'intune-mapping.json', 'export.md', 'icon.png') { Test-Path (Join-Path $made.Folder $file) | Should -BeTrue -Because $file }
        $export = Get-Content (Join-Path $made.Folder 'sccm-export.json') -Raw | ConvertFrom-Json
        $export.site | Should -Be 'ABC'
        $export.deployments[0].purpose | Should -Be 'Available'
        $export.deploymentTypes[0].installCommand | Should -Be '"setup.exe" /S'
        (Get-Content (Join-Path $made.Folder 'intune-mapping.json') -Raw | ConvertFrom-Json).deploymentTypes[0].status | Should -Be 'mapped'
        [IO.File]::ReadAllBytes((Join-Path $made.Folder 'icon.png')).Length | Should -BeGreaterThan 0
        $global:SccmQueries | Should -Contain 'GET instance'
        @($global:SccmQueries | Where-Object { $_ -like '*CI_UniqueID = ''ScopeId_X/Application_1/8''*' }).Count | Should -BeGreaterThan 0
        Remove-Item $root -Recurse -Force
    }

    It 'refuses to export when the id does not name exactly one application' {
        Mock -ModuleName SccmMigration Get-CimInstance { @() }
        { Export-SccmApplication -Session 's' -Namespace 'n' -SiteCode 'abc' -Provider 'cm1' -WorkRoot $TestDrive -Row ([pscustomobject]@{ key = 'nope' }) } | Should -Throw '*found 0*'
    }
}

Describe 'Copy-SccmContent' {
    BeforeEach {
        $root = New-TestRoot
        $share = Join-Path $root 'share'
        New-Item -ItemType Directory -Path (Join-Path $share 'sub') -Force | Out-Null
        Set-Content (Join-Path $share 'setup.exe') 'setup bytes'
        Set-Content (Join-Path $share 'readme.txt') 'read me'
        Set-Content (Join-Path $share 'sub\data.bin') ('x' * 2048)
        $folder = Join-Path $root 'export'
        New-Item -ItemType Directory -Path $folder | Out-Null
        $export = [ordered]@{ application = @{ ciUniqueId = 'K' }; deploymentTypes = @(@{ logicalName = 'DT_1'; contentLocation = "$share\" }) }
        ConvertTo-Json -InputObject $export -Depth 6 | Set-Content (Join-Path $folder 'sccm-export.json')
    }
    AfterEach { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }

    It 'copies every file, verifies each, and records what the packager needs' {
        $m = Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" -AllowNonUnc
        $m.fileCount | Should -Be 3
        (Get-Content (Join-Path $folder 'source\sub\data.bin') -Raw).Length | Should -BeGreaterThan 2000
        $manifest = Get-Content (Join-Path $folder 'source-manifest.json') -Raw | ConvertFrom-Json
        $manifest.from | Should -Be "$share\"
        ($manifest.files | Where-Object path -eq 'setup.exe').sha256 | Should -Be (Get-FileHash (Join-Path $share 'setup.exe')).Hash
        ($manifest.files | Where-Object path -eq 'sub\data.bin').bytes | Should -BeGreaterThan 2000
        $manifest.totalBytes | Should -Be (($manifest.files | Measure-Object bytes -Sum).Sum)
    }

    It 'refuses when SCCM now points somewhere else' {
        { Copy-SccmContent -ExportFolder $folder -CurrentLocation '\\other\share\' -AllowNonUnc } | Should -Throw '*content location is now*'
        Test-Path (Join-Path $folder 'source') | Should -BeFalse
    }

    It 'refuses content that is over the limit, a local path, an existing copy, and a missing export' {
        { Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" -AllowNonUnc -MaxContentMB 0 } | Should -Throw '*over the 0 MB limit*'
        { Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" } | Should -Throw '*not a network share*'
        Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" -AllowNonUnc | Out-Null
        { Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" -AllowNonUnc } | Should -Throw '*refusing to overwrite*'
        { Copy-SccmContent -ExportFolder (Join-Path $root 'nothing') -CurrentLocation 'x' } | Should -Throw '*Export the application first*'
    }

    It 'says so when the share cannot be reached' {
        Remove-Item $share -Recurse -Force
        { Copy-SccmContent -ExportFolder $folder -CurrentLocation "$share\" -AllowNonUnc } | Should -Throw '*Cannot reach the content*'
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

Describe 'read-only towards Configuration Manager' {
    It 'never calls a cmdlet that changes the site' {
        $forbidden = '\b(Set-CimInstance|New-CimInstance|Remove-CimInstance|Invoke-CimMethod|Invoke-WmiMethod|Set-WmiInstance|Remove-WmiObject|Remove-CimSession\s+-Force)\b|\b(Set|New|Remove|Invoke|Add|Import|Move|Update|Publish|Start|Stop|Sync|Approve|Deny|Save|Reset|Block|Unblock)-CM[A-Z]'
        $found = Get-ChildItem (Join-Path (Get-PluginRoot) 'scripts') -Include *.ps1, *.psm1 -Recurse | ForEach-Object {
            $text = Get-Content $_.FullName -Raw
            foreach ($m in [regex]::Matches($text, $forbidden)) { "$($_.Name): $($m.Value)" }
        }
        @($found) | Should -BeNullOrEmpty
    }

    It 'only queries with SELECT' {
        $queries = Get-ChildItem (Join-Path (Get-PluginRoot) 'scripts') -Include *.psm1 -Recurse | ForEach-Object { Get-Content $_.FullName -Raw } |
            ForEach-Object { [regex]::Matches($_, '-Query\s+"([^"]+)"') } | ForEach-Object { $_.Groups[1].Value }
        @($queries).Count | Should -BeGreaterThan 3
        @($queries | Where-Object { $_ -notmatch '^SELECT ' }) | Should -BeNullOrEmpty
    }

    It 'shares the protocol module with the One-Off Packager byte for byte' {
        $one = Join-Path (Get-PluginRoot) '..\oneoff-packager\scripts\StudioPlugin.psm1'
        if (Test-Path $one) { (Get-FileHash (Join-Path (Get-PluginRoot) 'scripts\StudioPlugin.psm1')).Hash | Should -Be (Get-FileHash $one).Hash }
    }
}

Describe 'the steps' {
    BeforeEach { $root = New-TestRoot }
    AfterEach { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }

    It 'ask for the settings they need' {
        $pwsh = (Get-Process -Id $PID).Path
        foreach ($case in @(
            @{ Script = 'Step-Connect.ps1'; Message = 'SMS Provider server' },
            @{ Script = 'Step-Search.ps1'; Message = 'SMS Provider server' },
            @{ Script = 'Step-Export.ps1'; Message = 'SMS Provider server and site code' },
            @{ Script = 'Step-CopyContent.ps1'; Message = 'SMS Provider server and site code' }
        )) {
            $inputPath = Join-Path $root 'in.json'; $resultPath = Join-Path $root "$($case.Script).json"
            ConvertTo-Json -InputObject ([ordered]@{ schema = 1; step = 's'; workItem = $root; workRoot = $root; settings = @{}; params = @{}; studio = @{} }) | Set-Content $inputPath
            & $pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File (Join-Path (Get-PluginRoot) "scripts\$($case.Script)") -StudioInput $inputPath -StudioResult $resultPath 2>&1 | Out-Null
            $LASTEXITCODE | Should -Be 1 -Because $case.Script
            $r = Get-Content $resultPath -Raw | ConvertFrom-Json
            $r.status | Should -Be 'fail'
            $r.summary | Should -Match $case.Message
        }
    }

    It 'refuse a provider that is not a server name' {
        { Open-SccmSession -Provider 'x; Remove-Item *' } | Should -Throw '*not a server name*'
    }
}
