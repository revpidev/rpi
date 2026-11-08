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
    Check-Equal 'verify-or-restart' (Get-RangeResumeDecision -Status 416 -CachedValidator '"a"' -ResponseValidator '' -HasPartial $true) '416 verifies or restarts'
    Check-Equal 'retry' (Get-RangeResumeDecision -Status 500 -CachedValidator '"a"' -ResponseValidator '' -HasPartial $true) '5xx retries'

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