#Requires -Version 7.0
Set-StrictMode -Version Latest

$script:Plugin = Split-Path -Parent $PSScriptRoot
function Get-PluginRoot { $script:Plugin }
function Get-RealDetectionXml { [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'fixtures\real-registry-detection.xml')) }

function Esc { param([string]$Text) [Security.SecurityElement]::Escape($Text) }

function New-EnhancedDetectionXml {
    # An EnhancedDetectionMethod document in the shape the site stores: settings, then a rule.
    param([string]$Settings, [string]$Expression)
    @"
<?xml version="1.0" encoding="utf-16"?>
<EnhancedDetectionMethod xmlns="http://schemas.microsoft.com/SystemCenterConfigurationManager/2009/AppMgmtDigest">
  <Settings xmlns="http://schemas.microsoft.com/SystemCenterConfigurationManager/2009/AppMgmtDigest">$Settings</Settings>
  <Rule id="ScopeId_X/DeploymentType_Y" Severity="Informational" NonCompliantWhenSettingIsNotFound="false" xmlns="http://schemas.microsoft.com/SystemsCenterConfigurationManager/2009/06/14/Rules">
    <Expression>$Expression</Expression>
  </Rule>
</EnhancedDetectionMethod>
"@
}

function New-RegistrySetting {
    param([string]$Name = 'RegSetting_1', [string]$Key = 'SOFTWARE\Vendor\App', [string]$Value = 'Version', [string]$Is64 = 'true', [string]$Type = 'String')
    @"
<SimpleSetting LogicalName="$Name" DataType="$Type" xmlns="http://schemas.microsoft.com/SystemsCenterConfigurationManager/2009/07/10/DesiredConfiguration">
  <RegistryDiscoverySource Hive="HKEY_LOCAL_MACHINE" Depth="Base" Is64Bit="$Is64" CreateMissingPath="true"><Key>$Key</Key><ValueName>$Value</ValueName></RegistryDiscoverySource>
</SimpleSetting>
"@
}

function New-FileSetting {
    param([string]$Name = 'File_1', [string]$Path = 'C:\Program Files\Vendor', [string]$Filter = 'app.exe')
    "<SimpleSetting LogicalName=""$Name"" DataType=""Int64"" xmlns=""http://schemas.microsoft.com/SystemsCenterConfigurationManager/2009/07/10/DesiredConfiguration""><File Path=""$Path"" Filter=""$Filter"" Is64Bit=""true"" Recursive=""false"" /></SimpleSetting>"
}

function New-Comparison {
    param([string]$Operator, [string]$Setting, [string]$SourceType, [string]$Method = 'Value', [string]$DataType = 'String', [string]$Constant, [string]$Property = '')
    $propertyPath = if ($Property) { " PropertyPath=""$Property""" } else { '' }
    "<Operator>$Operator</Operator><Operands><SettingReference SettingLogicalName=""$Setting"" SettingSourceType=""$SourceType"" Method=""$Method"" DataType=""$DataType""$propertyPath /><ConstantValue Value=""$Constant"" DataType=""$DataType"" /></Operands>"
}

function New-DeploymentTypeXml {
    param(
        [string]$LogicalName = 'DeploymentType_1', [string]$Title = 'Silent install', [string]$Technology = 'Script', [string]$Priority = '',
        [string]$Context = 'System', [string]$InstallCommand = '"setup.exe" /S', [string]$UninstallCommand = '"C:\Program Files\App\unins000.exe" /SILENT',
        [string]$Location = '\\server\share\Apps\App\', [string[]]$Files = @('setup.exe'), [string]$DetectionBody = '', [string]$ScriptType = '',
        [string]$CustomData = '', [string]$Extra = '', [string]$RequiresLogOn = ''
    )
    $fileXml = ($Files | ForEach-Object { "<File Name=""$_"" Size=""1024"" />" }) -join ''
    $scriptArg = if ($ScriptType) { "<Arg Name=""ScriptType"" Type=""String"">$ScriptType</Arg>" } else { '' }
    $detect = if ($DetectionBody) { "<DetectAction><Provider>Local</Provider><Args><Arg Name=""MethodBody"" Type=""String"">$(Esc $DetectionBody)</Arg>$scriptArg</Args></DetectAction>" } else { '' }
    $priorityAttr = if ($Priority) { " Priority=""$Priority""" } else { '' }
    @"
<DeploymentType AuthoringScopeId="ScopeId_X" LogicalName="$LogicalName" Version="3"$priorityAttr>
  <Title ResourceId="Res_1">$Title</Title>
  <Description ResourceId="Res_2" />
  <Technology>$Technology</Technology>
  <Installer Technology="$Technology">
    <ExecutionContext>$Context</ExecutionContext>
    <Contents><Content ContentId="Content_$LogicalName" Version="1">$fileXml<Location>$Location</Location><PeerCache>true</PeerCache></Content></Contents>
    $detect
    <InstallAction><Provider>Script</Provider><Args>
      <Arg Name="InstallCommandLine" Type="String">$(Esc $InstallCommand)</Arg>
      <Arg Name="ExecutionContext" Type="String">$Context</Arg>
      <Arg Name="RequiresLogOn" Type="String">$RequiresLogOn</Arg>
      <Arg Name="UserInteractionMode" Type="String">Hidden</Arg>
      <Arg Name="ExecuteTime" Type="Int32">5</Arg>
      <Arg Name="MaxExecuteTime" Type="Int32">15</Arg>
      <Arg Name="RunAs32Bit" Type="Boolean">false</Arg>
    </Args></InstallAction>
    <UninstallAction><Provider>Script</Provider><Args><Arg Name="InstallCommandLine" Type="String">$(Esc $UninstallCommand)</Arg></Args></UninstallAction>
    <CustomData>$CustomData</CustomData>
  </Installer>
  <Requirements><Rule id="Rule_1"><Annotation><DisplayName Text="Operating system One of {Windows 10}" /></Annotation><Expression><Operator>OneOf</Operator><Operands><SettingReference SettingLogicalName="OS" SettingSourceType="Asset" Method="Value" DataType="String" /></Operands></Expression></Rule></Requirements>
  $Extra
</DeploymentType>
"@
}

function New-DigestXml {
    param([string[]]$DeploymentTypes, [string]$Title = 'Test App 1.0', [string]$Icon = '', [string]$Supersedes = '')
    $iconXml = if ($Icon) { "<Resources><Icon Id=""Icon_1""><Data>$Icon</Data></Icon></Resources>" } else { '' }
    @"
<?xml version="1.0" encoding="utf-16"?>
<AppMgmtDigest xmlns="http://schemas.microsoft.com/SystemCenterConfigurationManager/2009/AppMgmtDigest" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Application AuthoringScopeId="ScopeId_X" LogicalName="Application_1" Version="8">
    <DisplayInfo DefaultLanguage="en-US"><Info Language="en-US"><Title>$Title</Title><Publisher>Test Publisher</Publisher><Version>1.0</Version><Description>A test application.</Description></Info></DisplayInfo>
    <DeploymentTypes>$($DeploymentTypes -join '')</DeploymentTypes>
    $Supersedes
  </Application>
  $iconXml
</AppMgmtDigest>
"@
}

function New-TestRoot {
    $root = Join-Path ([IO.Path]::GetTempPath()) ("sccm-test-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $root | Out-Null
    $root
}
