#!/usr/bin/env pwsh
# test-install-ps1.ps1 - self-contained tests for install.ps1's PATH helpers.
#
# Parses install.ps1 (syntax gate), loads only its function definitions, and
# exercises the pure/string/registry-mocked PATH logic. No registry writes and
# no real user PATH changes: Get-UserEnv/Set-UserEnv/Publish-EnvChange and
# Read-Host are mocked. Runs on Windows PowerShell 5.1 and PowerShell 7+.

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 resolves `ZipArchiveMode`/`ZipArchive` only from the base
# assembly; loading `System.IO.Compression.FileSystem` alone leaves the type
# unresolved ("Unable to find type"), which failed the rc.4 Windows 5.1 job.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

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
    $savedProcessPath = $env:PATH
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
    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return 'n' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'prompt "n" never writes PATH'

    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return 'No' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal '<untouched>' $script:WrittenPath 'prompt "No" never writes PATH'

    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return 'y' }
    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null
    Check-Equal 'C:\Users\me\bin;C:\Windows' $script:WrittenPath 'prompt "y" writes PATH'

    # Timeout and unreadable answers must never be treated as the default Yes.
    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return '__timeout__' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null 3>$null
    Check-Equal '<untouched>' $script:WrittenPath 'prompt timeout never writes PATH'

    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return '__unavailable__' }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null 3>$null
    Check-Equal '<untouched>' $script:WrittenPath 'unreadable prompt answer never writes PATH'

    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return $null }
    $script:WrittenPath = '<untouched>'
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null 3>$null
    Check-Equal '<untouched>' $script:WrittenPath 'null prompt answer never writes PATH'

    function Read-HostWithTimeout { param($Prompt, $TimeoutSeconds) return '' }
    $script:UserPathVariable = 'C:\Windows'
    $script:WrittenPath = $null
    Update-RpiPath 'C:\Users\me\bin\rpi.exe' 'C:\Users\me\bin' 6>$null 3>$null
    Check-Equal 'C:\Users\me\bin;C:\Windows' $script:WrittenPath 'empty (Enter) answer keeps the yes default'

    # A redirected console must yield the explicit no-answer sentinel instead of
    # blocking in Read-Host. Only the read path is tested (the probe is mocked).
    if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) {
        # Earlier PATH cases mock Read-HostWithTimeout itself; reload the real
        # implementation before probing it.
        $realReader = $functions | Where-Object { $_.Name -eq 'Read-HostWithTimeout' } | Select-Object -First 1
        . ([ScriptBlock]::Create($realReader.Extent.Text))
        function Test-InstallerInteractiveInput { return $true }
        $sentinel = Read-HostWithTimeout -Prompt 'probe ' -TimeoutSeconds 1
        Check-True ($sentinel -eq '__unavailable__' -or $sentinel -eq '__timeout__') `
            'redirected console returns a no-answer sentinel instead of blocking'
    } else {
        Write-Host 'ok - redirected console sentinel # SKIP (interactive console)'
    }

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
    $env:PATH = $savedProcessPath

    # ---- install profiles: lists, consent, semver, index --------------------

    $standard = @(Get-StandardPluginList)
    $full = @(Get-ProfilePluginList -ProfileName 'full')
    Check-Equal '6' ([string]$standard.Count) 'standard profile has six plugins'
    Check-Equal '7' ([string]$full.Count) 'full profile has seven plugins'
    Check-Equal 'rpi-usage' $full[$full.Count - 1] 'full appends rpi-usage'
    Check-Equal '0' ([string](@(Get-ProfilePluginList -ProfileName 'mini')).Count) 'mini has no plugins'
    Check-True ((@(Get-ProfilePluginList -ProfileName 'standard') -join ' ') -eq ($standard -join ' ')) 'standard list is stable'
    Check-True ((@(Get-ProfilePluginList -ProfileName 'bogus')).Count -eq 0) 'unknown profile has no plugins'

    Check-True (Test-TruthyValue '1') 'truthy 1'
    Check-True (Test-TruthyValue 'YES') 'truthy YES'
    Check-True (-not (Test-TruthyValue '0')) 'falsy 0'
    Check-True (-not (Test-TruthyValue $null)) 'falsy null'

    Check-True (Get-L0ConsentDecision -ProfileName 'mini' -InteractiveChoice $false -Yes $false) 'mini needs no consent'
    Check-True (Get-L0ConsentDecision -ProfileName 'standard' -InteractiveChoice $true -Yes $false) 'interactive choice consents'
    Check-True (Get-L0ConsentDecision -ProfileName 'full' -InteractiveChoice $false -Yes $true) '-Yes consents non-interactively'
    Check-True (-not (Get-L0ConsentDecision -ProfileName 'standard' -InteractiveChoice $false -Yes $false)) 'flag/env alone does not consent'

    Check-Equal 'eq' (Compare-SemverVersion -Left '0.1.0' -Right '0.1.0') 'semver equal'
    Check-Equal 'lt' (Compare-SemverVersion -Left '0.1.0-rc.1' -Right '0.1.0') 'prerelease is below release'
    Check-Equal 'gt' (Compare-SemverVersion -Left '0.1.0' -Right '0.1.0-rc.1') 'release is above prerelease'
    Check-Equal 'lt' (Compare-SemverVersion -Left '0.1.0-rc.2' -Right '0.1.0-rc.10') 'numeric prerelease compares numerically'
    Check-Equal 'lt' (Compare-SemverVersion -Left '0.1.0-alpha' -Right '0.1.0-alpha.1') 'shorter prerelease prefix is lower'
    Check-Equal 'gt' (Compare-SemverVersion -Left '0.1.0-alpha.1' -Right '0.1.0-alpha') 'longer prerelease prefix is higher'
    Check-Equal 'eq' (Compare-SemverVersion -Left '0.1.0+build.1' -Right '0.1.0') 'build metadata ignored'
    Check-Equal 'gt' (Compare-SemverVersion -Left '1.0.0' -Right '0.9.9') 'major wins'
    Check-Equal 'invalid' (Compare-SemverVersion -Left 'bad' -Right '0.1.0') 'invalid left'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '1.0' -Right '0.1.0') 'short version invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '01.0.0' -Right '0.1.0') 'leading zero invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-01' -Right '0.1.0') 'numeric prerelease leading zero invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0' -Right '0.1.0-01') 'invalid right is not treated as higher'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-01' -Right '0.1.0-rc.1') 'invalid left against prerelease'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-rc.1' -Right '0.1.0-01') 'invalid right against prerelease'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-rc.01' -Right '0.1.0-rc.1') 'numeric prerelease leading zero in the middle'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-rc.1' -Right '0.1.0-rc.01') 'numeric prerelease leading zero on the right'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-' -Right '0.1.0') 'empty prerelease invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0+' -Right '0.1.0') 'empty build metadata invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-a..b' -Right '0.1.0-a') 'empty prerelease identifier invalid'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0-a_b' -Right '0.1.0-a') 'invalid prerelease character'
    Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0+build_1' -Right '0.1.0') 'invalid build metadata character'
    Check-Equal 'eq' (Compare-SemverVersion -Left '0.1.0+build.01' -Right '0.1.0') 'numeric build metadata may keep leading zeros'
    Check-Equal 'gt' (Compare-SemverVersion -Left '99999999999999999999.0.0' -Right '1.0.0') 'semver core does not overflow int64'
    Check-Equal 'lt' (Compare-SemverVersion -Left '1.0.0' -Right '99999999999999999999.0.0') 'semver core overflow on the right'
    Check-Equal 'lt' (Compare-SemverVersion -Left '0.1.0-rc.99999999999999999999' -Right '0.1.0-rc.100000000000000000000') 'prerelease numeric does not overflow int64'

    # .NET $ matches before a trailing LF; every surrounding control character
    # must instead make the value invalid (SemVer is a whole-string grammar).
    foreach ($suffix in @("`n", "`r", "`t", ' ')) {
        $label = [int][char]$suffix
        Check-Equal 'invalid' (Compare-SemverVersion -Left ('0.1.0' + $suffix) -Right '0.1.0') "left semver with trailing byte $label is invalid"
        Check-Equal 'invalid' (Compare-SemverVersion -Left '0.1.0' -Right ('0.1.0' + $suffix)) "right semver with trailing byte $label is invalid"
    }
    Check-Equal 'eq' (Compare-SemverVersion -Left '0.1.0' -Right '0.1.0') 'exact semver still compares equal'
    Check-Equal 'eq' (Compare-SemverVersion -Left '0.1.0+meta' -Right '0.1.0+meta') 'exact semver with build metadata compares equal'

    $asset = 'rpi-0.1.0-x86_64-pc-windows-msvc-standard.zip'
    $sha = ('a' * 64)
    $goodIndex = @"
{
  "schemaVersion": 1,
  "version": "0.1.0",
  "assets": {
    "$asset": "$sha"
  }
}
"@
    $index = ConvertFrom-ProfileIndex -Text $goodIndex -Version '0.1.0' -AssetName $asset
    Check-True $index.Ok 'valid profile index accepted'
    Check-Equal $sha $index.Sha 'valid index returns the sha'

    $index = ConvertFrom-ProfileIndex -Text $goodIndex -Version '9.9.9' -AssetName $asset
    Check-True (-not $index.Ok) 'index version mismatch rejected'
    Check-True ($index.Error -like '*version mismatch*') 'version mismatch message'

    $index = ConvertFrom-ProfileIndex -Text $goodIndex -Version '0.1.0' -AssetName 'other.zip'
    Check-True (-not $index.Ok) 'missing asset entry rejected'

    $duplicate = $goodIndex -replace ('"' + $asset + '":'), ('"' + $asset + '": "' + $sha + '", "' + $asset + '":')
    $index = ConvertFrom-ProfileIndex -Text $duplicate -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'duplicate asset key rejected'

    $badHash = $goodIndex.Replace($sha, 'not-a-hash')
    $index = ConvertFrom-ProfileIndex -Text $badHash -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'malformed index hash rejected'

    $badSchema = $goodIndex.Replace('"schemaVersion": 1', '"schemaVersion": 2')
    $index = ConvertFrom-ProfileIndex -Text $badSchema -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'unsupported schemaVersion rejected'

    $index = ConvertFrom-ProfileIndex -Text 'not json' -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'invalid JSON rejected'

    $index = ConvertFrom-ProfileIndex -Text $goodIndex.Replace('"schemaVersion": 1', '"schemaVersion": "1"') -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'schemaVersion string is rejected'
    # JSON escapes are case-sensitive: \u is the only legal lowercase form.
    foreach ($escape in @('\U0061', '\N', '\B', '\F', '\R', '\T')) {
        $escaped = $goodIndex.Replace('"assets"', '"' + $escape + 'ssets"')
        $index = ConvertFrom-ProfileIndex -Text $escaped -Version '0.1.0' -AssetName $asset
        Check-True (-not $index.Ok) "uppercase JSON escape $escape is rejected"
        Check-True ($index.Error -like '*invalid escape*') "uppercase JSON escape $escape reports an invalid escape"
    }
    $legal = $goodIndex.Replace('"assets"', '"\u0061ssets"')
    $index = ConvertFrom-ProfileIndex -Text $legal -Version '0.1.0' -AssetName $asset
    Check-True $index.Ok 'legal \u escape is still accepted'
    $dupSchema = $goodIndex.Replace('"schemaVersion": 1,', '"schemaVersion": 1, "schemaVersion": 1,')
    $index = ConvertFrom-ProfileIndex -Text $dupSchema -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'duplicate schemaVersion key rejected'
    $dupAssets = $goodIndex.Replace('"assets": {', '"assets": {}, "assets": {')
    $index = ConvertFrom-ProfileIndex -Text $dupAssets -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'duplicate assets key rejected'
    $outside = @"
{ "schemaVersion": 1, "version": "0.1.0", "assets": {}, "extra": { "$asset": "$sha" } }
"@
    $index = ConvertFrom-ProfileIndex -Text $outside -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'asset outside the flat assets object rejected'
    $nested = @"
{ "schemaVersion": 1, "version": "0.1.0", "assets": { "nested": { "$asset": "$sha" } } }
"@
    $index = ConvertFrom-ProfileIndex -Text $nested -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'nested asset value rejected'
    $arrayAssets = $goodIndex.Replace('"assets": {', '"assets": [').Replace('"assets": [', '"assets": [')
    $index = ConvertFrom-ProfileIndex -Text ($goodIndex -replace '"assets": \{[^}]*\}', '"assets": []') -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'array assets rejected'
    $index = ConvertFrom-ProfileIndex -Text ($goodIndex + ' trailing') -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'trailing data rejected'
    $withSizes = @"
{ "schemaVersion": 1, "version": "0.1.0", "assets": { "$asset": "$sha" }, "sizes": { "$asset": 123 } }
"@
    $index = ConvertFrom-ProfileIndex -Text $withSizes -Version '0.1.0' -AssetName $asset
    Check-True $index.Ok 'flat sizes map accepted'
    $badSize = $withSizes.Replace(('"' + $asset + '": 123'), ('"' + $asset + '": "123"'))
    $index = ConvertFrom-ProfileIndex -Text $badSize -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'string size rejected'
    $badSize = $withSizes.Replace(('"' + $asset + '": 123'), ('"' + $asset + '": 1.5'))
    $index = ConvertFrom-ProfileIndex -Text $badSize -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'floating-point size rejected'
    $badSize = $withSizes.Replace(('"' + $asset + '": 123'), ('"other.zip": 123'))
    $index = ConvertFrom-ProfileIndex -Text $badSize -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'size for an unknown asset rejected'

    $largeIndex = '{' + (' ' * 262145)
    $index = ConvertFrom-ProfileIndex -Text $largeIndex -Version '0.1.0' -AssetName $asset
    Check-True (-not $index.Ok) 'profile index larger than 256 KiB rejected'
    Check-True ($index.Error -like '*too large*') 'oversized index reports the byte limit'

    # ---- outer bundle member allowlist ---------------------------------------

    $miniMembers = @('rpi.exe')
    Check-True (Test-BundleMembers -Members $miniMembers -ProfileName 'mini' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc') 'mini bundle with only rpi.exe accepted'
    Check-True (-not (Test-BundleMembers -Members @('rpi.exe', 'settings.defaults.json') -ProfileName 'mini' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'mini bundle with settings rejected'
    Check-True (-not (Test-BundleMembers -Members @() -ProfileName 'mini' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'bundle without rpi.exe rejected'

    $stdMembers = New-Object System.Collections.Generic.List[string]
    $stdMembers.Add('rpi.exe')
    $stdMembers.Add('settings.defaults.json')
    $stdMembers.Add('extensions/')
    foreach ($plugin in (Get-ProfilePluginList -ProfileName 'standard')) {
        $stdMembers.Add("extensions/$plugin-0.1.0-x86_64-pc-windows-msvc.rpix")
        $stdMembers.Add("extensions/$plugin-0.1.0-x86_64-pc-windows-msvc.rpix.sha256")
    }
    Check-True (Test-BundleMembers -Members @($stdMembers) -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc') 'exact standard bundle accepted'
    $duplicate = @($stdMembers) + @($stdMembers[0])
    Check-True (-not (Test-BundleMembers -Members $duplicate -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'duplicate bundle member rejected'
    $extra = @($stdMembers) + @('extensions/rpi-usage-0.1.0-x86_64-pc-windows-msvc.rpix')
    Check-True (-not (Test-BundleMembers -Members $extra -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'unregistered plugin member rejected'
    $extra = @($stdMembers) + @('../evil')
    Check-True (-not (Test-BundleMembers -Members $extra -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'unregistered traversal member rejected'
    $missing = @($stdMembers | Where-Object { $_ -notlike '*.sha256' })
    Check-True (-not (Test-BundleMembers -Members $missing -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'missing plugin sidecar member rejected'
    $noSettings = @($stdMembers | Where-Object { $_ -ne 'settings.defaults.json' })
    Check-True (-not (Test-BundleMembers -Members $noSettings -ProfileName 'standard' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'missing settings.defaults.json rejected'
    Check-True (-not (Test-BundleMembers -Members @('rpi.exe', 'extensions/') -ProfileName 'mini' -Version '0.1.0' -Target 'x86_64-pc-windows-msvc')) 'mini bundle with an extensions dir rejected'

    # ---- asset meta persistence + real zip member listing -------------------

    $metaPath = Join-Path $tempRoot 'asset.meta'
    Write-AssetMetaFile -Path $metaPath -Sha ('b' * 64) -Asset 'asset.tar.gz' `
        -Version '0.1.0' -ETag '"etag"' -LastModified '' -Url 'https://example/asset.tar.gz'
    # Set-Content writes CRLF on Windows PowerShell 5.1; strip CR so the
    # multiline anchors work on both PS editions.
    $metaText = (Get-Content -LiteralPath $metaPath -Raw) -replace "`r", ''
    Check-True ($metaText -match '(?m)^sha=b{64}$') 'asset meta records the sha'
    Check-True ($metaText -match '(?m)^etag="etag"$') 'asset meta records the etag'
    Check-True ($metaText -match '(?m)^url=https://example/asset.tar.gz$') 'asset meta records the url'

    $memberZip = Join-Path $tempRoot 'members.zip'
    $memberStream = [System.IO.File]::Open($memberZip, [System.IO.FileMode]::Create)
    $memberArchive = New-Object System.IO.Compression.ZipArchive($memberStream, [System.IO.Compression.ZipArchiveMode]::Create)
    foreach ($memberName in @('rpi.exe', 'settings.defaults.json', 'extensions/')) {
        if ($memberName.EndsWith('/')) { [void]$memberArchive.CreateEntry($memberName); continue }
        $memberEntry = $memberArchive.CreateEntry($memberName)
        $memberWriter = New-Object System.IO.StreamWriter($memberEntry.Open())
        $memberWriter.Write('x')
        $memberWriter.Dispose()
    }
    $memberArchive.Dispose()
    $memberStream.Dispose()
    $names = @(Get-ZipMemberNames -Path $memberZip)
    Check-True ($names.Count -eq 3) 'zip member names are listed'
    Check-True ($names -contains 'extensions/') 'zip directory entry is listed'

    $corruptZip = Join-Path $tempRoot 'corrupt.zip'
    Set-Content -LiteralPath $corruptZip -Value 'not a zip'
    $threw = $false
    try { Get-ZipMemberNames -Path $corruptZip | Out-Null } catch { $threw = $true }
    Check-True $threw 'unreadable zip raises for a clean failure path'

    # ---- safe archive members / zip extraction -------------------------------

    Check-True (Test-SafeArchiveMemberName -Name 'extensions/rpi-todo-0.1.0-x86_64-pc-windows-msvc.rpix') 'safe relative member'
    Check-True (-not (Test-SafeArchiveMemberName -Name '../evil')) 'parent traversal rejected'
    Check-True (-not (Test-SafeArchiveMemberName -Name '/etc/passwd')) 'absolute path rejected'
    Check-True (-not (Test-SafeArchiveMemberName -Name 'C:/windows/system32/x')) 'drive path rejected'
    Check-True (-not (Test-SafeArchiveMemberName -Name 'a/../../b')) 'nested traversal rejected'
    Check-True (-not (Test-SafeArchiveMemberName -Name '')) 'empty member rejected'

    $zipPath = Join-Path $tempRoot 'unsafe.zip'
    $zipStream = [System.IO.File]::Open($zipPath, [System.IO.FileMode]::Create)
    $zipArchive = New-Object System.IO.Compression.ZipArchive($zipStream, [System.IO.Compression.ZipArchiveMode]::Create)
    $entry = $zipArchive.CreateEntry('../evil.txt')
    $writer = New-Object System.IO.StreamWriter($entry.Open())
    $writer.Write('escape')
    $writer.Dispose()
    $zipArchive.Dispose()
    $zipStream.Dispose()
    $threw = $false
    try {
        Expand-SafeZipArchive -Path $zipPath -Destination (Join-Path $tempRoot 'unsafe-out')
    } catch {
        $threw = $true
    }
    Check-True $threw 'unsafe zip member rejected'
    Check-True (-not (Test-Path -LiteralPath (Join-Path $tempRoot 'evil.txt'))) 'unsafe zip wrote nothing outside'

    $goodZip = Join-Path $tempRoot 'good.zip'
    $zipStream = [System.IO.File]::Open($goodZip, [System.IO.FileMode]::Create)
    $zipArchive = New-Object System.IO.Compression.ZipArchive($zipStream, [System.IO.Compression.ZipArchiveMode]::Create)
    $entry = $zipArchive.CreateEntry('extensions/rpi-todo-0.1.0.rpix')
    $writer = New-Object System.IO.StreamWriter($entry.Open())
    $writer.Write('payload')
    $writer.Dispose()
    $zipArchive.Dispose()
    $zipStream.Dispose()
    $goodOut = Join-Path $tempRoot 'good-out'
    Expand-SafeZipArchive -Path $goodZip -Destination $goodOut
    Check-True (Test-Path -LiteralPath (Join-Path (Join-Path $goodOut 'extensions') 'rpi-todo-0.1.0.rpix')) 'safe zip extracts'

    # ---- rpix SHA256SUMS coverage -------------------------------------------

    $sumsDir = Join-Path $tempRoot 'sums'
    New-Item -ItemType Directory -Force -Path $sumsDir | Out-Null
    Set-Content -LiteralPath (Join-Path $sumsDir 'libtest.so') -Value 'cdylib' -NoNewline
    Set-Content -LiteralPath (Join-Path $sumsDir 'rpi-extension.json') -Value '{}' -NoNewline
    $libHash = (Get-FileSha256 (Join-Path $sumsDir 'libtest.so'))
    $jsonHash = (Get-FileSha256 (Join-Path $sumsDir 'rpi-extension.json'))
    @("$libHash  libtest.so", "$jsonHash  rpi-extension.json") |
        Set-Content -LiteralPath (Join-Path $sumsDir 'SHA256SUMS')
    Check-True (Test-RpixSums -Directory $sumsDir) 'exact SHA256SUMS coverage accepted'
    Set-Content -LiteralPath (Join-Path $sumsDir 'extra.txt') -Value 'unlisted'
    Check-True (-not (Test-RpixSums -Directory $sumsDir)) 'unlisted file rejected'
    Remove-Item -LiteralPath (Join-Path $sumsDir 'extra.txt')
    Remove-Item -LiteralPath (Join-Path $sumsDir 'libtest.so')
    Check-True (-not (Test-RpixSums -Directory $sumsDir)) 'missing listed file rejected'

    # ---- Range resume decision matrix ---------------------------------------

    Check-Equal 'fresh' (Get-RangeResumeDecision -Status 200 -CachedValidator '' -ResponseValidator '' -HasPartial $false) 'fresh download'
    Check-Equal 'restart' (Get-RangeResumeDecision -Status 200 -CachedValidator '"a"' -ResponseValidator '"a"' -HasPartial $true) '200 with a partial restarts'
    Check-Equal 'append' (Get-RangeResumeDecision -Status 206 -CachedValidator '"a"' -ResponseValidator '"a"' -HasPartial $true) 'matching 206 appends'
    Check-Equal 'restart' (Get-RangeResumeDecision -Status 206 -CachedValidator '"a"' -ResponseValidator '"b"' -HasPartial $true) 'validator change restarts'
    Check-Equal 'restart' (Get-RangeResumeDecision -Status 206 -CachedValidator '"a"' -ResponseValidator '' -HasPartial $true) '206 without a response validator restarts'
    Check-Equal 'restart' (Get-RangeResumeDecision -Status 206 -CachedValidator '' -ResponseValidator '"a"' -HasPartial $true) '206 without a cached validator restarts'
    Check-Equal 'verify-or-restart' (Get-RangeResumeDecision -Status 416 -CachedValidator '"a"' -ResponseValidator '' -HasPartial $true) '416 verifies or restarts'
    Check-Equal 'retry' (Get-RangeResumeDecision -Status 500 -CachedValidator '"a"' -ResponseValidator '' -HasPartial $true) '5xx retries'

    # ---- Retry-After / jitter ----------------------------------------------

    Check-Equal '7' (Get-RetryAfterSeconds -Value '7') 'Retry-After delta seconds'
    Check-Equal '60' (Get-RetryAfterSeconds -Value '999') 'Retry-After is capped at 60s'
    Check-Equal '0' (Get-RetryAfterSeconds -Value '0') 'Retry-After zero is immediate'
    Check-True ($null -eq (Get-RetryAfterSeconds -Value 'not-a-date')) 'invalid Retry-After is ignored'
    $retryDate = [DateTimeOffset]::UtcNow.AddSeconds(10).ToString('R')
    $retrySeconds = Get-RetryAfterSeconds -Value $retryDate
    Check-True ($retrySeconds -ge 8 -and $retrySeconds -le 10) 'Retry-After HTTP-date is parsed'
    Check-Equal '5' (Get-RetryDelaySeconds -Attempt 1 -RetryAfter '5') 'Retry-After wins over backoff'
    $delay1 = Get-RetryDelaySeconds -Attempt 1 -RetryAfter ''
    Check-True ($delay1 -ge 0.8 -and $delay1 -le 1.2) 'first backoff has +/-20% jitter'
    $delay4 = Get-RetryDelaySeconds -Attempt 4 -RetryAfter ''
    Check-True ($delay4 -ge 6.4 -and $delay4 -le 9.6) 'fourth backoff is 8s with jitter'
    $delayCap = Get-RetryDelaySeconds -Attempt 9 -RetryAfter ''
    Check-True ($delayCap -ge 6.4 -and $delayCap -le 9.6) 'later attempts stay capped at 8s (with jitter)'

    # ---- safe tar archive (only when tar.exe is available) -------------------

    if (Get-Command tar -ErrorAction SilentlyContinue) {
        $tarStage = Join-Path $tempRoot 'tar-stage'
        New-Item -ItemType Directory -Force -Path $tarStage | Out-Null
        Set-Content -LiteralPath (Join-Path $tarStage 'rpi-extension.json') -Value '{}'
        $tarPath = Join-Path $tempRoot 'safe-rpix.tgz'
        & tar -czf $tarPath -C $tarStage rpi-extension.json
        Check-True (Test-SafeTarArchive -Path $tarPath) 'safe tar archive accepted'
    } else {
        Write-Host 'ok - safe tar archive accepted # SKIP (tar.exe unavailable)'
    }

    # ---- settings seeding ---------------------------------------------------

    $seedRoot = Join-Path $tempRoot 'seed'
    New-Item -ItemType Directory -Force -Path $seedRoot | Out-Null
    $seedDefaults = Join-Path $seedRoot 'settings.defaults.json'
    $seedAgent = Join-Path $seedRoot 'agent'
    $seedSettings = Join-Path $seedAgent 'settings.json'
    [IO.File]::WriteAllText($seedDefaults, '{"packages":["rpi-todo"]}')
    Check-True (Seed-DefaultSettings -DefaultsPath $seedDefaults -SettingsPath $seedSettings -AgentDir $seedAgent) 'settings seeding succeeds on a fresh agent dir'
    Check-Equal '{"packages":["rpi-todo"]}' (Get-Content -LiteralPath $seedSettings -Raw) 'settings seeding copies the defaults byte-for-byte'
    Check-True (-not (Test-Path -LiteralPath "$seedSettings.stage-$PID")) 'settings seeding removes its stage file'

    $badAgent = Join-Path $seedRoot 'agent-is-a-file'
    [IO.File]::WriteAllText($badAgent, 'not a directory')
    $badSettings = Join-Path $badAgent 'settings.json'
    $seedOk = $true
    try {
        $seedOk = Seed-DefaultSettings -DefaultsPath $seedDefaults -SettingsPath $badSettings -AgentDir $badAgent
    } catch {
        $seedOk = $false
    }
    Check-True (-not $seedOk) 'settings seeding failure returns false instead of throwing'
    Check-Equal 'not a directory' (Get-Content -LiteralPath $badAgent -Raw) 'settings seeding failure leaves the agent path untouched'
    Check-True (-not (Test-Path -LiteralPath $badSettings)) 'settings seeding failure writes no settings file'

    # The finally block must not revive the failure when the stage parent is
    # itself inaccessible (Test-Path throws under ErrorActionPreference=Stop).
    $deniedAgent = Join-Path $seedRoot 'agent-denied'
    New-Item -ItemType Directory -Force -Path $deniedAgent | Out-Null
    if ($env:OS -ne 'Windows_NT' -and (Get-Command chmod -ErrorAction SilentlyContinue)) {
        & chmod 000 $deniedAgent 2>$null | Out-Null
        $deniedThrew = $false
        $deniedOk = $true
        try {
            $deniedOk = Seed-DefaultSettings -DefaultsPath $seedDefaults `
                -SettingsPath (Join-Path $deniedAgent 'settings.json') -AgentDir $deniedAgent 3>$null
        } catch {
            $deniedThrew = $true
        }
        & chmod 755 $deniedAgent 2>$null | Out-Null
        Check-True (-not $deniedThrew) 'settings seeding finally never throws on an inaccessible stage parent'
        Check-True (-not $deniedOk) 'settings seeding reports failure for an inaccessible agent dir'
        Check-True (-not (Test-Path -LiteralPath (Join-Path $deniedAgent 'settings.json'))) 'inaccessible agent dir gets no settings file'
    } elseif ($env:OS -ne 'Windows_NT') {
        Write-Host 'ok - settings seeding finally never throws # SKIP (chmod unavailable)'
    }

    # Windows PowerShell 5.1: deny write access to the agent dir instead.
    if ($env:OS -eq 'Windows_NT' -and (Get-Command icacls -ErrorAction SilentlyContinue)) {
        $aclAgent = Join-Path $seedRoot 'agent-acl'
        New-Item -ItemType Directory -Force -Path $aclAgent | Out-Null
        $who = "$env:USERNAME"
        & icacls $aclAgent /deny "${who}:(OI)(CI)(W)" 2>$null | Out-Null
        $aclBlocked = $false
        try {
            [IO.File]::WriteAllText((Join-Path $aclAgent 'probe-write'), 'x')
            Remove-Item -LiteralPath (Join-Path $aclAgent 'probe-write') -Force -ErrorAction SilentlyContinue
        } catch {
            $aclBlocked = $true
        }
        if ($aclBlocked) {
            $aclThrew = $false
            $aclOk = $true
            try {
                $aclOk = Seed-DefaultSettings -DefaultsPath $seedDefaults `
                    -SettingsPath (Join-Path $aclAgent 'settings.json') -AgentDir $aclAgent 3>$null
            } catch {
                $aclThrew = $true
            }
            Check-True (-not $aclThrew) 'settings seeding never throws when write access is denied (Windows ACL)'
            Check-True (-not $aclOk) 'settings seeding reports failure when write access is denied (Windows ACL)'
        } else {
            Write-Host 'ok - settings seeding Windows ACL case # SKIP (deny ACL did not take effect)'
        }
        & icacls $aclAgent /remove:d $who 2>$null | Out-Null
    }

    # ---- seven-day cache GC ------------------------------------------------

    $gcDir = Join-Path $tempRoot 'cache-gc'
    New-Item -ItemType Directory -Force -Path $gcDir | Out-Null
    $gcOld = Join-Path $gcDir 'old.part'
    $gcNew = Join-Path $gcDir 'new.part'
    $gcLocked = Join-Path $gcDir 'locked.part'
    $gcLock = Join-Path $gcDir 'locked.part.lock'
    [IO.File]::WriteAllText($gcOld, 'old')
    [IO.File]::WriteAllText($gcNew, 'new')
    [IO.File]::WriteAllText($gcLocked, 'locked')
    [IO.File]::WriteAllText($gcLock, '')
    (Get-Item -LiteralPath $gcOld).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    (Get-Item -LiteralPath $gcNew).LastWriteTimeUtc = [DateTime]::UtcNow
    (Get-Item -LiteralPath $gcLocked).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    (Get-Item -LiteralPath $gcLock).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    Invoke-CacheGc -PartsDir $gcDir
    Check-True (-not (Test-Path -LiteralPath $gcOld)) 'cache GC removes an old unlocked part'
    Check-True (Test-Path -LiteralPath $gcNew) 'cache GC keeps a recent part'
    Check-True (Test-Path -LiteralPath $gcLocked) 'cache GC keeps a part with an active lock'
    # The active lock is <asset>.lock, so <asset>.meta must probe that lock
    # instead of a non-existent <asset>.meta.lock.
    $gcLockedMeta = Join-Path $gcDir 'locked.part.meta'
    [IO.File]::WriteAllText($gcLockedMeta, 'sha=x')
    (Get-Item -LiteralPath $gcLockedMeta).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    Invoke-CacheGc -PartsDir $gcDir
    Check-True (Test-Path -LiteralPath $gcLockedMeta) 'cache GC keeps the meta of an actively locked asset'
    $gcStale = Join-Path $gcDir 'stale.part'
    $gcStaleMeta = Join-Path $gcDir 'stale.part.meta'
    [IO.File]::WriteAllText($gcStale, 'stale')
    [IO.File]::WriteAllText($gcStaleMeta, 'sha=y')
    (Get-Item -LiteralPath $gcStale).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    (Get-Item -LiteralPath $gcStaleMeta).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-8)
    Invoke-CacheGc -PartsDir $gcDir
    Check-True (-not (Test-Path -LiteralPath $gcStale)) 'cache GC removes an old unlocked part'
    Check-True (-not (Test-Path -LiteralPath $gcStaleMeta)) 'cache GC removes the meta of an old unlocked part'
    [IO.File]::WriteAllText($gcOld, 'old')
    Invoke-CacheGc -PartsDir $gcDir -Disabled
    Check-True (Test-Path -LiteralPath $gcOld) 'disabled cache GC leaves old parts'

    # ---- transactional binary/manifest commit -------------------------------

    $commitRoot = Join-Path $tempRoot 'commit'
    New-Item -ItemType Directory -Force -Path $commitRoot | Out-Null
    $commitOldBin = Join-Path $commitRoot 'rpi.exe'
    $commitOldManifest = Join-Path $commitRoot 'rpi.install.json'
    $commitStageBin = Join-Path $commitRoot '.rpi.stage-test.exe'
    $commitStageManifest = Join-Path $commitRoot 'rpi.install.json.stage-test'
    [IO.File]::WriteAllText($commitOldBin, 'OLD-BINARY')
    [IO.File]::WriteAllText($commitOldManifest, '{"version":"0.1.8"}')
    [IO.File]::WriteAllText($commitStageBin, 'NEW-BINARY')
    [IO.File]::WriteAllText($commitStageManifest, '{"version":"0.1.8-rc.4"}')
    Commit-InstallFiles -StageBin $commitStageBin -InstallPath $commitOldBin `
        -StageManifest $commitStageManifest -ManifestPath $commitOldManifest
    Check-Equal 'NEW-BINARY' (Get-Content -LiteralPath $commitOldBin -Raw) 'commit replaces the binary'
    Check-Equal '{"version":"0.1.8-rc.4"}' (Get-Content -LiteralPath $commitOldManifest -Raw) 'commit replaces the manifest'
    Check-True (-not (Test-Path -LiteralPath "$commitOldBin.backup-$PID")) 'commit removes the binary backup'
    Check-True (-not (Test-Path -LiteralPath "$commitOldManifest.backup-$PID")) 'commit removes the manifest backup'

    $failRoot = Join-Path $tempRoot 'commit-fail-upgrade'
    New-Item -ItemType Directory -Force -Path $failRoot | Out-Null
    $failBin = Join-Path $failRoot 'rpi.exe'
    $failManifestDir = Join-Path $failRoot 'rpi.install.json'
    $failStageBin = Join-Path $failRoot '.rpi.stage-fail.exe'
    $failStageManifest = Join-Path $failRoot 'rpi.install.json.stage-fail'
    [IO.File]::WriteAllText($failBin, 'OLD-BINARY')
    New-Item -ItemType Directory -Force -Path $failManifestDir | Out-Null
    [IO.File]::WriteAllText($failStageBin, 'NEW-BINARY')
    [IO.File]::WriteAllText($failStageManifest, '{"version":"0.1.8-rc.4"}')
    $failThrew = $false
    try {
        Commit-InstallFiles -StageBin $failStageBin -InstallPath $failBin `
            -StageManifest $failStageManifest -ManifestPath $failManifestDir
    } catch {
        $failThrew = $true
    }
    Check-True $failThrew 'commit fails when the manifest replace fails'
    Check-Equal 'OLD-BINARY' (Get-Content -LiteralPath $failBin -Raw) 'failed commit restores the old binary'
    Check-True (Test-Path -LiteralPath $failManifestDir -PathType Container) 'failed commit leaves the old manifest path untouched'
    Check-True (-not (Test-Path -LiteralPath "$failBin.backup-$PID")) 'failed commit removes the restored binary backup'
    Check-True (-not (Test-Path -LiteralPath $failStageManifest)) 'failed commit removes the unused stage manifest'

    $firstRoot = Join-Path $tempRoot 'commit-fail-first'
    New-Item -ItemType Directory -Force -Path $firstRoot | Out-Null
    $firstBin = Join-Path $firstRoot 'rpi.exe'
    $firstManifestDir = Join-Path $firstRoot 'rpi.install.json'
    $firstStageBin = Join-Path $firstRoot '.rpi.stage-first.exe'
    $firstStageManifest = Join-Path $firstRoot 'rpi.install.json.stage-first'
    New-Item -ItemType Directory -Force -Path $firstManifestDir | Out-Null
    [IO.File]::WriteAllText($firstStageBin, 'NEW-BINARY')
    [IO.File]::WriteAllText($firstStageManifest, '{"version":"0.1.8-rc.4"}')
    $firstThrew = $false
    try {
        Commit-InstallFiles -StageBin $firstStageBin -InstallPath $firstBin `
            -StageManifest $firstStageManifest -ManifestPath $firstManifestDir
    } catch {
        $firstThrew = $true
    }
    Check-True $firstThrew 'first install fails when the manifest replace fails'
    Check-True (-not (Test-Path -LiteralPath $firstBin)) 'failed first install leaves no new binary'
    Check-True (-not (Test-Path -LiteralPath $firstStageManifest)) 'failed first install removes the unused stage manifest'

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