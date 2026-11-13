#Requires -Version 7.0
# Copies the selected application's content from its SCCM share into its export folder.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'SccmMigration.psm1') -Force

Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $provider = Get-StudioSetting $in 'smsProvider'
    $siteCode = Get-StudioSetting $in 'siteCode'
    if (-not $provider -or -not $siteCode) { throw 'Set the SMS Provider server and site code in the plugin settings.' }
    $protocol = [string](Get-StudioSetting $in 'cimProtocol'); if (-not $protocol) { $protocol = 'Dcom' }
    $max = 2048
    $setting = Get-StudioSetting $in 'maxContentMB'
    if ($setting) { if (-not [int]::TryParse([string]$setting, [ref]$max) -or $max -lt 1) { throw "'Largest content to copy' must be a whole number of MB." } }
    $selection = Get-StudioParam $in 'selection'
    if (-not $selection) { throw 'Select an application first.' }
    $workRoot = $in.workItem
    $folder = Join-Path $workRoot (Get-ExportFolderName -Row $selection)
    $exportPath = Join-Path $folder 'sccm-export.json'
    if (-not (Test-Path -LiteralPath $exportPath)) { throw 'Export the application first.' }
    $export = [IO.File]::ReadAllText($exportPath) | ConvertFrom-Json -Depth 30
    $dt = @($export.deploymentTypes) | Select-Object -First 1
    if (-not $dt) { throw 'The export has no deployment type.' }

    $session = Open-SccmSession -Provider $provider -Protocol $protocol
    try {
        $current = Get-SccmContentLocation -Session $session -Namespace (Get-SccmNamespace -SiteCode $siteCode) -CiUniqueId $export.application.ciUniqueId -LogicalName $dt.logicalName
    } finally {
        Remove-CimSession -CimSession $session -ErrorAction SilentlyContinue
    }
    $copied = Copy-SccmContent -ExportFolder $folder -CurrentLocation $current -LogicalName $dt.logicalName -MaxContentMB $max
    $unsigned = @($copied.files | Where-Object { $_.signatureStatus -and $_.signatureStatus -ne 'Valid' })
    $manifest = Join-Path $folder 'source-manifest.json'
    @{
        Status    = if ($unsigned.Count -gt 0) { 'warn' } else { 'ok' }
        Summary   = "Copied $($copied.fileCount) file(s), $([math]::Round($copied.totalBytes / 1MB, 1)) MB, each verified by SHA-256.$(if ($unsigned.Count -gt 0) { " $($unsigned.Count) signed-type file(s) are not validly signed." })"
        Outputs   = @{ folder = (Split-Path $folder -Leaf) }
        Artifacts = @([ordered]@{ path = "$(Split-Path $folder -Leaf)/source-manifest.json"; sha256 = Get-FileSha256 -Path $manifest; label = 'Source manifest' })
    }
}
