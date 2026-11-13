#Requires -Version 7.0
# The Studio's side of the plugin protocol: read the step's input, write its result,
# check an approval, and keep paths inside the work item.
Set-StrictMode -Version Latest

function Read-StudioInput {
    param([Parameter(Mandatory)][string]$Path)
    $text = [IO.File]::ReadAllText($Path)
    $text | ConvertFrom-Json -Depth 30
}

function Get-StudioSetting {
    param([Parameter(Mandatory)]$StudioInput, [Parameter(Mandatory)][string]$Name)
    $property = $StudioInput.settings.PSObject.Properties[$Name]
    if ($property -and $null -ne $property.Value -and "$($property.Value)" -ne '') { return $property.Value }
    return $null
}

function Get-StudioParam {
    param([Parameter(Mandatory)]$StudioInput, [Parameter(Mandatory)][string]$Name, $Default = $null)
    $property = $StudioInput.params.PSObject.Properties[$Name]
    if ($property -and $null -ne $property.Value) { return $property.Value }
    return $Default
}

function Write-StudioResult {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][ValidateSet('ok', 'warn', 'fail', 'blocked')][string]$Status,
        [Parameter(Mandatory)][string]$Summary,
        [hashtable]$Outputs = @{},
        [object[]]$Artifacts = @(),
        [object[]]$Rows = @()
    )
    [ordered]@{
        status    = $Status
        summary   = $Summary
        outputs   = $Outputs
        artifacts = @($Artifacts)
        rows      = @($Rows)
    } | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $Path -Encoding utf8NoBOM
}

function Get-FileSha256 {
    param([Parameter(Mandatory)][string]$Path)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Test-PathWithin {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Root)
    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $boundary = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    $candidate.Equals($boundary, [StringComparison]::OrdinalIgnoreCase) -or
        $candidate.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Resolve-WorkItemPath {
    # A work-item-relative path, refused if it escapes the work item.
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)][string]$Relative)
    if ([IO.Path]::IsPathRooted($Relative) -or $Relative -match '(^|[\\/])\.\.([\\/]|$)') {
        throw "'$Relative' is not a path inside the work item."
    }
    $full = [IO.Path]::GetFullPath((Join-Path $WorkItem $Relative))
    if (-not (Test-PathWithin -Path $full -Root $WorkItem)) { throw "'$Relative' is outside the work item." }
    $full
}

function ConvertTo-WorkItemRelative {
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)][string]$Path)
    $root = [IO.Path]::GetFullPath($WorkItem).TrimEnd('\')
    $full = [IO.Path]::GetFullPath($Path)
    if (-not (Test-PathWithin -Path $full -Root $root)) { throw "$Path is outside the work item." }
    $full.Substring($root.Length + 1)
}

function Assert-StudioApproval {
    # The approval the Studio recorded must be for the file as it is now. The
    # Studio checked this before starting the step; this is the independent second check.
    param(
        [Parameter(Mandatory)]$StudioInput,
        [Parameter(Mandatory)][string]$WorkItem,
        [Parameter(Mandatory)][string]$GateFile
    )
    $approval = $StudioInput.PSObject.Properties['approval']
    if (-not $approval -or -not $approval.Value) { throw 'This step needs your approval from the Studio first.' }
    $approval = $approval.Value
    if ($approval.file -ne $GateFile.Replace('\', '/') -and $approval.file -ne $GateFile) {
        throw "The approval is for '$($approval.file)', not '$GateFile'."
    }
    $path = Resolve-WorkItemPath -WorkItem $WorkItem -Relative $GateFile
    $actual = Get-FileSha256 -Path $path
    if ($actual -ne ([string]$approval.sha256).ToUpperInvariant()) {
        throw "$GateFile changed after it was approved. Review and approve it again."
    }
    $approval
}

function Invoke-StudioStep {
    # Runs $Body with the step's input; its return value is the result:
    # a hashtable with Status (default ok), Summary, Outputs, Artifacts and Rows.
    # An exception is a failed step.
    param(
        [Parameter(Mandatory)][string]$StudioInput,
        [Parameter(Mandatory)][string]$StudioResult,
        [Parameter(Mandatory)][scriptblock]$Body
    )
    $failed = $false
    try {
        $in = Read-StudioInput -Path $StudioInput
        $out = & $Body $in
        $status = if ($out.ContainsKey('Status')) { $out.Status } else { 'ok' }
        $arguments = @{ Path = $StudioResult; Status = $status; Summary = [string]$out.Summary }
        foreach ($key in 'Outputs', 'Artifacts', 'Rows') { if ($out.ContainsKey($key)) { $arguments[$key] = $out[$key] } }
        Write-StudioResult @arguments
    } catch {
        $failed = $true
        Write-StudioResult -Path $StudioResult -Status fail -Summary $_.Exception.Message
        [Console]::Error.WriteLine("ERROR: $($_.Exception.Message)")
    }
    if ($failed) { exit 1 }
    exit 0
}

function New-StudioArtifact {
    param([Parameter(Mandatory)][string]$WorkItem, [Parameter(Mandatory)][string]$Path, [string]$Label)
    [ordered]@{
        path   = ConvertTo-WorkItemRelative -WorkItem $WorkItem -Path $Path
        sha256 = Get-FileSha256 -Path $Path
        label  = $Label
    }
}

Export-ModuleMember -Function Read-StudioInput, Get-StudioSetting, Get-StudioParam, Write-StudioResult, Get-FileSha256,
    Test-PathWithin, Resolve-WorkItemPath, ConvertTo-WorkItemRelative, Assert-StudioApproval, Invoke-StudioStep, New-StudioArtifact
