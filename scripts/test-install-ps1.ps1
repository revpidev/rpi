#!/usr/bin/env pwsh
# test-install-ps1.ps1 - self-contained tests for install.ps1's PATH helpers.
#
# Parses install.ps1 (syntax gate), loads only its function definitions, and
# exercises the pure/string/registry-mocked PATH logic. No registry writes and
# no real user PATH changes: Get-UserEnv/Set-UserEnv/Publish-EnvChange and
# Read-Host are mocked. Runs on Windows PowerShell 5.1 and PowerShell 7+.

$ErrorActionPreference = 'Stop'

$script:Tests = 0
$script:Failed = 0

function Check-True {
    param([bool]$Condition, [string]$Name)
    $script:Tests++
    if ($Condition) {
        Write-Host "ok $script:Tests - $Name"
    } else {
        Write-Host "not ok $script:Tests - $Name"
        $script:Failed++
    }
}

function Check-Equal {
    param([string]$Expected, [string]$Actual, [string]$Name)
    $script:Tests++
    if ([string]::Equals($Expected, $Actual, [System.StringComparison]::Ordinal)) {
        Write-Host "ok $script:Tests - $Name"
    } else {
        Write-Host "not ok $script:Tests - $Name"
        Write-Host "    expected: [$Expected]"
        Write-Host "    actual:   [$Actual]"
        $script:Failed++
    }
}

$scriptDir = $PSScriptRoot
if (-not $scriptDir) {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
}
$installer = Join-Path (Split-Path -Parent $scriptDir) 'install.ps1'
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    Write-Host "install.ps1 not found at $installer"
    exit 1
}

# ---- syntax gate + load only the function definitions ------------------------

$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path -LiteralPath $installer).Path, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) {
    # Print before the assertion so the diagnostics survive a failed check.
    foreach ($parseError in $parseErrors) {
        Write-Host "    parse error at line $($parseError.Extent.StartLineNumber): $($parseError.Message)"
    }
}
Check-Equal '0' ([string]$parseErrors.Count) 'install.ps1 parses without errors'

# Windows PowerShell 5.1 decodes a BOM-less script as ANSI. A non-ASCII byte
# sequence can then contain a smart quote (U+201C/U+201D), which the parser
# treats as a string delimiter and which breaks an otherwise valid string.
# Keep the installer ASCII-only.
$installerBytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $installer).Path)
$nonAsciiBytes = @($installerBytes | Where-Object { $_ -gt 0x7F })
Check-Equal '0' ([string]$nonAsciiBytes.Count) 'install.ps1 is ASCII-only (PS 5.1 ANSI decoding)'

$functions = $ast.FindAll(
    { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] },
    $false)
foreach ($function in $functions) {
    . ([ScriptBlock]::Create($function.Extent.Text))
}

# ---- mocks -------------------------------------------------------------------

$script:UserPathVariable = $null
$script:WrittenPath = $null
function Get-UserEnv { param([string]$Key) return $script:UserPathVariable }
function Set-UserEnv { param([string]$Key, [string]$Value) $script:WrittenPath = $Value }
function Publish-EnvChange { }

$savedUserProfile = $env:USERPROFILE
$savedLocalAppData = $env:LOCALAPPDATA
$savedAppData = $env:APPDATA
$savedCi = $env:CI
$savedNonInteractive = $env:RPI_NONINTERACTIVE
$savedTerm = $env:TERM

$env:USERPROFILE = 'C:\Users\me'
$env:LOCALAPPDATA = 'C:\Users\me\AppData\Local'
$env:APPDATA = 'C:\Users\me\AppData\Roaming'

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('rpi-install-ps1-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempRoot | Out-Null

try {
    # ---- pure string helpers -------------------------------------------------

    Check-Equal 'C:\' (Remove-TrailingSeparators 'C:\') 'drive root stays a root'
    Check-Equal 'C:\Users\me' (Remove-TrailingSeparators 'C:\Users\me\') 'trailing backslash removed'
    Check-Equal '\\server\share' (Remove-TrailingSeparators '\\server\share\') 'UNC trailing slash removed'
    Check-Equal 'C:\Program Files\rpi' (ConvertTo-NormalizedPathEntry '"C:\Program Files\rpi"') 'quoted PATH entry normalized'
    Check-Equal 'C:\Tools\rpi' (ConvertTo-NormalizedPathEntry 'C:\Tools\rpi\') 'trailing separator normalized'

    if ($env:OS -eq 'Windows_NT') {
        $savedLocal = $env:LOCALAPPDATA
        $env:LOCALAPPDATA = 'C:\Users\me\AppData\Local'
        Check-Equal 'C:\Users\me\AppData\Local\Programs\rpi' `
            (ConvertTo-NormalizedPathEntry '%LOCALAPPDATA%\Programs\rpi') '%VAR% PATH entry expands for comparison'
        $env:LOCALAPPDATA = $savedLocal
    }

    # ---- user-scope detection ------------------------------------------------

    Check-True (Test-UserScopedPrefix 'C:\Users\me\AppData\Local\Programs\rpi') 'prefix inside LOCALAPPDATA is user-scoped'
    Check-True (Test-UserScopedPrefix 'C:\Users\me\bin') 'prefix inside USERPROFILE is user-scoped'
    Check-True (Test-UserScopedPrefix 'C:\Users\me') 'exact USERPROFILE is user-scoped'
    Check-True (Test-UserScopedPrefix 'C:\Users\me\bin\') 'trailing slash is user-scoped'
    Check-True (-not (Test-UserScopedPrefix 'C:\Users\me2\bin')) 'prefix collision is not user-scoped'
    Check-True (-not (Test-UserScopedPrefix 'C:\Program Files\rpi')) 'program files prefix is not user-scoped'

    # ---- PATH entry filtering ------------------------------------------------

    $kept = @(Get-PathEntriesWithoutDirectory 'C:\Windows;;%USERPROFILE%\bin;C:\Tools\rpi;' 'C:\Tools\rpi')
    Check-Equal '2' ([string]$kept.Count) 'empty and duplicate entries are removed'
    Check-Equal 'C:\Windows' $kept[0] 'first retained entry is unchanged'
    Check-Equal '%USERPROFILE%\bin' $kept[1] '%VAR% spelling is preserved in the retained entry'

    # ---- Add-UserPathEntry ---------------------------------------------------

    $script:UserPathVariable = 'C:\Windows;C:\Users\me\AppData\Local\Programs\rpi;%USERPROFILE%\bin;C:\Tools\rpi'
    $env:PATH = 'C:\Windows;C:\Users\me\AppData\Local\Programs\rpi'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\Users\me\AppData\Local\Programs\rpi\' -Prepend
    Check-Equal 'C:\Users\me\AppData\Local\Programs\rpi;C:\Windows;%USERPROFILE%\bin;C:\Tools\rpi' `
        $script:WrittenPath 'prepend removes literal duplicates and keeps %VAR% entries'

    $script:UserPathVariable = 'C:\Windows;%LOCALAPPDATA%\Programs\rpi'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\Users\me\AppData\Local\Programs\rpi' -Prepend
    Check-Equal 'C:\Users\me\AppData\Local\Programs\rpi;C:\Windows' `
        $script:WrittenPath 'expanded %VAR% duplicate is removed'
    Check-Equal 'C:\Users\me\AppData\Local\Programs\rpi;C:\Windows' $env:PATH 'process PATH is updated and deduplicated'

    $script:UserPathVariable = 'C:\Windows;"C:\Program Files\rpi"'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\Program Files\rpi' -Prepend
    Check-Equal 'C:\Program Files\rpi;C:\Windows' $script:WrittenPath 'quoted duplicate is removed'

    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\Tools\rpi'
    Check-Equal 'C:\Windows;C:\Tools\rpi' $script:WrittenPath 'append keeps the existing order'

    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\' -Prepend
    Check-Equal 'C:\;C:\Windows' $script:WrittenPath 'drive root is not reduced to "C:"'

    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Add-UserPathEntry 'C:\Tools\100%rpi' -Prepend
    Check-Equal 'C:\Tools\100%rpi;C:\Windows' $script:WrittenPath 'literal percent in the install dir is preserved'

    # ---- combined-PATH search / machine shadow warning -----------------------

    $pathA = Join-Path $tempRoot 'a'
    $pathB = Join-Path $tempRoot 'b'
    New-Item -ItemType Directory -Path $pathA | Out-Null
    New-Item -ItemType Directory -Path $pathB | Out-Null
    Set-Content -LiteralPath (Join-Path $pathA 'rpi.exe') -Value 'fake'
    Set-Content -LiteralPath (Join-Path $pathB 'rpi.exe') -Value 'fake'

    Check-True ([string]::Equals(
        (Join-Path $pathB 'rpi.exe'),
        (Find-FirstRpiOnCombinedPath ($pathB + ';' + $pathA)),
        [System.StringComparison]::OrdinalIgnoreCase)) 'combined PATH returns the first existing rpi.exe'
    Check-True (-not (Find-FirstRpiOnCombinedPath (Join-Path $tempRoot 'missing'))) 'missing combined PATH returns null'

    $warning = Get-RpiPathShadowWarning $pathB $pathA $pathB
    Check-True ([bool]$warning) 'machine-level rpi ahead of the install dir produces a warning'
    $warning = Get-RpiPathShadowWarning $pathA $pathA $pathB
    Check-True (-not [bool]$warning) 'no warning when the machine entry is the install dir'

    # ---- manual command / refresh command -----------------------------------

    $manual = @(Get-ManualPathCommand "C:\O'Brien\rpi")
    Check-Equal '3' ([string]$manual.Count) 'manual note has three lines'
    Check-True (($manual -join "`n").Contains('DoNotExpandEnvironmentNames')) 'manual note preserves %VAR% spellings'
    Check-True (($manual -join "`n").Contains("C:\O''Brien\rpi")) 'manual note escapes single quotes'
    Check-Equal '$env:PATH = "C:\Tools\`$rpi;$env:PATH"' `
        (Get-PathRefreshCommand 'C:\Tools\$rpi' 'powershell') 'PowerShell refresh note escapes $'

    # ---- active-command detection -------------------------------------------

    $realGetCommand = Get-Command
    function Get-Command {
        param($Name, $CommandType, $ErrorAction)
        if ($script:ActiveSource) {
            return [pscustomobject]@{ Source = $script:ActiveSource }
        }
        return $null
    }

    $script:ActiveSource = 'C:\Users\me\bin\rpi.exe'
    Check-True (Test-RpiInstalledIsFirstOnPath 'C:\Users\ME\bin\RPI.EXE') 'active rpi comparison is case-insensitive'
    $script:ActiveSource = 'C:\Other\rpi.exe'
    Check-True (-not (Test-RpiInstalledIsFirstOnPath 'C:\Users\me\bin\rpi.exe')) 'different active rpi is rejected'

    # ---- Update-RpiPath branches --------------------------------------------

    $script:ActiveSource = $null
    $script:UserPathVariable = 'C:\Windows'
    $PathAction = 'skip'
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'skip never writes PATH'

    $PathAction = 'add'
    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal 'C:\Users\me\bin;C:\Windows' $script:WrittenPath 'add prepends the install dir'

    $PathAction = 'prompt'
    function Test-InstallerInteractiveInput { return $true }
    function Read-Host { param($Prompt) return 'n' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'prompt "n" never writes PATH'

    function Read-Host { param($Prompt) return 'No' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'prompt "No" never writes PATH'

    function Read-Host { param($Prompt) return 'y' }
    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal 'C:\Users\me\bin;C:\Windows' $script:WrittenPath 'prompt "y" writes PATH'

    $PathAction = 'prompt'
    function Test-InstallerInteractiveInput { return $false }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'non-interactive run never prompts or writes PATH'

    $PathAction = 'add'
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Program Files\rpi\rpi.exe' 'C:\Program Files\rpi' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'outside-profile prefix is never written automatically'

    # already active: no write
    $script:ActiveSource = 'C:\Users\me\bin\rpi.exe'
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'already-active rpi is left alone'

    Remove-Item Function:\Get-Command -ErrorAction SilentlyContinue

    # ---- non-interactive environment ---------------------------------------

    $env:CI = ''
    $env:RPI_NONINTERACTIVE = ''
    $env:TERM = 'xterm'
    Check-True (-not (Test-NonInteractiveEnvironment)) 'no CI/RPI_NONINTERACTIVE/TERM=dumb is interactive'
    $env:CI = 'true'
    Check-True (Test-NonInteractiveEnvironment) 'CI disables prompting'
    $env:CI = ''
    $env:RPI_NONINTERACTIVE = 'YES'
    Check-True (Test-NonInteractiveEnvironment) 'RPI_NONINTERACTIVE disables prompting'
    $env:RPI_NONINTERACTIVE = ''
    $env:TERM = 'dumb'
    Check-True (Test-NonInteractiveEnvironment) 'TERM=dumb disables prompting'
} finally {
    $env:USERPROFILE = $savedUserProfile
    $env:LOCALAPPDATA = $savedLocalAppData
    $env:APPDATA = $savedAppData
    $env:CI = $savedCi
    $env:RPI_NONINTERACTIVE = $savedNonInteractive
    $env:TERM = $savedTerm
    Remove-Item -Recurse -Force $tempRoot -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "ran $script:Tests checks, $script:Failed failed"
if ($script:Failed -ne 0) {
    exit 1
}