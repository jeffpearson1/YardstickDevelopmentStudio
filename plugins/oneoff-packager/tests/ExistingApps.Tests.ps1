#Requires -Version 7.0
#Requires -Modules @{ ModuleName = 'Pester'; ModuleVersion = '5.0' }
# Before routing: products that already have a Yardstick recipe or an Intune app.

BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\OneOffPackager.psm1') -Force

    function New-TestYardstickDev {
        param($Item)
        $dev = Join-Path $Item.Root 'YardstickDev'
        foreach ($dir in 'Recipes\Development', 'Recipes\Jetbrains') { New-Item -ItemType Directory -Path (Join-Path $dev $dir) -Force | Out-Null }
        Set-Content -LiteralPath (Join-Path $dev 'Recipes\7zip.yaml') -Value "id: 7zip`ndisplayName: 7-Zip`npublisher: Igor Pavlov`n"
        Set-Content -LiteralPath (Join-Path $dev 'Recipes\Jetbrains\clion.yaml') -Value "id: clion`ndisplayName: 'CLion'`n"
        Set-Content -LiteralPath (Join-Path $dev 'Recipes\Development\brave.yaml') -Value "id: brave`ndisplayName: Brave`n"
        $dev
    }

    function Set-TestProducts {
        param($Item, $Products, $Completed = @())
        Set-TestJson -Path (Join-Path $Item.WorkItem 'products.json') -Value ([ordered]@{ products = @($Products); routedProducts = @(@{ id = 'qgis' }); completedProducts = @($Completed) })
    }

    function Get-TestProducts { param($Item) Get-Content (Join-Path $Item.WorkItem 'products.json') -Raw | ConvertFrom-Json }
}

Describe 'finding apps that already exist' {
    BeforeEach { $t = New-TestWorkItem; $dev = New-TestYardstickDev $t }
    AfterEach { Remove-TestWorkItem $t }

    It 'indexes shipped recipes and leaves development recipes out' {
        $index = @(Get-OneOffRecipeIndex -YardstickDev $dev)
        @($index.id | Sort-Object) | Should -Be @('7zip', 'clion')
        ($index | Where-Object id -eq 'clion').displayName | Should -Be 'CLion'
        @(Get-OneOffRecipeIndex -YardstickDev (Join-Path $t.Root 'missing')).Count | Should -Be 0
    }

    It 'matches a recipe by id or by name, ignoring punctuation' {
        $index = @(Get-OneOffRecipeIndex -YardstickDev $dev)
        @(Find-OneOffRecipeMatches -Product @{ id = 'sevenzip'; name = '7 Zip' } -Recipes $index).id | Should -Be '7zip'
        @(Find-OneOffRecipeMatches -Product @{ id = 'clion'; name = 'JetBrains CLion' } -Recipes $index).id | Should -Be 'clion'
        @(Find-OneOffRecipeMatches -Product @{ id = 'zipper'; name = '7-Zip Helper' } -Recipes $index).Count | Should -Be 0
    }

    It 'moves existing products to completedProducts and marks Intune apps without a recipe' {
        Set-TestProducts $t @(
            [ordered]@{ id = '7zip'; name = '7-Zip'; searchPatterns = @('7-?zip') },
            [ordered]@{ id = 'zoom'; name = 'Zoom Workplace'; searchPatterns = @('zoom') },
            [ordered]@{ id = 'faststone'; name = 'FastStone Image Viewer'; searchPatterns = @('fast\s*stone') })
        $apps = @(
            [pscustomobject]@{ id = 'a1'; displayName = '7-Zip 23.01'; publisher = 'Igor Pavlov'; isAssigned = $true },
            [pscustomobject]@{ id = 'a2'; displayName = 'Zoom Workplace'; publisher = 'Zoom'; isAssigned = $false })
        $result = Update-OneOffExistingProducts -WorkItem $t.WorkItem -Recipes @(Get-OneOffRecipeIndex -YardstickDev $dev) -IntuneApps $apps
        $result.Completed | Should -Be @('7-Zip', 'Zoom Workplace')
        $result.IntuneOnly | Should -Be @('Zoom Workplace')
        $doc = Get-TestProducts $t
        @($doc.products.id) | Should -Be @('faststone')
        @($doc.completedProducts.id) | Should -Be @('7zip', 'zoom')
        @($doc.routedProducts.id) | Should -Be @('qgis') -Because 'routed products are left alone'
        $zoom = $doc.completedProducts | Where-Object id -eq 'zoom'
        $zoom.flags[0].label | Should -Be 'In Intune, no Yardstick recipe'
        $zoom.flags[0].tone | Should -Be 'amber'
        $zoom.existing.intuneApps[0].id | Should -Be 'a2'
        $seven = $doc.completedProducts | Where-Object id -eq '7zip'
        @($seven.flags.label) | Should -Be @('Yardstick recipe: 7zip', 'In Intune')
        $seven.existing.recipes[0].path | Should -Be 'Recipes\7zip.yaml'
        @($doc.products[0].flags).Count | Should -Be 0
    }

    It 'keeps a product the user chose to package anyway' {
        Set-TestProducts $t @([ordered]@{ id = '7zip'; name = '7-Zip'; completedOverride = $true })
        $result = Update-OneOffExistingProducts -WorkItem $t.WorkItem -Recipes @(Get-OneOffRecipeIndex -YardstickDev $dev) -IntuneApps @()
        $result.Completed.Count | Should -Be 0
        $doc = Get-TestProducts $t
        $doc.products[0].id | Should -Be '7zip'
        $doc.products[0].flags[0].label | Should -Be 'Yardstick recipe: 7zip' -Because 'it is still marked'
    }

    It 'says when Intune was not checked, and still checks recipes' {
        Set-TestProducts $t @([ordered]@{ id = 'zoom'; name = 'Zoom' }, [ordered]@{ id = '7zip'; name = '7-Zip' })
        $result = Update-OneOffExistingProducts -WorkItem $t.WorkItem -Recipes @(Get-OneOffRecipeIndex -YardstickDev $dev) -IntuneApps $null -IntuneNote 'Intune was not checked: no credential.'
        $result.IntuneChecked | Should -BeFalse
        $doc = Get-TestProducts $t
        @($doc.completedProducts.id) | Should -Be @('7zip')
        $doc.products[0].flags[0].label | Should -Be 'Intune not checked'
        $doc.products[0].flags[0].detail | Should -Be 'Intune was not checked: no credential.'
    }

    It 'runs as a step without a tenant and reports what it found' {
        Set-TestProducts $t @([ordered]@{ id = 'clion'; name = 'CLion' }, [ordered]@{ id = 'zoom'; name = 'Zoom' })
        $r = Invoke-TestStep -Item $t -Script 'Step-CheckExisting.ps1' -Settings @{ yardstickDevPath = $dev }
        $r.ExitCode | Should -Be 0
        $r.Result.status | Should -Be 'warn'
        $r.Result.summary | Should -Be 'Checked 2 Yardstick recipe(s). Already available: CLion. Intune was not checked: set the Tenant ID in the plugin settings.'
    }
}
