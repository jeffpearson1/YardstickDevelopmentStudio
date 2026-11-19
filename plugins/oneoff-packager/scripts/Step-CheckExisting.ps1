#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string]$StudioInput, [Parameter(Mandatory)][string]$StudioResult)
Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'OneOffPackager.psm1') -Force

# Before routing: which products already have a Yardstick recipe or an Intune app. Those move
# to completedProducts, where the user can still choose to package one. Read-only everywhere.
Invoke-StudioStep -StudioInput $StudioInput -StudioResult $StudioResult -Body {
    param($in)
    $workItem = $in.workItem
    $recipes = @(Get-OneOffRecipeIndex -YardstickDev (Get-StudioSetting $in 'yardstickDevPath'))

    $apps = $null
    $note = ''
    $tenantId = Get-StudioSetting $in 'tenantId'
    if (-not $tenantId) {
        $note = 'Intune was not checked: set the Tenant ID in the plugin settings.'
    } else {
        try {
            $found = Get-OneOffExistingIntuneApps -WorkItem $workItem -TenantId $tenantId -TenantName (Get-StudioSetting $in 'tenantName') `
                -CredentialTarget (Get-StudioSetting $in 'credentialTarget')
            $apps = @($found.Apps)
        } catch {
            $note = "Intune was not checked: $($_.Exception.Message)"
        }
    }
    $result = Update-OneOffExistingProducts -WorkItem $workItem -Recipes $recipes -IntuneApps $apps -IntuneNote $note

    $parts = @("Checked $($recipes.Count) Yardstick recipe(s)" + $(if ($null -ne $apps) { " and $($apps.Count) Intune app(s)." } else { '.' }))
    $parts += if ($result.Completed.Count) { "Already available: $($result.Completed -join ', ')." } else { 'No product already exists.' }
    if ($result.IntuneOnly.Count) { $parts += "In Intune without a Yardstick recipe: $($result.IntuneOnly -join ', ')." }
    if ($note) { $parts += $note }
    @{
        # Without Intune the answer is incomplete, so say so on the stage.
        Status    = if ($note -or $result.IntuneOnly.Count) { 'warn' } else { 'ok' }
        Summary   = $parts -join ' '
        Outputs   = @{ products = 'products.json' }
        Artifacts = @(New-StudioArtifact -WorkItem $workItem -Path (Join-Path $workItem 'products.json') -Label 'Products')
    }
}
