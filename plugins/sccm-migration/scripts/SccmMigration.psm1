#Requires -Version 7.0
# Reads application objects from a Configuration Manager site through the SMS Provider
# (WMI over CIM), turns an application's deployment types into something an Intune package can
# be made from, and copies its content. Nothing here writes to the site: it only queries.
Set-StrictMode -Version Latest

Import-Module (Join-Path $PSScriptRoot 'StudioPlugin.psm1')

$script:ApplicationProperties = 'CI_ID, CI_UniqueID, LocalizedDisplayName, SoftwareVersion, Manufacturer, IsDeployed, IsExpired, IsSuperseded, NumberOfDeploymentTypes, DateLastModified'
$script:ComparisonOperators = @{
    Equals = 'equal'; NotEquals = 'notEqual'; GreaterThan = 'greaterThan'; GreaterEquals = 'greaterThanOrEqual'
    LessThan = 'lessThan'; LessEquals = 'lessThanOrEqual'
}

# ------------------------------------------------------------------ WQL

function ConvertTo-WqlLiteral {
    # The inside of a WQL string literal: backslash and quote are escaped with a backslash.
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Text)
    $Text.Replace('\', '\\').Replace("'", "\'")
}

function ConvertTo-WqlLike {
    # The inside of a LIKE pattern that matches `Text` anywhere: % _ and [ are made literal.
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Text)
    $literal = ConvertTo-WqlLiteral -Text $Text
    $literal = [regex]::Replace($literal, '[%_\[]', { param($m) "[$($m.Value)]" })
    "%$literal%"
}

function Test-SccmSiteCode {
    param([string]$SiteCode)
    $SiteCode -match '^[A-Za-z0-9]{3}$'
}

# ------------------------------------------------------------------ the digest (SDMPackageXML)

$script:Digest = "*[local-name()='{0}']"

function Select-Elements {
    # A path of local names ignoring namespaces: '/' is a child, '//' any depth below the node.
    param([Parameter(Mandatory)]$Node, [Parameter(Mandatory)][string]$Path)
    $xpath = '.'
    foreach ($step in [regex]::Matches($Path, '(//?)([^/]+)')) { $xpath += $step.Groups[1].Value + ($script:Digest -f $step.Groups[2].Value) }
    @($Node.SelectNodes($xpath))
}

function Get-ElementText {
    param($Node)
    if ($null -eq $Node) { return '' }
    ([string]$Node.InnerText).Trim()
}

function Get-FirstText {
    param($Node, [string]$Path)
    if ($null -eq $Node) { return '' }
    $found = @(Select-Elements -Node $Node -Path $Path)
    if ($found.Count -eq 0) { return '' }
    Get-ElementText $found[0]
}

function Get-ArgBag {
    # Every <Arg Name="x">text</Arg> under a node, the first of each name.
    param($Node)
    $bag = @{}
    if ($null -eq $Node) { return $bag }
    foreach ($arg in Select-Elements -Node $Node -Path '//Arg') {
        $name = $arg.GetAttribute('Name')
        if ($name -and -not $bag.ContainsKey($name)) { $bag[$name] = Get-ElementText $arg }
    }
    $bag
}

function Get-LeafBag {
    # Every leaf element's text under a node, keyed by its name, the first of each.
    param($Node)
    $bag = @{}
    if ($null -eq $Node) { return $bag }
    foreach ($leaf in @($Node.SelectNodes(".//*[not(*)]"))) {
        if (-not $bag.ContainsKey($leaf.LocalName)) { $bag[$leaf.LocalName] = Get-ElementText $leaf }
    }
    $bag
}

function Format-CommandLine {
    # One line, and anything that looks like a secret removed: the export is shared with an agent.
    param([string]$Command)
    $one = [regex]::Replace($Command.Trim(), '\s*[\r\n]+\s*', ' ')
    $redacted = [regex]::Replace($one, '(?i)(\b(?:password|passwd|pwd|secret|token|apikey|api-key|licen[sc]e(?:key)?|serial(?:number)?|key)\s*[=:]\s*)("[^"]*"|\S+)', '$1<redacted>')
    $redacted = [regex]::Replace($redacted, '(?i)(\s[-/]{1,2}(?:password|passwd|pwd|secret|token|licen[sc]e(?:key)?|serial|key)\s+)("[^"]*"|\S+)', '$1<redacted>')
    [pscustomobject]@{ Text = $redacted; Redacted = ($redacted -ne $one) }
}

function ConvertFrom-RuleExpression {
    param([Parameter(Mandatory)]$Node)
    $operands = foreach ($child in @($Node.SelectNodes("*[local-name()='Operands']/*"))) {
        switch ($child.LocalName) {
            'Expression' { ConvertFrom-RuleExpression -Node $child }
            'SettingReference' {
                [ordered]@{
                    kind = 'setting'; logicalName = $child.GetAttribute('SettingLogicalName'); sourceType = $child.GetAttribute('SettingSourceType')
                    method = $child.GetAttribute('Method'); propertyPath = $child.GetAttribute('PropertyPath'); dataType = $child.GetAttribute('DataType')
                }
            }
            'ConstantValue' { [ordered]@{ kind = 'constant'; value = $child.GetAttribute('Value'); dataType = $child.GetAttribute('DataType') } }
            'ConstantValueList' {
                [ordered]@{ kind = 'constantList'; values = @($child.SelectNodes("*[local-name()='ConstantValue']") | ForEach-Object { $_.GetAttribute('Value') }) }
            }
            default { [ordered]@{ kind = $child.LocalName } }
        }
    }
    [ordered]@{ kind = 'expression'; operator = Get-FirstText $Node '/Operator'; operands = @($operands) }
}

function ConvertFrom-EnhancedDetection {
    # The EnhancedDetectionMethod document: where each setting is read from, and the rule over them.
    param([Parameter(Mandatory)][string]$Text)
    $clean = [regex]::Replace($Text.Trim(), '^<\?xml[^>]*\?>', '')
    $doc = [xml]$clean
    $settings = [ordered]@{}
    foreach ($setting in @($doc.DocumentElement.SelectNodes("*[local-name()='Settings']/*"))) {
        $name = $setting.GetAttribute('LogicalName')
        $source = @($setting.SelectNodes("*[not(local-name()='Annotation')]")) | Select-Object -First 1
        if (-not $source) { continue }
        $info = [ordered]@{ kind = $source.LocalName; dataType = $setting.GetAttribute('DataType') }
        foreach ($attribute in $source.Attributes) { $info[$attribute.LocalName] = $attribute.Value }
        foreach ($child in @($source.SelectNodes('*'))) { if ($child.SelectNodes('*').Count -eq 0) { $info[$child.LocalName] = Get-ElementText $child } }
        $settings[$name] = $info
    }
    $rule = @($doc.DocumentElement.SelectNodes("*[local-name()='Rule']/*[local-name()='Expression']")) | Select-Object -First 1
    [ordered]@{ settings = $settings; expression = if ($rule) { ConvertFrom-RuleExpression -Node $rule } }
}

function Read-DeploymentTypeDetection {
    param([Parameter(Mandatory)]$DeploymentType, [Parameter(Mandatory)][string]$Technology)
    $detectAction = @(Select-Elements -Node $DeploymentType -Path '//DetectAction') | Select-Object -First 1
    $bag = Get-ArgBag -Node $detectAction
    $body = if ($bag.ContainsKey('MethodBody')) { $bag['MethodBody'] } else { '' }
    if ($body -match '<EnhancedDetectionMethod') {
        return [ordered]@{ kind = 'enhanced'; enhanced = ConvertFrom-EnhancedDetection -Text $body }
    }
    if ($body) {
        $type = if ($bag.ContainsKey('ScriptType')) { $bag['ScriptType'] } else { '' }
        if (-not $type -and $body -match '(?im)^\s*(Dim |WScript\.|Set \w+\s*=)') { $type = 'VBScript' }
        if (-not $type) { $type = 'PowerShell' }
        return [ordered]@{ kind = 'script'; scriptType = $type; script = $body }
    }
    $custom = Get-LeafBag -Node (@(Select-Elements -Node $DeploymentType -Path '//CustomData') | Select-Object -First 1)
    if ($Technology -eq 'MSI' -and $custom.ContainsKey('ProductCode') -and $custom['ProductCode']) {
        return [ordered]@{ kind = 'msi'; productCode = $custom['ProductCode']; productVersion = if ($custom.ContainsKey('ProductVersion')) { $custom['ProductVersion'] } else { '' } }
    }
    [ordered]@{ kind = 'none' }
}

function Read-SccmApplicationXml {
    # SDMPackageXML to a plain object: the application's display info and every deployment type
    # with its commands, behaviour, content, detection, requirements and relationships.
    param([Parameter(Mandatory)][string]$Xml)
    $clean = [regex]::Replace($Xml.Trim(), '^<\?xml[^>]*\?>', '')
    $doc = [xml]$clean
    $root = $doc.DocumentElement
    $info = @(Select-Elements -Node $root -Path '//DisplayInfo/Info') | Select-Object -First 1
    $icon = @(Select-Elements -Node $root -Path '//Resources/Icon/Data') | Select-Object -First 1
    $redactions = 0

    $types = foreach ($dt in @(Select-Elements -Node $root -Path '//DeploymentTypes/DeploymentType')) {
        $installer = @(Select-Elements -Node $dt -Path '/Installer') | Select-Object -First 1
        $technology = Get-FirstText $dt '/Technology'
        if (-not $technology -and $installer) { $technology = $installer.GetAttribute('Technology') }
        $install = Get-ArgBag -Node (@(Select-Elements -Node $dt -Path '//InstallAction') | Select-Object -First 1)
        $uninstall = Get-ArgBag -Node (@(Select-Elements -Node $dt -Path '//UninstallAction') | Select-Object -First 1)
        $custom = Get-LeafBag -Node (@(Select-Elements -Node $dt -Path '//CustomData') | Select-Object -First 1)
        $pick = { param($bag, [string[]]$names) foreach ($n in $names) { if ($bag.ContainsKey($n) -and $bag[$n]) { return $bag[$n] } }; '' }
        $installCommand = (& $pick $install @('InstallCommandLine')); if (-not $installCommand) { $installCommand = & $pick $custom @('InstallCommandLine') }
        $uninstallCommand = (& $pick $uninstall @('UninstallCommandLine', 'InstallCommandLine')); if (-not $uninstallCommand) { $uninstallCommand = & $pick $custom @('UninstallCommandLine') }
        $repairCommand = & $pick $custom @('RepairCommandLine')
        $commands = foreach ($c in $installCommand, $uninstallCommand, $repairCommand) { Format-CommandLine -Command $c }
        $redactions += @($commands | Where-Object { $_.Redacted }).Count
        $value = { param([string[]]$names) foreach ($bag in $install, $uninstall, $custom) { $v = & $pick $bag $names; if ($v) { return $v } }; '' }

        $contentNode = @(Select-Elements -Node $dt -Path '//Content') | Where-Object { Get-FirstText $_ '/Location' } | Select-Object -First 1
        $location = if ($contentNode) { Get-FirstText $contentNode '/Location' } else { '' }
        $files = @(if ($contentNode) { Select-Elements -Node $contentNode -Path '/File' | ForEach-Object { [ordered]@{ name = $_.GetAttribute('Name'); bytes = $_.GetAttribute('Size') } } })
        $exitCodes = @(Select-Elements -Node $dt -Path '//ExitCode' | ForEach-Object {
            $code = $_.GetAttribute('Code'); if (-not $code) { $code = Get-FirstText $_ '/Code' }
            $class = $_.GetAttribute('Class'); if (-not $class) { $class = Get-FirstText $_ '/Class' }
            [ordered]@{ code = $code; class = $class }
        })
        $requirements = @(Select-Elements -Node $dt -Path '/Requirements/Rule' | ForEach-Object {
            $expression = @($_.SelectNodes("*[local-name()='Expression']")) | Select-Object -First 1
            [ordered]@{
                id = $_.GetAttribute('id')
                name = (@($_.SelectNodes("*[local-name()='Annotation']/*[local-name()='DisplayName']")) | Select-Object -First 1 | ForEach-Object { $_.GetAttribute('Text') })
                expression = if ($expression) { ConvertFrom-RuleExpression -Node $expression }
            }
        })
        $dependencies = @(Select-Elements -Node $dt -Path '/Dependencies//DeploymentTypeApplicationReference' | ForEach-Object {
            [ordered]@{ authoringScopeId = $_.GetAttribute('AuthoringScopeId'); logicalName = $_.GetAttribute('LogicalName'); version = $_.GetAttribute('Version') }
        })
        [ordered]@{
            logicalName = $dt.GetAttribute('LogicalName')
            title = Get-FirstText $dt '/Title'
            description = Get-FirstText $dt '/Description'
            technology = $technology
            priority = $dt.GetAttribute('Priority')
            executionContext = (& $value @('ExecutionContext'))
            requiresLogOn = (& $value @('RequiresLogOn'))
            userInteractionMode = (& $value @('UserInteractionMode'))
            maxExecuteMinutes = (& $value @('MaxExecuteTime'))
            estimatedMinutes = (& $value @('ExecuteTime'))
            runAs32Bit = (& $value @('RunAs32Bit'))
            installCommand = $commands[0].Text
            uninstallCommand = $commands[1].Text
            repairCommand = $commands[2].Text
            contentId = if ($contentNode) { $contentNode.GetAttribute('ContentId') } else { '' }
            contentLocation = $location
            contentFiles = $files
            exitCodes = $exitCodes
            detection = Read-DeploymentTypeDetection -DeploymentType $dt -Technology $technology
            requirements = $requirements
            dependencies = $dependencies
        }
    }
    $supersedes = @(Select-Elements -Node $root -Path '//Application/Supersedes//DeploymentTypeApplicationReference' | ForEach-Object {
        [ordered]@{ authoringScopeId = $_.GetAttribute('AuthoringScopeId'); logicalName = $_.GetAttribute('LogicalName'); version = $_.GetAttribute('Version') }
    })
    [ordered]@{
        title = Get-FirstText $info '/Title'
        publisher = Get-FirstText $info '/Publisher'
        description = Get-FirstText $info '/Description'
        iconBase64 = if ($icon) { (Get-ElementText $icon) -replace '\s', '' } else { '' }
        deploymentTypes = @($types)
        supersedes = $supersedes
        redactedCommands = $redactions
    }
}

# ------------------------------------------------------------------ Intune mapping

function ConvertTo-IntuneRule {
    # One comparison leaf to an Intune detection rule, or the reason it can't be one.
    param([Parameter(Mandatory)]$Leaf, [Parameter(Mandatory)]$Settings)
    $setting = @($Leaf.operands | Where-Object { $_.kind -eq 'setting' }) | Select-Object -First 1
    $constant = @($Leaf.operands | Where-Object { $_.kind -eq 'constant' }) | Select-Object -First 1
    if (-not $setting) { return @{ unmapped = "Rule '$($Leaf.operator)' has no setting to read." } }
    $source = if ($Settings.Contains($setting.logicalName)) { $Settings[$setting.logicalName] } else { $null }
    $operator = $Leaf.operator
    $exists = ($setting.method -eq 'Count') -and (
        ($operator -eq 'GreaterThan' -and $constant -and $constant.value -eq '0') -or ($operator -eq 'GreaterEquals' -and $constant -and $constant.value -eq '1'))
    if ($setting.method -eq 'Count' -and -not $exists) { return @{ unmapped = 'Detects that something is absent or counts it; Intune rules only detect presence.' } }
    if (-not $exists -and -not $script:ComparisonOperators.ContainsKey($operator)) {
        return @{ unmapped = "Operator '$operator' has no Intune equivalent." }
    }
    switch ($setting.sourceType) {
        'Registry' {
            if (-not $source) { return @{ unmapped = "Registry setting $($setting.logicalName) is not described in the detection method." } }
            $key = "$($source['Hive'])\$($source['Key'])"
            $rule = [ordered]@{
                type = 'registry'; keyPath = $key; valueName = if ($source.Contains('ValueName')) { $source['ValueName'] } else { '' }
                check32BitOn64System = ($source.Contains('Is64Bit') -and $source['Is64Bit'] -ne 'true')
            }
            if ($exists) { $rule.detectionType = 'exists'; return @{ rule = $rule } }
            $type = switch ($setting.dataType) { 'String' { 'string' } 'Int64' { 'integer' } 'Integer' { 'integer' } 'Version' { 'version' } default { $null } }
            if (-not $type) { return @{ unmapped = "Registry value type '$($setting.dataType)' can't be compared in Intune." } }
            $rule.detectionType = $type; $rule.operator = $script:ComparisonOperators[$operator]; $rule.value = $constant.value
            @{ rule = $rule }
        }
        { $_ -in 'File', 'Folder' } {
            if (-not $source) { return @{ unmapped = "File setting $($setting.logicalName) is not described in the detection method." } }
            $rule = [ordered]@{
                type = 'file'; path = if ($source.Contains('Path')) { $source['Path'] } else { '' }
                fileOrFolderName = if ($source.Contains('Filter')) { $source['Filter'] } else { '' }
                check32BitOn64System = ($source.Contains('Is64Bit') -and $source['Is64Bit'] -ne 'true')
            }
            if ($exists) { $rule.detectionType = 'exists'; return @{ rule = $rule } }
            $property = $setting.propertyPath
            if ($property -in 'Version', 'FileVersion') {
                $rule.detectionType = 'version'; $rule.operator = $script:ComparisonOperators[$operator]; $rule.value = $constant.value
                return @{ rule = $rule }
            }
            @{ unmapped = "File property '$property' is not something this plugin maps; write the check as a PowerShell script." }
        }
        'MSI' {
            $code = if ($source -and $source.Contains('ProductCode')) { $source['ProductCode'] } else { '' }
            if (-not $code) { return @{ unmapped = 'MSI detection without a product code.' } }
            $rule = [ordered]@{ type = 'msi'; productCode = $code; productVersionOperator = 'notConfigured'; productVersion = '' }
            if ($constant -and $constant.value -and -not $exists) { $rule.productVersionOperator = $script:ComparisonOperators[$operator]; $rule.productVersion = $constant.value }
            @{ rule = $rule }
        }
        default { @{ unmapped = "Detection source '$($setting.sourceType)' is not supported." } }
    }
}

function Get-ExpressionLeaves {
    # The comparisons of an All-must-match expression, or $null when it contains an OR or NOT.
    param($Expression)
    if ($Expression.operator -eq 'And') {
        $leaves = @()
        foreach ($operand in $Expression.operands) {
            if ($operand.kind -ne 'expression') { return $null }
            $inner = Get-ExpressionLeaves -Expression $operand
            if ($null -eq $inner) { return $null }
            $leaves += $inner
        }
        return , @($leaves)
    }
    if ($Expression.operator -in 'Or', 'Not') { return $null }
    , @($Expression)
}

function ConvertTo-IntuneMapping {
    # What an Intune app would be made from for one deployment type.
    param([Parameter(Mandatory)]$DeploymentType)
    $detection = $DeploymentType.detection
    $rules = @()
    $unmapped = @()
    switch ($detection.kind) {
        'none' { $unmapped += 'The deployment type has no detection method; write one.' }
        'script' {
            if ($detection.scriptType -eq 'PowerShell') {
                $rules += [ordered]@{ type = 'script'; scriptType = 'PowerShell'; script = $detection.script; note = 'SCCM and Intune agree: exit code 0 with any output means detected.' }
            } else {
                $unmapped += "$($detection.scriptType) detection can't be used in Intune, which runs PowerShell only. Rewrite it as a PowerShell script."
            }
        }
        'msi' {
            $rules += [ordered]@{ type = 'msi'; productCode = $detection.productCode; productVersionOperator = 'notConfigured'; productVersion = '' }
        }
        'enhanced' {
            $expression = $detection.enhanced.expression
            if (-not $expression) {
                $unmapped += 'The detection method has no rule.'
            } else {
                $leaves = Get-ExpressionLeaves -Expression $expression
                if ($null -eq $leaves) {
                    $unmapped += 'The detection rule uses OR or NOT. Intune detection rules must all match; rewrite it as a PowerShell script.'
                } else {
                    foreach ($leaf in $leaves) {
                        $mapped = ConvertTo-IntuneRule -Leaf $leaf -Settings $detection.enhanced.settings
                        if ($mapped.ContainsKey('rule')) { $rules += $mapped.rule } else { $unmapped += $mapped.unmapped }
                    }
                }
            }
        }
    }
    $notes = @()
    if ($DeploymentType.executionContext -eq 'User' -or $DeploymentType.requiresLogOn -in 'OnlyWhenUserLoggedOn', 'WhetherOrNotUserLoggedOn') {
        $notes += 'SCCM runs this as the user or only while one is signed in. Intune installs in user context only when a user is signed in; check that is acceptable.'
    }
    if ($DeploymentType.runAs32Bit -eq 'true') { $notes += 'The install runs as a 32-bit process in SCCM.' }
    if (@($DeploymentType.requirements).Count -gt 0) { $notes += "$(@($DeploymentType.requirements).Count) requirement rule(s) are listed but not migrated: Intune has a smaller set (architecture, minimum OS, disk, memory, CPU)." }
    if (@($DeploymentType.dependencies).Count -gt 0) { $notes += 'It has dependencies in SCCM. They are not migrated; add them to Intune by hand.' }
    $status = if ($rules.Count -gt 0 -and $unmapped.Count -eq 0) { 'mapped' } elseif ($rules.Count -gt 0) { 'partial' } else { 'unmapped' }
    $context = if ($DeploymentType.executionContext) { $DeploymentType.executionContext } else { 'System' }
    [ordered]@{
        logicalName = $DeploymentType.logicalName
        title = $DeploymentType.title
        technology = $DeploymentType.technology
        context = $context
        installCommand = $DeploymentType.installCommand
        uninstallCommand = $DeploymentType.uninstallCommand
        installExperience = if ($context -eq 'User') { 'user' } else { 'system' }
        maxInstallMinutes = if ($DeploymentType.maxExecuteMinutes) { [int]$DeploymentType.maxExecuteMinutes } else { $null }
        detection = if ($rules.Count -gt 0) { (($rules | ForEach-Object { $_.type }) -join ' + ') } else { 'none' }
        status = $status
        detectionRules = @($rules)
        unmapped = @($unmapped)
        notes = @($notes)
    }
}

function New-IntuneMappingDocument {
    param([Parameter(Mandatory)]$Application, [Parameter(Mandatory)]$Parsed)
    $mapped = @($Parsed.deploymentTypes | ForEach-Object { ConvertTo-IntuneMapping -DeploymentType $_ })
    $primary = $null
    if ($mapped.Count -gt 0) {
        $withPriority = @($Parsed.deploymentTypes | Where-Object { $_.priority -match '^\d+$' } | Sort-Object { [int]$_.priority })
        $primary = if ($withPriority.Count -gt 0) { $withPriority[0].logicalName } else { $mapped[0].logicalName }
    }
    $top = @()
    if ($mapped.Count -gt 1) { $top += "The application has $($mapped.Count) deployment types. An Intune app has one install and one uninstall command, so choose which to migrate (listed first: $primary)." }
    if (@($Parsed.supersedes).Count -gt 0) { $top += 'It supersedes other SCCM applications. Supersedence is not migrated.' }
    if ($mapped.Count -eq 0) { $top += 'The application has no deployment types.' }
    [ordered]@{
        schema = 1
        application = [ordered]@{ name = $Application.name; version = $Application.version; manufacturer = $Application.manufacturer; ciUniqueId = $Application.ciUniqueId }
        primaryDeploymentType = $primary
        deploymentTypes = $mapped
        notes = $top
    }
}

# ------------------------------------------------------------------ export

function Get-DisplayTitle {
    # SCCM names usually carry the version already ("Spectragryph 1.0.7"); don't say it twice.
    param([string]$Name, [string]$Version)
    if ($Version -and $Name -notlike "*$Version*") { "$Name $Version".Trim() } else { $Name.Trim() }
}

function Get-ExportFolderName {
    param([Parameter(Mandatory)]$Row)
    if ($Row -is [System.Collections.IDictionary]) { $Row = [pscustomobject]$Row }
    $get = { param($n) $p = $Row.PSObject.Properties[$n]; if ($p -and $null -ne $p.Value) { [string]$p.Value } else { '' } }
    if (-not (& $get 'name').Trim()) { throw 'The selected row has no name.' }
    $text = Get-DisplayTitle -Name (& $get 'name') -Version (& $get 'version')
    $slug = ([regex]::Replace($text.ToLowerInvariant(), '[^a-z0-9]+', '-')).Trim('-')
    if ($slug.Length -gt 60) { $slug = $slug.Substring(0, 60).TrimEnd('-') }
    if (-not $slug) { throw 'The selected row has no name.' }
    $slug
}

function New-ExportMarkdown {
    param([Parameter(Mandatory)]$Export, [Parameter(Mandatory)]$Mapping)
    $app = $Export.application
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add("# Migrate $(Get-DisplayTitle -Name $app.name -Version $app.version) from Configuration Manager to Intune")
    $lines.Add('')
    $lines.Add("This is an application from site **$($Export.site)** on ``$($Export.smsProvider)``. Manufacturer: $($app.manufacturer). Last changed in SCCM: $($app.dateLastModified).")
    if ($Export.description) { $lines.Add(''); $lines.Add($Export.description) }
    $lines.Add('')
    $lines.Add('## Where it is deployed in SCCM')
    $lines.Add('')
    if (@($Export.deployments).Count -eq 0) { $lines.Add('Not deployed.') }
    foreach ($d in $Export.deployments) { $lines.Add("- $($d.purpose): $($d.collection)$(if ($d.desiredConfig -eq 'Uninstall') { ' (**uninstall**)' })") }
    foreach ($note in $Mapping.notes) { $lines.Add(''); $lines.Add("**Note:** $note") }
    foreach ($dt in $Mapping.deploymentTypes) {
        $source = $Export.deploymentTypes | Where-Object { $_.logicalName -eq $dt.logicalName } | Select-Object -First 1
        $lines.Add('')
        $lines.Add("## Deployment type: $($dt.title) ($($dt.technology))")
        $lines.Add('')
        $lines.Add("- Install: ``$($dt.installCommand)``")
        $lines.Add("- Uninstall: ``$($dt.uninstallCommand)``")
        $lines.Add("- Runs as: $($dt.context)$(if ($dt.maxInstallMinutes) { "; up to $($dt.maxInstallMinutes) min" })")
        if ($source.contentLocation) {
            $lines.Add("- Content: ``$($source.contentLocation)`` ($(@($source.contentFiles).Count) listed file(s))")
        }
        $lines.Add("- Detection in Intune: **$($dt.status)** ($($dt.detection))")
        foreach ($rule in $dt.detectionRules) {
            $text = switch ($rule.type) {
                'registry' { "registry ``$($rule.keyPath)`` value ``$($rule.valueName)`` $($rule.detectionType) $($rule.operator) $($rule.value)" }
                'file' { "file ``$($rule.path)\$($rule.fileOrFolderName)`` $($rule.detectionType) $($rule.operator) $($rule.value)" }
                'msi' { "MSI product code ``$($rule.productCode)``" }
                'script' { 'a PowerShell script (see sccm-export.json)' }
                default { $rule.type }
            }
            $lines.Add("  - $text")
        }
        foreach ($u in $dt.unmapped) { $lines.Add("  - **Not mapped:** $u") }
        foreach ($n in $dt.notes) { $lines.Add("- **Note:** $n") }
    }
    $lines.Add('')
    $lines.Add('## Choosing a route')
    $lines.Add('')
    $lines.Add('Content on an internal share, or software the vendor does not offer for download, is usually migrated as a **one-off package**: the content is copied and its SHA-256 pinned. If the vendor offers a download of this version, a **Yardstick recipe** that fetches it is easier to keep up to date.')
    $lines -join "`n"
}

# ------------------------------------------------------------------ CIM

function Open-SccmSession {
    param([Parameter(Mandatory)][string]$Provider, [ValidateSet('Dcom', 'Wsman')][string]$Protocol = 'Dcom')
    if ($Provider -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') { throw "'$Provider' is not a server name." }
    New-CimSession -ComputerName $Provider -SessionOption (New-CimSessionOption -Protocol $Protocol) -ErrorAction Stop
}

function Get-SccmNamespace { param([Parameter(Mandatory)][string]$SiteCode)
    if (-not (Test-SccmSiteCode $SiteCode)) { throw "'$SiteCode' is not a three-character site code." }
    "root\SMS\site_$($SiteCode.ToUpperInvariant())"
}

function Test-SccmConnection {
    param([Parameter(Mandatory)]$Session, [Parameter(Mandatory)][string]$SiteCode, [string]$Provider)
    $namespace = Get-SccmNamespace -SiteCode $SiteCode
    $site = @(Get-CimInstance -CimSession $Session -Namespace $namespace -Query 'SELECT SiteCode, SiteName, Version, ServerName FROM SMS_Site' -ErrorAction Stop)
    $match = $site | Where-Object { $_.SiteCode -eq $SiteCode.ToUpperInvariant() } | Select-Object -First 1
    if (-not $match) { throw "The provider answered, but site $SiteCode is not one it knows." }
    [pscustomobject]@{ SiteCode = $match.SiteCode; SiteName = $match.SiteName; Version = $match.Version; Server = $match.ServerName; Namespace = $namespace }
}

function ConvertTo-IsoDate {
    param($Value)
    if ($null -eq $Value -or "$Value" -eq '') { return '' }
    try { ([datetime]$Value).ToUniversalTime().ToString('yyyy-MM-dd') } catch { [string]$Value }
}

function Find-SccmApplications {
    param([Parameter(Mandatory)]$Session, [Parameter(Mandatory)][string]$Namespace, [string]$Text = '', [int]$Limit = 300, [string]$WorkRoot)
    $where = 'IsLatest = 1'
    if ($Text.Trim()) {
        $like = ConvertTo-WqlLike -Text $Text.Trim()
        $where += " AND (LocalizedDisplayName LIKE '$like' OR Manufacturer LIKE '$like')"
    }
    $found = @(Get-CimInstance -CimSession $Session -Namespace $Namespace -Query "SELECT $script:ApplicationProperties FROM SMS_Application WHERE $where" -ErrorAction Stop |
        Sort-Object LocalizedDisplayName)
    $rows = @($found | Select-Object -First $Limit | ForEach-Object {
        $row = [ordered]@{
            key = [string]$_.CI_UniqueID; name = [string]$_.LocalizedDisplayName; version = [string]$_.SoftwareVersion; manufacturer = [string]$_.Manufacturer
            deployed = [bool]$_.IsDeployed; state = if ($_.IsExpired) { 'retired' } elseif ($_.IsSuperseded) { 'superseded' } else { 'active' }
            deploymentTypes = [int]$_.NumberOfDeploymentTypes; modified = ConvertTo-IsoDate $_.DateLastModified
        }
        # An application exported before shows its folder, so its actions are ready after a restart.
        if ($WorkRoot -and $row.name) {
            $slug = Get-ExportFolderName -Row $row
            if (Test-Path -LiteralPath (Join-Path $WorkRoot "$slug\sccm-export.json")) { $row['folder'] = $slug }
        }
        $row
    })
    [pscustomobject]@{ Rows = $rows; Total = $found.Count }
}

function Export-SccmApplication {
    param(
        [Parameter(Mandatory)]$Session, [Parameter(Mandatory)][string]$Namespace, [Parameter(Mandatory)][string]$SiteCode,
        [Parameter(Mandatory)][string]$Provider, [Parameter(Mandatory)][string]$WorkRoot, [Parameter(Mandatory)]$Row
    )
    $key = [string]$Row.key
    $instance = @(Get-CimInstance -CimSession $Session -Namespace $Namespace -Query "SELECT $script:ApplicationProperties FROM SMS_Application WHERE CI_UniqueID = '$(ConvertTo-WqlLiteral -Text $key)'" -ErrorAction Stop)
    if ($instance.Count -ne 1) { throw "Expected one application with that id but found $($instance.Count)." }
    # SDMPackageXML is a lazy property: it is only returned when the instance itself is fetched.
    $full = Get-CimInstance -InputObject $instance[0] -ErrorAction Stop
    $xml = [string]$full.SDMPackageXML
    if (-not $xml) { throw 'The application has no SDMPackageXML.' }
    $parsed = Read-SccmApplicationXml -Xml $xml

    $name = [string]$full.LocalizedDisplayName
    $deployments = @(Get-CimInstance -CimSession $Session -Namespace $Namespace -Query "SELECT CollectionName, OfferTypeID, DesiredConfigType, Enabled FROM SMS_ApplicationAssignment WHERE ApplicationName = '$(ConvertTo-WqlLiteral -Text $name)'" -ErrorAction SilentlyContinue |
        ForEach-Object {
            [ordered]@{
                collection = [string]$_.CollectionName
                purpose = switch ([int]$_.OfferTypeID) { 0 { 'Required' } 2 { 'Available' } default { "Offer type $($_.OfferTypeID)" } }
                desiredConfig = if ([int]$_.DesiredConfigType -eq 2) { 'Uninstall' } else { 'Install' }
                enabled = [bool]$_.Enabled
            }
        })
    $application = [ordered]@{
        name = $name; version = [string]$full.SoftwareVersion; manufacturer = [string]$full.Manufacturer
        ciId = [int]$full.CI_ID; ciUniqueId = $key; isDeployed = [bool]$full.IsDeployed; isExpired = [bool]$full.IsExpired
        isSuperseded = [bool]$full.IsSuperseded; dateLastModified = ConvertTo-IsoDate $full.DateLastModified
    }
    $export = [ordered]@{
        schema = 1; capturedUtc = [DateTime]::UtcNow.ToString('o'); site = $SiteCode.ToUpperInvariant(); smsProvider = $Provider
        application = $application; description = $parsed.description; publisher = $parsed.publisher
        deployments = $deployments; deploymentTypes = $parsed.deploymentTypes; supersedes = $parsed.supersedes; redactedCommands = $parsed.redactedCommands
    }
    $mapping = New-IntuneMappingDocument -Application ([pscustomobject]$application) -Parsed $parsed

    $folder = Join-Path $WorkRoot (Get-ExportFolderName -Row ([pscustomobject]@{ name = $name; version = [string]$full.SoftwareVersion }))
    New-Item -ItemType Directory -Path $folder -Force | Out-Null
    $write = { param($file, $value) ConvertTo-Json -InputObject $value -Depth 20 | Set-Content -LiteralPath (Join-Path $folder $file) -Encoding utf8NoBOM }
    & $write 'sccm-export.json' $export
    & $write 'intune-mapping.json' $mapping
    Set-Content -LiteralPath (Join-Path $folder 'export.md') -Value (New-ExportMarkdown -Export ([pscustomobject]($export | ConvertTo-Json -Depth 20 | ConvertFrom-Json -Depth 30)) -Mapping ($mapping | ConvertTo-Json -Depth 20 | ConvertFrom-Json -Depth 30)) -Encoding utf8NoBOM
    if ($parsed.iconBase64) {
        try { [IO.File]::WriteAllBytes((Join-Path $folder 'icon.png'), [Convert]::FromBase64String($parsed.iconBase64)) } catch { }
    }
    [pscustomobject]@{ Folder = $folder; Export = $export; Mapping = $mapping }
}

function Get-SccmContentLocation {
    # The content location SCCM holds now for a deployment type, read afresh.
    param([Parameter(Mandatory)]$Session, [Parameter(Mandatory)][string]$Namespace, [Parameter(Mandatory)][string]$CiUniqueId, [Parameter(Mandatory)][string]$LogicalName)
    $instance = @(Get-CimInstance -CimSession $Session -Namespace $Namespace -Query "SELECT CI_ID, CI_UniqueID FROM SMS_Application WHERE CI_UniqueID = '$(ConvertTo-WqlLiteral -Text $CiUniqueId)'" -ErrorAction Stop)
    if ($instance.Count -ne 1) { throw 'The application is no longer in SCCM under that id.' }
    $full = Get-CimInstance -InputObject $instance[0] -ErrorAction Stop
    $parsed = Read-SccmApplicationXml -Xml ([string]$full.SDMPackageXML)
    $dt = $parsed.deploymentTypes | Where-Object { $_.logicalName -eq $LogicalName } | Select-Object -First 1
    if (-not $dt) { throw 'The deployment type is no longer in SCCM.' }
    [string]$dt.contentLocation
}

# ------------------------------------------------------------------ content

function Copy-SccmContent {
    # Copies a deployment type's content into the export folder, verified file by file.
    # `CurrentLocation` is what SCCM says now; it must still be where the export found it.
    param(
        [Parameter(Mandatory)][string]$ExportFolder,
        [Parameter(Mandatory)][string]$CurrentLocation,
        [string]$LogicalName,
        [int]$MaxContentMB = 2048,
        [switch]$AllowNonUnc
    )
    $exportPath = Join-Path $ExportFolder 'sccm-export.json'
    if (-not (Test-Path -LiteralPath $exportPath)) { throw 'Export the application first.' }
    $export = [IO.File]::ReadAllText($exportPath) | ConvertFrom-Json -Depth 30
    $dts = @($export.deploymentTypes)
    $dt = if ($LogicalName) { $dts | Where-Object { $_.logicalName -eq $LogicalName } | Select-Object -First 1 } else { $dts | Select-Object -First 1 }
    if (-not $dt) { throw 'The deployment type is not in the export.' }
    $location = [string]$dt.contentLocation
    if (-not $location) { throw 'The deployment type has no content location (its commands may run from somewhere else).' }
    if ($CurrentLocation.TrimEnd('\') -ne $location.TrimEnd('\')) {
        throw "SCCM's content location is now '$CurrentLocation', not '$location' as exported. Export again."
    }
    if (-not $AllowNonUnc -and $location -notmatch '^\\\\[^\\]+\\[^\\]+') { throw "The content location '$location' is not a network share." }
    if ($location -match '(^|\\)\.\.(\\|$)') { throw 'The content location contains ..' }
    if (-not (Test-Path -LiteralPath $location -PathType Container)) { throw "Cannot reach the content at $location." }

    $destination = Join-Path $ExportFolder 'source'
    if (Test-Path -LiteralPath $destination) { throw "$destination already exists; refusing to overwrite it." }
    $sourceFiles = @(Get-ChildItem -LiteralPath $location -Recurse -File)
    $total = ($sourceFiles | Measure-Object Length -Sum).Sum
    if ($total -gt $MaxContentMB * 1MB) {
        throw "The content is $([math]::Round($total / 1MB)) MB, over the $MaxContentMB MB limit. Raise the limit in the plugin settings if that is intended."
    }
    New-Item -ItemType Directory -Path $destination | Out-Null
    $root = (Resolve-Path -LiteralPath $location).Path.TrimEnd('\')
    $records = foreach ($file in $sourceFiles) {
        $relative = $file.FullName.Substring($root.Length + 1)
        $target = Join-Path $destination $relative
        New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $target
        $sourceHash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
        $copyHash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
        if ($sourceHash -ne $copyHash) { throw "The copy of $relative does not match the source." }
        $signature = $null; $version = $null
        if ($file.Extension -in '.exe', '.dll', '.msi', '.msp', '.sys') {
            $signature = Get-AuthenticodeSignature -LiteralPath $target
            if ($file.Extension -in '.exe', '.dll') { $version = (Get-Item -LiteralPath $target).VersionInfo }
        }
        [ordered]@{
            path = $relative; bytes = $file.Length; sha256 = $copyHash
            signatureStatus = if ($signature) { [string]$signature.Status } else { $null }
            signer = if ($signature -and $signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
            fileVersion = if ($version) { $version.FileVersion } else { $null }
            productVersion = if ($version) { $version.ProductVersion } else { $null }
            productName = if ($version) { $version.ProductName } else { $null }
        }
    }
    $copied = @(Get-ChildItem -LiteralPath $destination -Recurse -File).Count
    if ($copied -ne $sourceFiles.Count) { throw "Copied $copied of $($sourceFiles.Count) files." }
    $manifest = [ordered]@{
        copiedUtc = [DateTime]::UtcNow.ToString('o'); from = $location; deploymentType = $dt.logicalName
        totalBytes = [long]$total; fileCount = $sourceFiles.Count; files = @($records)
    }
    ConvertTo-Json -InputObject $manifest -Depth 8 | Set-Content -LiteralPath (Join-Path $ExportFolder 'source-manifest.json') -Encoding utf8NoBOM
    [pscustomobject]$manifest
}

Export-ModuleMember -Function ConvertTo-WqlLiteral, ConvertTo-WqlLike, Test-SccmSiteCode, Read-SccmApplicationXml, ConvertTo-IntuneMapping,
    New-IntuneMappingDocument, Get-ExportFolderName, New-ExportMarkdown, Open-SccmSession, Get-SccmNamespace, Test-SccmConnection,
    Find-SccmApplications, Export-SccmApplication, Get-SccmContentLocation, Copy-SccmContent, Format-CommandLine, Get-DisplayTitle
