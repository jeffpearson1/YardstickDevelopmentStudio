#Requires -Version 7.0
# Writes the request the user approves before a real install on this PC. The approval is
# bound to this file's hash, and the file records the exact recipes, the host and what the
# Prepare test found, so the approval means that and nothing wider.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $recipes = @(Get-ChildItem -LiteralPath (Join-Path $workItem 'recipes') -Filter *.yaml -File -ErrorAction SilentlyContinue | Sort-Object Name)
    if ($recipes.Count -eq 0) { throw 'There are no recipes to test.' }

    $entries = foreach ($recipe in $recipes) {
        $prepared = Find-TestResult -WorkItem $workItem -RecipePath $recipe.FullName -Mode Prepare
        if (-not $prepared) { throw "$($recipe.BaseName) has no Prepare test for its current contents. Run the Prepare test first." }
        $result = $prepared.Result
        if ($result.Overall -match 'Fail|Blocked') { throw "$($recipe.BaseName)'s Prepare test was '$($result.Overall)'. Fix it first." }
        [ordered]@{
            id = $recipe.BaseName
            path = ConvertTo-WorkItemRelative -WorkItem $workItem -Path $recipe.FullName
            sha256 = Get-FileSha256 -Path $recipe.FullName
            version = [string]$result.Version
            installerSha256 = [string]$result.SHA256
            signatureStatus = [string]$result.SignatureStatus
            installCommand = [string]$result.InstallCommand
            uninstallCommand = [string]$result.UninstallCommand
            prepareRun = $prepared.RunId
        }
    }

    $yardstickDev = Get-StudioSetting $in 'yardstickDevPath'
    $commit = $null
    if ($yardstickDev -and (Test-Path -LiteralPath $yardstickDev)) {
        $commit = (& git -C $yardstickDev rev-parse HEAD 2>$null)
        if ($LASTEXITCODE -ne 0) { $commit = $null }
    }
    $request = [ordered]@{
        preparedUtc = [DateTime]::UtcNow.ToString('o')
        host = $env:COMPUTERNAME
        user = "$env:USERDOMAIN\$env:USERNAME"
        yardstickDevPath = $yardstickDev
        yardstickCommit = $commit
        recipes = @($entries)
    }
    $artifacts = Join-Path $workItem 'artifacts'
    New-Item -ItemType Directory -Path $artifacts -Force | Out-Null
    $jsonPath = Join-Path $artifacts 'full-test-request.json'
    $request | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding utf8NoBOM

    $lines = @('# Install test to approve', '',
        "This **installs, detects and uninstalls** the software below on **$($request.host)** (as $($request.user)), then looks for leftovers.",
        'A system-context test asks for one UAC prompt. An uninstall can remove software that was already on this PC, so the test refuses a recipe whose app is already installed.', '')
    foreach ($e in $entries) {
        $lines += "## $($e.id) $($e.version)"
        $lines += ''
        $lines += "- Installer SHA-256: ``$($e.installerSha256)``"
        $lines += "- Signature: $($e.signatureStatus)"
        $lines += "- Install: ``$($e.installCommand)``"
        $lines += "- Uninstall: ``$($e.uninstallCommand)``"
        $lines += "- Recipe SHA-256: ``$($e.sha256)``"
        if ($e.signatureStatus -and $e.signatureStatus -ne 'Valid') { $lines += "- **Warning:** the installer's signature status is $($e.signatureStatus)." }
        $lines += ''
    }
    $lines += "YardstickDev commit: $(if ($commit) { $commit } else { 'unknown' })"
    $mdPath = Join-Path $artifacts 'full-test-request.md'
    Set-Content -LiteralPath $mdPath -Value ($lines -join "`n") -Encoding utf8NoBOM
    @{
        Summary   = "Install-test request ready for $($entries.Count) recipe(s) on $($request.host)."
        Outputs   = @{ request = 'artifacts/full-test-request.json'; summary = 'artifacts/full-test-request.md' }
        Artifacts = @((New-StudioArtifact -WorkItem $workItem -Path $jsonPath -Label 'Install-test request'), (New-StudioArtifact -WorkItem $workItem -Path $mdPath -Label 'Request summary'))
    }
}
