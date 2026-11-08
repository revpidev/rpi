<#
.SYNOPSIS
    Official installer for the rpi CLI on Windows.

.DESCRIPTION
    Downloads the latest rpi release asset for x86_64-pc-windows-msvc from
    GitHub Releases, verifies its SHA-256 (an integrity check against
    corrupted downloads - it does NOT protect against tampered release
    assets; signing is a planned follow-up, see ADR-0011), installs the
    binary, and writes the install manifest (rpi.install.json) next to it.

    Usage:
      powershell -ExecutionPolicy Bypass -File install.ps1
      .\install.ps1 -Prefix C:\tools\rpi
      .\install.ps1 -AddPath
      .\install.ps1 -NoAddPath

    One-liner:
      irm https://revpi.dev/install.ps1 | iex

.PARAMETER Profile
    Install profile: mini (binary only), standard (default; rpi + six official
    plugins) or full (standard + rpi-usage). The profile only selects which
    prebuilt, hash-pinned archive is downloaded; rpi itself has no profile
    concept. Naming a profile does NOT by itself consent to the bundled native
    (L0) plugins - see -Yes.

.PARAMETER Yes
    Non-interactive consent to installing the bundle's official native (L0)
    plugins. Only an interactive profile-prompt answer (or Enter for the
    default) counts as consent otherwise; -Profile/RPI_INSTALL_PROFILE alone
    must be paired with -Yes/RPI_INSTALL_YES=1. mini needs no consent.
    Accepts RPI_INSTALL_YES=1 as an equivalent switch.

.PARAMETER NoCache
    Do not reuse cached partial/complete downloads. Accepts
    RPI_INSTALL_NO_CACHE=1 as an equivalent switch.

.PARAMETER Prefix
    Install directory (default: $env:LOCALAPPDATA\Programs\rpi).

.PARAMETER AddPath
    Add the install directory to the user PATH without prompting (when the
    prefix is inside the user profile). Accepts RPI_INSTALL_ADD_PATH=1 as an
    equivalent non-interactive switch.

.PARAMETER NoAddPath
    Never modify the user PATH. Accepts RPI_INSTALL_ADD_PATH=0 as an
    equivalent non-interactive switch.

.NOTES
    PATH behavior: when rpi is not the command resolved from PATH, an
    interactive run asks once (default: yes) before prepending the install
    directory to the HKCU user PATH. Interactive means a usable console with
    unredirected input/output and no CI/RPI_NONINTERACTIVE/TERM=dumb marker.
    The update preserves %VAR% entries and the registry value kind, removes
    duplicates (including %VAR% spellings), broadcasts WM_SETTINGCHANGE, and
    updates the current process PATH. If another rpi precedes the new entry in
    the combined machine+user PATH, the installer warns that new terminals may
    keep using that copy. -AddPath (or RPI_INSTALL_ADD_PATH=1) writes without
    prompting. Without that explicit request, non-interactive runs,
    -NoAddPath, system-wide prefixes and prefixes outside the user profile
    never modify PATH; they print the manual command instead.

    Environment overrides (for mirrors and tests):
      RPI_INSTALL_ADD_PATH   1/true/yes/on = add without prompting;
                             0/false/no/off = never add; unset = prompt when
                             the installer is interactive
      RPI_NONINTERACTIVE     1/true/yes/on = never prompt (explicit -AddPath
                             or RPI_INSTALL_ADD_PATH=1 still writes)
      RPI_GITHUB_API_URL     GitHub API URL for the latest release
                             (default: https://api.github.com/repos/revpidev/rpi/releases/latest)
      RPI_VERSION_CHECK_URL  Fallback version endpoint
                             (default: https://revpi.dev/api/latest-version)
      RPI_RELEASE_BASE_URL   Base used to construct GitHub download URLs
                             (default: https://github.com/revpidev/rpi/releases)
      RPI_SITE_BASE_URL      Official-site mirror base (default: https://revpi.dev);
                             assets are fetched from <site>/releases/download/...
      RPI_INSTALL_PROFILE    mini|standard|full (the -Profile flag wins)
      RPI_INSTALL_YES        1/true/yes/on = non-interactive native consent
      RPI_INSTALL_REQUIRE_PLUGINS  1 = exit 1 when any plugin fails
      RPI_INSTALL_NO_CACHE   1 = disable the download cache
      RPI_INSTALL_CACHE_DIR  cache root (default: %LOCALAPPDATA%\rpi\install)
      RPI_CODING_AGENT_DIR   agent dir (settings/extensions root)

    Download fallback order (ADR-0011 revision, for networks where GitHub is
    unreachable): 1) the browser_download_url pair returned by the GitHub API,
    2) the URL constructed by naming rule from RPI_RELEASE_BASE_URL, 3) the
    official-site mirror under RPI_SITE_BASE_URL. A failed DOWNLOAD moves on
    to the next candidate; a failed sha256 integrity check is always fatal.
#>
[CmdletBinding()]
param(
    [string]$Prefix = $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'Programs\rpi' } else { Join-Path $env:USERPROFILE '.local\bin' }),
    [string]$Profile = '',
    [switch]$Yes,
    [switch]$NoCache,
    [switch]$AddPath,
    [switch]$NoAddPath,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar slows large transfers on Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'

if ($Help) {
    Get-Help $PSCommandPath
    exit 0
}

$Repo = 'revpidev/rpi'
$GitHubApiUrl = if ($env:RPI_GITHUB_API_URL) { $env:RPI_GITHUB_API_URL } else { "https://api.github.com/repos/$Repo/releases/latest" }
$VersionCheckUrl = if ($env:RPI_VERSION_CHECK_URL) { $env:RPI_VERSION_CHECK_URL } else { 'https://revpi.dev/api/latest-version' }
$ReleaseBaseUrl = if ($env:RPI_RELEASE_BASE_URL) { $env:RPI_RELEASE_BASE_URL } else { "https://github.com/$Repo/releases" }
$SiteBaseUrl = if ($env:RPI_SITE_BASE_URL) { $env:RPI_SITE_BASE_URL } else { 'https://revpi.dev' }
$Target = 'x86_64-pc-windows-msvc'

function Fail([string]$Message) {
    Write-Host "error: $Message" -ForegroundColor Red
    exit 1
}

if ($AddPath -and $NoAddPath) {
    Fail "-AddPath and -NoAddPath are mutually exclusive"
}

$PathAction = 'prompt'
$PathActionEnv = ''
if ($env:RPI_INSTALL_ADD_PATH) {
    $PathActionEnv = $env:RPI_INSTALL_ADD_PATH.Trim()
}
if ($AddPath) {
    $PathAction = 'add'
} elseif ($NoAddPath) {
    $PathAction = 'skip'
} elseif ($PathActionEnv -match '^(1|true|yes|on)$') {
    $PathAction = 'add'
} elseif ($PathActionEnv -match '^(0|false|no|off)$') {
    $PathAction = 'skip'
}

# ---- PATH integration --------------------------------------------------------

# Environment-level non-interactive markers. Kept separate from the console
# checks so the decision is testable and so CI/pty runners (which may expose a
# console without ever writing to it) do not block the installer.
function Test-NonInteractiveEnvironment {
    if ($env:CI) {
        return $true
    }
    if ($env:RPI_NONINTERACTIVE -match '^(1|true|yes|on)$') {
        return $true
    }
    if ($env:TERM -eq 'dumb') {
        return $true
    }
    return $false
}

function Test-InstallerInteractiveInput {
    if (Test-NonInteractiveEnvironment) {
        return $false
    }
    try {
        if ([Console]::IsInputRedirected) {
            return $false
        }
        if ([Console]::IsOutputRedirected) {
            return $false
        }
    } catch {
        return $false
    }
    return $true
}

# Strip trailing separators, but keep a drive root ("C:\") a root: turning it
# into "C:" would make the PATH entry refer to the process's current directory
# on that drive.
function Remove-TrailingSeparators {
    param([string]$Value)

    if (-not $Value) {
        return ''
    }
    if ($Value -match '^[A-Za-z]:[\\/]$') {
        return $Value.Substring(0, 3)
    }
    return $Value.TrimEnd([char[]]@('\', '/'))
}

# Compare PATH entries with Windows semantics: strip surrounding quotes and
# expand %VAR% forms, while callers keep the original spelling for the registry
# write so user-managed variables are not rewritten.
function ConvertTo-NormalizedPathEntry {
    param([string]$Entry)

    if (-not $Entry) {
        return ''
    }
    $value = $Entry.Trim()
    if ($value.Length -ge 2 -and $value.StartsWith('"') -and $value.EndsWith('"')) {
        $value = $value.Substring(1, $value.Length - 2)
    }
    try {
        $value = [Environment]::ExpandEnvironmentVariables($value)
    } catch {
    }
    return Remove-TrailingSeparators $value
}

function Get-PathEntriesWithoutDirectory {
    param([string]$PathValue, [string]$Directory)

    $result = @()
    if (-not $PathValue) {
        return $result
    }
    $dir = ConvertTo-NormalizedPathEntry $Directory
    foreach ($entry in ($PathValue -split ';')) {
        if (-not $entry) {
            continue
        }
        $normalized = ConvertTo-NormalizedPathEntry $entry
        if (-not $normalized) {
            continue
        }
        if ([string]::Equals($normalized, $dir, [System.StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        $result += $entry
    }
    return $result
}

# Read user environment values directly from the registry instead of using
# [Environment]::GetEnvironmentVariable. The DoNotExpandEnvironmentNames flag
# keeps entries such as %USERPROFILE% literal so writing PATH back does not
# accidentally expand or rewrite user-managed variables.
function Get-UserEnv {
    param([string]$Key)

    $registerKey = Get-Item -Path "HKCU:"
    $envRegisterKey = $registerKey.OpenSubKey("Environment")
    if ($null -eq $envRegisterKey) {
        return $null
    }
    return $envRegisterKey.GetValue($Key, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
}

# Tell Explorer and newly launched terminals that HKCU:\Environment changed.
function Publish-EnvChange {
    if (-not ("Win32.NativeMethods" -as [Type])) {
        Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
    }

    $hwndBroadcast = [IntPtr]0xffff
    $wmSettingChange = 0x1a
    $result = [UIntPtr]::Zero
    [Win32.NativeMethods]::SendMessageTimeout($hwndBroadcast, $wmSettingChange, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$result) | Out-Null
}

# Preserve the existing registry value kind: REG_EXPAND_SZ stays expandable,
# REG_SZ stays literal. Do not infer ExpandString from a "%" in the new value
# (that would start expanding a literal percent sign in an install path).
function Set-UserEnv {
    param([string]$Key, [string]$Value)

    $registerKey = Get-Item -Path "HKCU:"
    $envRegisterKey = $registerKey.OpenSubKey("Environment", $true)
    if ($null -eq $envRegisterKey) {
        $envRegisterKey = $registerKey.CreateSubKey("Environment")
    }
    if ($null -eq $Value) {
        $envRegisterKey.DeleteValue($Key, $false)
    } else {
        $existing = $envRegisterKey.GetValue($Key, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $existing) {
            $registryValueKind = $envRegisterKey.GetValueKind($Key)
        } else {
            $registryValueKind = [Microsoft.Win32.RegistryValueKind]::String
        }
        $envRegisterKey.SetValue($Key, $Value, $registryValueKind)
    }

    try {
        Publish-EnvChange
    } catch {
        Write-Warning "PATH was updated, but the environment-change broadcast failed: $($_.Exception.Message)"
    }
}

function Add-UserPathEntry {
    param([string]$Directory, [switch]$Prepend)

    # Keep the caller's spelling for the registry write (a literal "%" in a
    # directory name must not be expanded by the installer); comparison inside
    # Get-PathEntriesWithoutDirectory still uses the normalized form.
    $dir = Remove-TrailingSeparators $Directory
    $pathValue = Get-UserEnv -Key 'Path'
    $path = @(Get-PathEntriesWithoutDirectory $pathValue $dir)
    if ($Prepend) {
        $path = @($dir) + $path
    } else {
        $path += $dir
    }
    Set-UserEnv -Key 'Path' -Value ($path -join ';')

    # Update the installer process too, so the rest of this run sees the new
    # directory without asking the user to restart first.
    $processPath = @(Get-PathEntriesWithoutDirectory $env:PATH $dir)
    if ($Prepend) {
        $env:PATH = ((@($dir) + $processPath) -join ';')
    } else {
        $env:PATH = (($processPath + $dir) -join ';')
    }
}

function Test-UserScopedPrefix {
    param([string]$Directory)

    $dir = Remove-TrailingSeparators $Directory
    $roots = @($env:USERPROFILE, $env:LOCALAPPDATA, $env:APPDATA) | Where-Object { $_ }
    foreach ($root in $roots) {
        $trimmed = Remove-TrailingSeparators $root
        if ($dir.Equals($trimmed, [System.StringComparison]::OrdinalIgnoreCase) -or
            $dir.StartsWith($trimmed + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Test-RpiInstalledIsFirstOnPath {
    param([string]$InstallPath)

    $active = Get-Command 'rpi' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $active -or -not $active.Source) {
        return $false
    }
    return [string]::Equals([string]$active.Source, $InstallPath, [System.StringComparison]::OrdinalIgnoreCase)
}

# Pure helper: find the first existing rpi.exe in a combined (machine then
# user) PATH string. Used to warn when a machine-level rpi still wins over the
# new user PATH entry in a fresh terminal.
function Find-FirstRpiOnCombinedPath {
    param([string]$CombinedPath)

    if (-not $CombinedPath) {
        return $null
    }
    foreach ($entry in ($CombinedPath -split ';')) {
        $candidateDir = ConvertTo-NormalizedPathEntry $entry
        if (-not $candidateDir) {
            continue
        }
        try {
            $candidate = Join-Path $candidateDir 'rpi.exe'
        } catch {
            continue
        }
        try {
            if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
                continue
            }
            return (Resolve-Path -LiteralPath $candidate).Path
        } catch {
            continue
        }
    }
    return $null
}

function Get-RpiPathShadowWarning {
    param([string]$Directory, [string]$MachinePath, [string]$UserPath)

    $combined = @()
    if ($MachinePath) {
        $combined += $MachinePath
    }
    if ($UserPath) {
        $combined += $UserPath
    }
    $first = Find-FirstRpiOnCombinedPath ($combined -join ';')
    if (-not $first) {
        return $null
    }
    $firstDir = ConvertTo-NormalizedPathEntry (Split-Path $first -Parent)
    $expectedDir = ConvertTo-NormalizedPathEntry $Directory
    if ([string]::Equals($firstDir, $expectedDir, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $null
    }
    return "Another rpi at $first precedes $Directory in the combined system+user PATH. New terminals may keep running that copy; remove or reorder the system PATH entry."
}

function Get-MachinePath {
    try {
        return [Environment]::GetEnvironmentVariable('Path', 'Machine')
    } catch {
        return $null
    }
}

function Get-InstallerParentShell {
    try {
        $currentProcess = Get-CimInstance Win32_Process -Filter "ProcessId = $PID" -ErrorAction Stop
        $parentProcess = Get-CimInstance Win32_Process -Filter "ProcessId = $($currentProcess.ParentProcessId)" -ErrorAction Stop
        if ($parentProcess -and $parentProcess.Name -ieq 'cmd.exe') {
            return 'cmd'
        }
    } catch {
    }
    return 'powershell'
}

function Escape-ForDoubleQuotedPowerShell {
    param([string]$Value)

    if (-not $Value) {
        return ''
    }
    return $Value.Replace('`', '``').Replace('"', '`"').Replace('$', '`$')
}

function Get-PathRefreshCommand {
    param([string]$Directory, [string]$Shell)

    if ($Shell -eq 'cmd') {
        return "set `"PATH=$Directory;%PATH%`""
    }
    $escaped = Escape-ForDoubleQuotedPowerShell $Directory
    return "`$env:PATH = `"$escaped;`$env:PATH`""
}

function Write-PathRefreshNote {
    param([string]$Directory)

    $command = Get-PathRefreshCommand $Directory (Get-InstallerParentShell)
    Write-Host ""
    Write-Host "Restart your terminal, or refresh this shell:"
    Write-Host "  $command"
}

# Manual fallback command: preserve the existing registry value kind and
# %VAR% spellings (unlike [Environment]::SetEnvironmentVariable, which writes a
# plain string).
function Get-ManualPathCommand {
    param([string]$Directory)

    $quoted = $Directory -replace "'", "''"
    return @(
        "`$key = Get-Item HKCU:\Environment",
        "`$kind = if (`$key.GetValue('Path', `$null)) { `$key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::String }",
        "`$key.SetValue('Path', '$quoted;' + [string]`$key.GetValue('Path', `$null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames), `$kind)"
    )
}

function Write-ManualPathNote {
    param([string]$Directory)

    Write-Host ""
    Write-Host "note: $Directory is not the active rpi on PATH; add it with:"
    foreach ($line in (Get-ManualPathCommand $Directory)) {
        Write-Host "  $line"
    }
    Write-Host "(then open a new terminal for the change to take effect)"
}

function Add-RpiToUserPath {
    param([string]$Directory)

    try {
        Add-UserPathEntry $Directory -Prepend
    } catch {
        Write-Warning "Could not update the user PATH: $($_.Exception.Message)"
        Write-ManualPathNote $Directory
        return
    }
    Write-Host ""
    Write-Host "Added $Directory to your user PATH."
    Write-PathRefreshNote $Directory
    $warning = Get-RpiPathShadowWarning $Directory (Get-MachinePath) (Get-UserEnv -Key 'Path')
    if ($warning) {
        Write-Warning $warning
    }
}

function Update-RpiPath {
    param([string]$InstallPath, [string]$Directory)

    if (Test-RpiInstalledIsFirstOnPath $InstallPath) {
        Write-Host ""
        Write-Host "rpi is already on PATH."
        $warning = Get-RpiPathShadowWarning $Directory (Get-MachinePath) (Get-UserEnv -Key 'Path')
        if ($warning) {
            Write-Warning $warning
        }
        return
    }

    $dir = Remove-TrailingSeparators $Directory
    if (-not (Test-UserScopedPrefix $dir)) {
        Write-Host ""
        Write-Host "note: $dir is outside your user profile; not modifying the user PATH automatically."
        Write-ManualPathNote $dir
        return
    }

    switch ($PathAction) {
        'skip' {
            Write-ManualPathNote $dir
        }
        'add' {
            Add-RpiToUserPath $dir
        }
        default {
            if (-not (Test-InstallerInteractiveInput)) {
                Write-ManualPathNote $dir
                return
            }
            try {
                $answer = Read-Host "Add $dir to your user PATH now? [Y/n]"
            } catch {
                Write-ManualPathNote $dir
                return
            }
            if ($null -eq $answer) {
                $answer = ''
            } else {
                $answer = ([string]$answer).Trim()
            }
            if (-not $answer -or $answer -notmatch '^(n|no)$') {
                Add-RpiToUserPath $dir
            } else {
                Write-ManualPathNote $dir
            }
        }
    }
}

# ---- install profile helpers (pure; exercised by scripts/test-install-ps1.ps1) ----

# Profile plugin sets. scripts/profiles.json is the packaging single source of
# truth; the shell installer carries the same list and the shell test asserts
# they match. The PowerShell test asserts full = standard + usage.
function Get-StandardPluginList {
    return @(
        'rpi-subagents'
        'rpi-todo'
        'rpi-ask-user-question'
        'rpi-smart-fetch'
        'rpi-statusline'
        'rpi-plan-mode'
    )
}

function Get-ProfilePluginList {
    param([string]$ProfileName)
    switch ($ProfileName.ToLowerInvariant()) {
        'mini' { return @() }
        'standard' { return @(Get-StandardPluginList) }
        'full' { return @(Get-StandardPluginList) + @('rpi-usage') }
        default { return @() }
    }
}

function Test-TruthyValue {
    param([string]$Value)
    if ($null -eq $Value) { return $false }
    return [bool]($Value.Trim() -match '^(1|true|yes|on)$')
}

# Consent matrix (install-packaging.md section 6.1): mini needs none; an
# interactive prompt choice (including Enter for the default) counts; a
# non-interactive run, timeout or -Profile/env alone needs -Yes/RPI_INSTALL_YES.
function Get-L0ConsentDecision {
    param([string]$ProfileName, [bool]$InteractiveChoice, [bool]$Yes)
    if ($ProfileName.ToLowerInvariant() -eq 'mini') { return $true }
    if ($InteractiveChoice) { return $true }
    return [bool]$Yes
}

function Get-FileSha256 {
    param([string]$Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $bytes = $sha.ComputeHash($stream)
            return ([System.BitConverter]::ToString($bytes) -replace '-', '').ToLowerInvariant()
        } finally {
            $sha.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

# Safe-archive member rules shared by the zip and tar paths: reject absolute
# paths, drive letters, NUL and any '..' component. Links are rejected by the
# callers (a tar type check / the zip external-attributes check).
function Test-SafeArchiveMemberName {
    param([string]$Name)
    if ([string]::IsNullOrEmpty($Name)) { return $false }
    $normalized = $Name -replace '\\', '/'
    if ($normalized.StartsWith('/')) { return $false }
    if ($normalized -match '^[A-Za-z]:') { return $false }
    if ($normalized -match '(^|/)\.\.(/|$)') { return $false }
    if ($normalized.Contains([string][char]0)) { return $false }
    return $true
}

function Compare-SemverVersion {
    param([string]$Left, [string]$Right)
    $pattern = '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$'
    if (-not ($Left -match $pattern)) { return 'invalid' }
    $l = @{ major = [int64]$Matches[1]; minor = [int64]$Matches[2]; patch = [int64]$Matches[3]; pre = $Matches[4] }
    if (-not ($Right -match $pattern)) { return 'invalid' }
    $r = @{ major = [int64]$Matches[1]; minor = [int64]$Matches[2]; patch = [int64]$Matches[3]; pre = $Matches[4] }
    foreach ($field in @('major', 'minor', 'patch')) {
        if ($l[$field] -lt $r[$field]) { return 'lt' }
        if ($l[$field] -gt $r[$field]) { return 'gt' }
    }
    if (-not $l.pre -and -not $r.pre) { return 'eq' }
    if (-not $l.pre) { return 'gt' }
    if (-not $r.pre) { return 'lt' }
    $leftIds = $l.pre.Split('.')
    $rightIds = $r.pre.Split('.')
    foreach ($id in @($leftIds + $rightIds)) {
        if ($id -notmatch '^[0-9A-Za-z-]+$') { return 'invalid' }
        if ($id -match '^[0-9]+$' -and $id.Length -gt 1 -and $id.StartsWith('0')) { return 'invalid' }
    }
    $count = [Math]::Max($leftIds.Count, $rightIds.Count)
    for ($i = 0; $i -lt $count; $i++) {
        if ($i -ge $leftIds.Count) { return 'lt' }
        if ($i -ge $rightIds.Count) { return 'gt' }
        $a = $leftIds[$i]
        $b = $rightIds[$i]
        $aNumeric = $a -match '^[0-9]+$'
        $bNumeric = $b -match '^[0-9]+$'
        if ($aNumeric -and $bNumeric) {
            if ([int64]$a -lt [int64]$b) { return 'lt' }
            if ([int64]$a -gt [int64]$b) { return 'gt' }
        } elseif ($aNumeric) {
            return 'lt'
        } elseif ($bNumeric) {
            return 'gt'
        } else {
            $cmp = [string]::CompareOrdinal($a, $b)
            if ($cmp -lt 0) { return 'lt' }
            if ($cmp -gt 0) { return 'gt' }
        }
    }
    return 'eq'
}

# Validate and read the flat rpi-pages profile index
# (install-packaging.md section 3). Duplicate keys are detected in the raw
# text because ConvertFrom-Json would silently keep the last one.
function ConvertFrom-ProfileIndex {
    param([string]$Text, [string]$Version, [string]$AssetName)
    $result = [pscustomobject]@{ Ok = $false; Sha = $null; Error = '' }
    if ([string]::IsNullOrWhiteSpace($Text)) {
        $result.Error = 'empty profile index'
        return $result
    }
    if ($Text.Length -gt 262144) {
        $result.Error = 'profile index is too large'
        return $result
    }
    $schemaCount = ([regex]::Matches($Text, '"schemaVersion"\s*:')).Count
    if ($schemaCount -ne 1) {
        $result.Error = "profile index must contain schemaVersion exactly once (found $schemaCount)"
        return $result
    }
    $versionCount = ([regex]::Matches($Text, '"version"\s*:')).Count
    if ($versionCount -ne 1) {
        $result.Error = "profile index must contain version exactly once (found $versionCount)"
        return $result
    }
    $assetPattern = '"' + [regex]::Escape($AssetName) + '"\s*:'
    $assetCount = ([regex]::Matches($Text, $assetPattern)).Count
    if ($assetCount -ne 1) {
        $result.Error = "asset entry missing or duplicated for $AssetName (found $assetCount)"
        return $result
    }
    try {
        $doc = $Text | ConvertFrom-Json
    } catch {
        $result.Error = "profile index is not valid JSON: $($_.Exception.Message)"
        return $result
    }
    if ([int]$doc.schemaVersion -ne 1) {
        $result.Error = "unsupported profile index schemaVersion: $($doc.schemaVersion)"
        return $result
    }
    if ([string]$doc.version -ne $Version) {
        $result.Error = "profile index version mismatch: index=$($doc.version) expected=$Version"
        return $result
    }
    $property = $doc.assets.PSObject.Properties |
        Where-Object { $_.Name -eq $AssetName } |
        Select-Object -First 1
    if (-not $property) {
        $result.Error = "asset entry missing from the profile index: $AssetName"
        return $result
    }
    $sha = [string]$property.Value
    if ($sha -notmatch '^[0-9a-fA-F]{64}$') {
        $result.Error = "profile index entry for $AssetName is not a 64-hex sha256"
        return $result
    }
    $result.Ok = $true
    $result.Sha = $sha.ToLowerInvariant()
    return $result
}

# 206/200/416 decision for a partial file (install-packaging.md section 7).
function Get-RangeResumeDecision {
    param([int]$Status, [string]$CachedValidator, [string]$ResponseValidator, [bool]$HasPartial)
    if (-not $HasPartial) { return 'fresh' }
    if ($Status -eq 200) { return 'restart' }
    if ($Status -eq 416) { return 'verify-or-restart' }
    if ($Status -eq 206) {
        if ($CachedValidator -and $ResponseValidator -and ($CachedValidator -ne $ResponseValidator)) {
            return 'restart'
        }
        return 'append'
    }
    return 'retry'
}

function Invoke-MetadataRequest {
    param(
        [string]$Url,
        [int]$TimeoutSeconds = 30,
        [int]$MaxBytes = 262144
    )
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.UserAgent = 'rpi-installer'
    $request.Timeout = $TimeoutSeconds * 1000
    $request.ReadWriteTimeout = $TimeoutSeconds * 1000
    $response = $null
    try {
        $response = $request.GetResponse()
        $stream = $response.GetResponseStream()
        $reader = New-Object System.IO.StreamReader($stream)
        try {
            $text = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
        if ($text.Length -gt $MaxBytes) {
            throw "response is too large ($($text.Length) bytes)"
        }
        return $text
    } finally {
        if ($response) { $response.Close() }
    }
}

# One HTTP transfer. Appends to an existing partial only when a validator was
# supplied (Range + If-Range); returns the status and validators so the caller
# can decide whether to append, restart or give up. No total timeout.
function Invoke-HttpTransfer {
    param(
        [string]$Url,
        [string]$PartPath,
        [long]$Offset,
        [string]$Validator,
        [int]$StallTimeout,
        [string]$LockPath,
        [switch]$NoProgress
    )
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.UserAgent = 'rpi-installer'
    $request.AllowAutoRedirect = $true
    $request.Timeout = -1
    $request.ReadWriteTimeout = $StallTimeout * 1000
    if ($Offset -gt 0 -and $Validator) {
        $request.AddRange($Offset)
        $request.Headers['If-Range'] = $Validator
    }
    $response = $null
    try {
        try {
            $response = $request.GetResponse()
        } catch [System.Net.WebException] {
            $webResponse = $_.Exception.Response
            if (-not $webResponse) { throw }
            $status = [int]$webResponse.StatusCode
            $webResponse.Close()
            return [pscustomobject]@{ Status = $status; ETag = $null; LastModified = $null; Truncated = $false }
        }
        $status = [int]$response.StatusCode
        $etag = [string]$response.Headers['ETag']
        $lastModified = [string]$response.Headers['Last-Modified']
        if ($status -eq 200 -and $Offset -gt 0) {
            return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $false }
        }
        if ($status -eq 416) {
            return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $false }
        }
        $fileMode = [System.IO.FileMode]::Create
        if ($Offset -gt 0 -and $status -eq 206) { $fileMode = [System.IO.FileMode]::Append }
        $file = [System.IO.File]::Open($PartPath, $fileMode, [System.IO.FileAccess]::Write)
        try {
            $stream = $response.GetResponseStream()
            $buffer = New-Object byte[] 65536
            $total = $response.ContentLength
            if ($total -gt 0 -and $Offset -gt 0) { $total = $total + $Offset }
            $written = $Offset
            $lastMark = 0
            $lastHeartbeat = [DateTime]::UtcNow
            while ($true) {
                $read = $stream.Read($buffer, 0, $buffer.Length)
                if ($read -le 0) { break }
                $file.Write($buffer, 0, $read)
                $written += $read
                if ($LockPath -and ([DateTime]::UtcNow - $lastHeartbeat).TotalSeconds -ge 30) {
                    try { [System.IO.File]::SetLastWriteTimeUtc($LockPath, [DateTime]::UtcNow) } catch { }
                    $lastHeartbeat = [DateTime]::UtcNow
                }
                if ($NoProgress) { continue }
                if ($total -gt 0) {
                    $percent = [int](($written * 100) / $total)
                    if ($percent -gt 100) { $percent = 100 }
                    if ([Console]::IsOutputRedirected) {
                        foreach ($mark in @(25, 50, 75, 100)) {
                            if ($percent -ge $mark -and $lastMark -lt $mark) {
                                Write-Host "progress: $mark%"
                                $lastMark = $mark
                            }
                        }
                    } else {
                        Write-Host -NoNewline ("`r  {0}%  {1:N1} / {2:N1} MB" -f $percent, ($written / 1048576.0), ($total / 1048576.0))
                    }
                }
            }
            if (-not $NoProgress -and -not [Console]::IsOutputRedirected) {
                Write-Host ''
            }
        } finally {
            $file.Dispose()
        }
        $truncated = ($total -gt 0 -and $written -lt $total)
        return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $truncated }
    } finally {
        if ($response) { $response.Close() }
    }
}

# Download one asset with cache reuse, Range/If-Range resume, a heartbeat lock,
# retries and mirror fallback. An integrity mismatch is fatal and never switches
# sources. Returns the path of the verified file.
function Invoke-AssetDownload {
    param(
        [string]$Asset,
        [string[]]$Urls,
        [string]$ExpectedSha,
        [string]$PartsDir,
        [int]$MaxAttempts = 5,
        [int]$StallTimeout = 30
    )
    $part = Join-Path $PartsDir $Asset
    $meta = "$part.meta"
    $lock = "$part.lock"

    if ((Test-Path -LiteralPath $part) -and (Test-Path -LiteralPath $meta)) {
        $metaText = Get-Content -LiteralPath $meta -Raw
        $metaSha = ([regex]::Match($metaText, '(?m)^sha=(.*)$')).Groups[1].Value.Trim()
        if ($ExpectedSha -and $metaSha -eq $ExpectedSha) {
            if ((Get-FileSha256 $part) -eq $ExpectedSha) {
                $cachedUrl = ([regex]::Match($metaText, '(?m)^url=(.*)$')).Groups[1].Value.Trim()
                if ($cachedUrl) { $script:LastDownloadUrl = $cachedUrl }
                Write-Host "using cached $Asset"
                return $part
            }
        }
    }
    if (Test-Path -LiteralPath $meta) {
        $metaText = Get-Content -LiteralPath $meta -Raw
        $metaSha = ([regex]::Match($metaText, '(?m)^sha=(.*)$')).Groups[1].Value.Trim()
        if ($ExpectedSha -and $metaSha -ne $ExpectedSha) {
            Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
        }
    }

    # Lock: atomic create; an active transfer refreshes the mtime, so only a
    # lock untouched for over an hour is treated as abandoned.
    while ($true) {
        try {
            New-Item -ItemType File -Path $lock -ErrorAction Stop | Out-Null
            break
        } catch {
            if (Test-Path -LiteralPath $lock) {
                $age = ([DateTime]::UtcNow - (Get-Item -LiteralPath $lock).LastWriteTimeUtc).TotalHours
                if ($age -gt 1) {
                    Write-Warning "reclaiming a stale download lock: $lock"
                    Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
                    continue
                }
            }
            Write-Warning "another installer is downloading this asset; waiting..."
            Start-Sleep -Seconds 2
        }
    }

    try {
        foreach ($url in $Urls) {
            if (-not $url) { continue }
            for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
                $offset = 0
                if (Test-Path -LiteralPath $part) {
                    $offset = (Get-Item -LiteralPath $part).Length
                }
                $validator = ''
                if ($offset -gt 0 -and (Test-Path -LiteralPath $meta)) {
                    $metaText = Get-Content -LiteralPath $meta -Raw
                    $validator = ([regex]::Match($metaText, '(?m)^etag=(.*)$')).Groups[1].Value.Trim()
                    if (-not $validator) {
                        $validator = ([regex]::Match($metaText, '(?m)^last_modified=(.*)$')).Groups[1].Value.Trim()
                    }
                    if (-not $validator) {
                        Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
                        $offset = 0
                    }
                }
                Write-Host "downloading $url (attempt $attempt/$MaxAttempts)"
                try {
                    $transfer = Invoke-HttpTransfer -Url $url -PartPath $part -Offset $offset `
                        -Validator $validator -StallTimeout $StallTimeout -LockPath $lock
                } catch {
                    Write-Warning "download failed from ${url}: $($_.Exception.Message)"
                    if ($attempt -ge $MaxAttempts) { break }
                    Start-Sleep -Seconds ([Math]::Min(8, [Math]::Pow(2, $attempt - 1)))
                    continue
                }
                if ($transfer.Status -eq 403 -or $transfer.Status -eq 404) {
                    Write-Warning "download failed from $url (HTTP $($transfer.Status)); trying the next mirror"
                    break
                }
                if ($transfer.Status -eq 416 -and $offset -eq 0) {
                    Write-Warning 'server rejected the range request; restarting from zero'
                    Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
                    continue
                }
                if ($transfer.Truncated) {
                    if (Test-Path -LiteralPath $part) {
                        $etagText = if ($transfer.ETag) { $transfer.ETag } else { '' }
                        $lastModifiedText = if ($transfer.LastModified) { $transfer.LastModified } else { '' }
                        @(
                            "sha=$ExpectedSha"
                            "asset=$Asset"
                            "version=$Version"
                            "etag=$etagText"
                            "last_modified=$lastModifiedText"
                            "url=$url"
                        ) | Set-Content -LiteralPath $meta -Encoding ASCII
                    }
                    if ($attempt -ge $MaxAttempts) {
                        Write-Warning "download truncated from $url after $attempt attempts; trying the next mirror"
                        break
                    }
                    Write-Warning 'download truncated; retrying with a resume request'
                    Start-Sleep -Seconds 1
                    continue
                }
                $hasPartial = ($offset -gt 0)
                $responseValidator = if ($transfer.ETag) { $transfer.ETag } else { $transfer.LastModified }
                $decision = Get-RangeResumeDecision -Status $transfer.Status -CachedValidator $validator `
                    -ResponseValidator $responseValidator -HasPartial $hasPartial
                switch ($decision) {
                    'restart' {
                        Write-Warning 'restarting from zero (discarded the partial file)'
                        Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
                        continue
                    }
                    'verify-or-restart' {
                        if ($ExpectedSha -and (Test-Path -LiteralPath $part) -and
                            (Get-FileSha256 $part) -eq $ExpectedSha) {
                            # Already complete; fall through to verification.
                        } else {
                            Write-Warning 'server rejected the resume range; restarting from zero'
                            Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
                            continue
                        }
                    }
                    'retry' {
                        if ($transfer.Status -eq 403 -or $transfer.Status -eq 404) {
                            Write-Warning "download failed from $url (HTTP $($transfer.Status)); trying the next mirror"
                            break
                        }
                        if ($attempt -ge $MaxAttempts) {
                            Write-Warning "download failed from $url after $attempt attempts; trying the next mirror"
                            break
                        }
                        Start-Sleep -Seconds ([Math]::Min(8, [Math]::Pow(2, $attempt - 1)))
                        continue
                    }
                }
                if ($transfer.Status -eq 206 -or $transfer.Status -eq 200) {
                    if (-not (Test-Path -LiteralPath $part) -or (Get-Item -LiteralPath $part).Length -eq 0) {
                        Write-Warning "empty download from $url"
                        continue
                    }
                    $etagText = if ($transfer.ETag) { $transfer.ETag } else { '' }
                    $lastModifiedText = if ($transfer.LastModified) { $transfer.LastModified } else { '' }
                    @(
                        "sha=$ExpectedSha"
                        "asset=$Asset"
                        "version=$Version"
                        "etag=$etagText"
                        "last_modified=$lastModifiedText"
                        "url=$url"
                    ) | Set-Content -LiteralPath $meta -Encoding ASCII
                    if ($ExpectedSha) {
                        $actual = Get-FileSha256 $part
                        if ($actual -ne $ExpectedSha) {
                            Remove-Item -Force $part, $meta -ErrorAction SilentlyContinue
                            throw "sha256 integrity check failed for $Asset (expected $ExpectedSha, got $actual)"
                        }
                    }
                    $script:LastDownloadUrl = $url
                    return $part
                }
                if ($attempt -ge $MaxAttempts) { break }
                Start-Sleep -Seconds ([Math]::Min(8, [Math]::Pow(2, $attempt - 1)))
            }
        }
        throw "all download candidates failed for $Asset (tried: $($Urls -join ', '))"
    } finally {
        Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
    }
}

function Get-ZipMemberNames {
    param([string]$Path)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        return @($zip.Entries | ForEach-Object { $_.FullName })
    } finally {
        $zip.Dispose()
    }
}

# The outer bundle may only contain the binary, settings.defaults.json and the
# selected profile's .rpix (+sidecar) members; every expected member must be
# present. Mirrors install.sh's validate_outer_archive.
function Test-BundleMembers {
    param(
        [string[]]$Members,
        [string]$ProfileName,
        [string]$Version,
        [string]$Target
    )
    $expected = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    [void]$expected.Add('rpi.exe')
    if ($ProfileName -ne 'mini') {
        [void]$expected.Add('settings.defaults.json')
        foreach ($plugin in (Get-ProfilePluginList -ProfileName $ProfileName)) {
            [void]$expected.Add("extensions/$plugin-$Version-$Target.rpix")
            [void]$expected.Add("extensions/$plugin-$Version-$Target.rpix.sha256")
        }
    }
    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    foreach ($member in $Members) {
        if ([string]::IsNullOrEmpty($member)) { continue }
        $name = $member -replace '\\', '/'
        if ($name.StartsWith('./')) { $name = $name.Substring(2) }
        if ($name -eq 'extensions/' -or $name -eq 'extensions') {
            if ($ProfileName -eq 'mini') { return $false }
            continue
        }
        if (-not $expected.Contains($name)) { return $false }
        [void]$seen.Add($name)
    }
    foreach ($name in $expected) {
        if (-not $seen.Contains($name)) { return $false }
    }
    return $true
}

# Reject unsafe zip members, symlinks and non-file entries before writing.
function Expand-SafeZipArchive {
    param([string]$Path, [string]$Destination)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            if (-not (Test-SafeArchiveMemberName -Name $name)) {
                throw "unsafe archive member: $name"
            }
            $mode = ($entry.ExternalAttributes -shr 16) -band 0xFFFF
            if (($mode -band 0xF000) -eq 0xA000) {
                throw "archive contains a symlink: $name"
            }
            $target = Join-Path $Destination ($name -replace '/', [System.IO.Path]::DirectorySeparatorChar)
            if ($name.EndsWith('/')) {
                New-Item -ItemType Directory -Force -Path $target | Out-Null
                continue
            }
            $parent = Split-Path -Parent $target
            if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
        }
    } finally {
        $zip.Dispose()
    }
}

function Get-TarMemberList {
    param([string]$Path)
    $members = & tar -tzf $Path 2>$null
    if ($LASTEXITCODE -ne 0) { throw "cannot list archive $Path" }
    return @($members)
}

function Test-SafeTarArchive {
    param([string]$Path)
    $members = @(Get-TarMemberList -Path $Path)
    if ($members.Count -eq 0) { return $false }
    foreach ($name in $members) {
        $normalized = $name
        if ($normalized.StartsWith('./')) { $normalized = $normalized.Substring(2) }
        if (-not (Test-SafeArchiveMemberName -Name $normalized)) { return $false }
    }
    $types = @(& tar -tvzf $Path 2>$null | ForEach-Object {
        if ($_.Length -gt 0) { $_.Substring(0, 1) }
    } | Sort-Object -Unique)
    if ($LASTEXITCODE -ne 0) { return $false }
    foreach ($type in $types) {
        if ($type -ne '-' -and $type -ne 'd') { return $false }
    }
    return $true
}

function Test-RpixSums {
    param([string]$Directory)
    $sumsPath = Join-Path $Directory 'SHA256SUMS'
    if (-not (Test-Path -LiteralPath $sumsPath -PathType Leaf)) { return $false }
    $listed = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    foreach ($line in (Get-Content -LiteralPath $sumsPath)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if ($line -notmatch '^([0-9a-fA-F]{64})\s+(.+)$') { return $false }
        $expected = $Matches[1].ToLowerInvariant()
        $relative = $Matches[2].Replace('\', '/')
        if (-not (Test-SafeArchiveMemberName -Name $relative)) { return $false }
        if (-not $listed.Add($relative)) { return $false }
        $file = Join-Path $Directory ($relative -replace '/', [System.IO.Path]::DirectorySeparatorChar)
        if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { return $false }
        if ((Get-FileSha256 $file) -ne $expected) { return $false }
    }
    foreach ($file in (Get-ChildItem -LiteralPath $Directory -File -Recurse)) {
        $relative = $file.FullName.Substring($Directory.Length).TrimStart('\', '/').Replace('\', '/')
        if ($relative -eq 'SHA256SUMS') { continue }
        if (-not $listed.Contains($relative)) { return $false }
    }
    return $true
}

function Install-ProfilePlugins {
    param(
        [string]$BundleDir,
        [string]$Version,
        [string]$ProfileName,
        [string]$Target,
        [string]$AgentDir
    )
    $pluginDir = Join-Path $AgentDir 'extensions'
    New-Item -ItemType Directory -Force -Path $pluginDir | Out-Null
    $installed = 0
    $failed = 0
    $failedNames = @()
    foreach ($plugin in (Get-ProfilePluginList -ProfileName $ProfileName)) {
        $rpixName = "$plugin-$Version-$Target.rpix"
        $rpix = Join-Path $BundleDir (Join-Path 'extensions' $rpixName)
        $sidecar = "$rpix.sha256"
        if (-not (Test-Path -LiteralPath $rpix -PathType Leaf) -or
            -not (Test-Path -LiteralPath $sidecar -PathType Leaf)) {
            Write-Warning "$plugin`: missing .rpix or sidecar in the bundle"
            $failed++
            $failedNames += $plugin
            continue
        }
        $sidecarSha = ((Get-Content -LiteralPath $sidecar -Raw).Trim() -split '\s+')[0]
        if ($sidecarSha -notmatch '^[0-9a-fA-F]{64}$' -or
            (Get-FileSha256 $rpix) -ne $sidecarSha.ToLowerInvariant()) {
            Write-Warning "$plugin`: .rpix sidecar mismatch"
            $failed++
            $failedNames += $plugin
            continue
        }
        if (-not (Test-SafeTarArchive -Path $rpix)) {
            Write-Warning "$plugin`: unsafe or unsupported .rpix members"
            $failed++
            $failedNames += $plugin
            continue
        }
        $tmp = Join-Path $pluginDir "$plugin.tmp-$PID"
        $old = Join-Path $pluginDir "$plugin.old-$PID"
        Remove-Item -Recurse -Force $tmp, $old -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $tmp | Out-Null
        $ok = $true
        try {
            & tar -xzf $rpix -C $tmp 2>$null
            if ($LASTEXITCODE -ne 0 -or -not (Test-RpixSums -Directory $tmp)) { $ok = $false }
            if ($ok) {
                $manifestPath = Join-Path $tmp 'rpi-extension.json'
                if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
                    $ok = $false
                } else {
                    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
                    if ([string]$manifest.name -ne $plugin -or
                        [string]$manifest.version -ne $Version -or
                        [int]$manifest.rpiAbi -ne 1) {
                        $ok = $false
                    } elseif ($manifest.minHostVersion) {
                        $comparison = Compare-SemverVersion -Left ([string]$manifest.minHostVersion) -Right $Version
                        if ($comparison -eq 'invalid' -or $comparison -eq 'gt') { $ok = $false }
                    }
                }
            }
        } catch {
            $ok = $false
        }
        if (-not $ok) {
            Write-Warning "$plugin`: rejected (checksum, manifest or compatibility)"
            Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
            $failed++
            $failedNames += $plugin
            continue
        }
        $final = Join-Path $pluginDir $plugin
        try {
            if (Test-Path -LiteralPath $final) { Move-Item -LiteralPath $final -Destination $old -Force }
            Move-Item -LiteralPath $tmp -Destination $final -Force
            Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue
            $installed++
        } catch {
            Write-Warning "$plugin`: atomic replace failed: $($_.Exception.Message)"
            if (Test-Path -LiteralPath $old) {
                Move-Item -LiteralPath $old -Destination $final -Force -ErrorAction SilentlyContinue
            }
            Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
            $failed++
            $failedNames += $plugin
        }
    }
    return [pscustomobject]@{
        Installed = $installed
        Failed = $failed
        FailedNames = $failedNames
        Total = $installed + $failed
    }
}

# ---- platform check ----------------------------------------------------------

$Target = 'x86_64-pc-windows-msvc'
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    Fail "unsupported CPU architecture: $env:PROCESSOR_ARCHITECTURE (only $Target builds are published)"
}
Write-Host "detected platform: $Target"

$YesFlag = [bool]$Yes -or (Test-TruthyValue $env:RPI_INSTALL_YES)
$NoCacheFlag = [bool]$NoCache -or (Test-TruthyValue $env:RPI_INSTALL_NO_CACHE)

# TLS 1.2 for Windows PowerShell 5.1 (default there is TLS 1.0).
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

# ---- version / asset URL resolution ------------------------------------------

$Version = $null
$ReleaseAssets = @()

try {
    $release = Invoke-RestMethod -Uri $GitHubApiUrl -Headers @{ 'User-Agent' = 'rpi-installer' }
    $v = ([string]$release.tag_name) -replace '^v', ''
    if ($v) {
        $Version = $v
        $ReleaseAssets = @($release.assets)
    }
} catch { }

if ($Version) {
    Write-Host "latest release: v$Version (via GitHub API)"
} else {
    Write-Warning "GitHub API unavailable; falling back to version endpoint"
    try {
        $infoDoc = Invoke-RestMethod -Uri $VersionCheckUrl
    } catch {
        Fail "version endpoint $VersionCheckUrl is unreachable: $($_.Exception.Message)"
    }
    $Version = ([string]$infoDoc.version) -replace '^v', ''
    if (-not $Version) { Fail "version endpoint $VersionCheckUrl did not return a version" }
    Write-Host "latest release: v$Version (via version endpoint)"
}

function Get-ReleaseAssetSize {
    param([object[]]$Assets, [string]$Name)
    $asset = $Assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1
    if ($asset -and $asset.size) { return [int64]$asset.size }
    return 0
}

function Format-SizeHint {
    param([int64]$Bytes)
    if ($Bytes -le 0) { return '' }
    return (' ({0:N1} MB)' -f ($Bytes / 1048576.0))
}

# ---- profile selection + L0 consent -----------------------------------------

$ProfileValue = ''
if ($Profile) {
    $ProfileValue = $Profile.Trim().ToLowerInvariant()
} elseif ($env:RPI_INSTALL_PROFILE) {
    $ProfileValue = $env:RPI_INSTALL_PROFILE.Trim().ToLowerInvariant()
}
if ($ProfileValue -and @('mini', 'standard', 'full') -notcontains $ProfileValue) {
    Fail "invalid profile: $ProfileValue (expected mini, standard or full)"
}

$Consent = $false
$ProfileSource = 'flag/env'
if ($ProfileValue) {
    if ($ProfileValue -ne 'mini' -and (Test-InstallerInteractiveInput)) {
        Write-Host ''
        Write-Host "The '$ProfileValue' profile installs these official plugins:"
        foreach ($plugin in (Get-ProfilePluginList -ProfileName $ProfileValue)) {
            Write-Host "  - $plugin"
        }
        Write-Host 'Official plugins are native (L0) and run unsandboxed with your permissions.'
        $answer = Read-Host "Install these native plugins with profile '$ProfileValue'? [y/N]"
        $Consent = [bool]($answer -and $answer.Trim() -match '^(y|yes)$')
    }
} else {
    if (Test-InstallerInteractiveInput) {
        $miniSize = Get-ReleaseAssetSize $ReleaseAssets "rpi-$Version-$Target.zip"
        $stdSize = Get-ReleaseAssetSize $ReleaseAssets "rpi-$Version-$Target-standard.zip"
        $fullSize = Get-ReleaseAssetSize $ReleaseAssets "rpi-$Version-$Target-full.zip"
        Write-Host ''
        Write-Host 'Choose an install profile:'
        Write-Host "  1) mini      rpi core only - no plugins$(Format-SizeHint $miniSize)"
        Write-Host "  2) standard  rpi + 6 official plugins (recommended)$(Format-SizeHint $stdSize)"
        Write-Host '               rpi-subagents, rpi-todo, rpi-ask-user-question,'
        Write-Host '               rpi-smart-fetch, rpi-statusline, rpi-plan-mode'
        Write-Host "  3) full      standard + usage$(Format-SizeHint $fullSize)"
        Write-Host ''
        Write-Host 'Everything above comes from ONE pinned archive - no plugin artifacts are'
        Write-Host 'downloaded. The archive sha256 is pinned by the revpi.dev profile index.'
        Write-Host 'Official plugins are native (L0) and run unsandboxed with your permissions.'
        Write-Host 'Selecting standard/full here explicitly consents to that group. Remove one'
        Write-Host 'later with `rpi remove <name>`.'
        $validChoice = $false
        while (-not $validChoice) {
            $choice = Read-Host 'Profile [1/2/3, Enter = standard]'
            if ($null -eq $choice) { $choice = '' }
            switch ($choice.Trim().ToLowerInvariant()) {
                '' { $ProfileValue = 'standard'; $Consent = $true; $validChoice = $true }
                '1' { $ProfileValue = 'mini'; $Consent = $false; $validChoice = $true }
                'mini' { $ProfileValue = 'mini'; $Consent = $false; $validChoice = $true }
                '3' { $ProfileValue = 'full'; $Consent = $true; $validChoice = $true }
                'full' { $ProfileValue = 'full'; $Consent = $true; $validChoice = $true }
                '2' { $ProfileValue = 'standard'; $Consent = $true; $validChoice = $true }
                'standard' { $ProfileValue = 'standard'; $Consent = $true; $validChoice = $true }
                default { Write-Warning "invalid choice: $choice (expected 1, 2, 3, or Enter)" }
            }
        }
    } else {
        $ProfileValue = 'standard'
        $Consent = $false
        $ProfileSource = 'noninteractive'
    }
}

if ($ProfileValue -ne 'mini' -and -not (Get-L0ConsentDecision -ProfileName $ProfileValue -InteractiveChoice $Consent -Yes $YesFlag)) {
    Write-Host ''
    Write-Host "The '$ProfileValue' profile installs these official plugins:"
    foreach ($plugin in (Get-ProfilePluginList -ProfileName $ProfileValue)) {
        Write-Host "  - $plugin"
    }
    Fail "refusing to install native plugins without consent (profile '$ProfileValue', source: $ProfileSource). Choose one of: re-run interactively and answer the profile prompt, add -Yes (or RPI_INSTALL_YES=1), or use -Profile mini. Nothing was downloaded."
}
if ($ProfileValue -ne 'mini') {
    Write-Host "profile: $ProfileValue (native-plugin consent: yes)"
}

# ---- profile index ------------------------------------------------------------

$AssetName = if ($ProfileValue -eq 'mini') {
    "rpi-$Version-$Target.zip"
} else {
    "rpi-$Version-$Target-$ProfileValue.zip"
}
$IndexSha = ''

if ($ProfileValue -ne 'mini') {
    $indexUrl = "$SiteBaseUrl/api/install-profiles/$Version.json"
    try {
        $indexText = Invoke-MetadataRequest -Url $indexUrl
    } catch {
        Fail "profile index unavailable for v$Version ($indexUrl): $($_.Exception.Message). Fix/retry the site index, or install without plugins with -Profile mini."
    }
    $index = ConvertFrom-ProfileIndex -Text $indexText -Version $Version -AssetName $AssetName
    if (-not $index.Ok) {
        Fail "profile index invalid for v$Version ($indexUrl): $($index.Error). Fix/retry the site index, or install without plugins with -Profile mini."
    }
    $IndexSha = $index.Sha
}

# ---- download -----------------------------------------------------------------

$TmpDir = Join-Path ([IO.Path]::GetTempPath()) ("rpi-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    if ($NoCacheFlag) {
        $cacheRoot = Join-Path $TmpDir 'cache'
    } elseif ($env:RPI_INSTALL_CACHE_DIR) {
        $cacheRoot = $env:RPI_INSTALL_CACHE_DIR
    } elseif ($env:LOCALAPPDATA) {
        $cacheRoot = Join-Path $env:LOCALAPPDATA 'rpi\install'
    } else {
        $cacheRoot = Join-Path $TmpDir 'cache'
    }
    $partsDir = Join-Path $cacheRoot 'parts'
    New-Item -ItemType Directory -Force -Path $partsDir | Out-Null

    $candidates = New-Object System.Collections.Generic.List[string]
    $apiAsset = $ReleaseAssets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    if ($apiAsset -and $apiAsset.browser_download_url) { $candidates.Add([string]$apiAsset.browser_download_url) }
    foreach ($url in @(
            "$ReleaseBaseUrl/download/v$Version/$AssetName",
            "$SiteBaseUrl/releases/download/v$Version/$AssetName")) {
        if ($url -and -not $candidates.Contains($url)) { $candidates.Add($url) }
    }

    Write-Host ''
    Write-Host "fetching $AssetName.sha256"
    $sidecarCandidates = @($candidates | ForEach-Object { "$_" + '.sha256' })
    try {
        $sidecarPath = Invoke-AssetDownload -Asset "$AssetName.sha256" -Urls $sidecarCandidates `
            -ExpectedSha '' -PartsDir $partsDir
    } catch {
        Fail "download failed for $AssetName.sha256: $($_.Exception.Message)"
    }
    $sidecarMatch = [regex]::Match((Get-Content -LiteralPath $sidecarPath -Raw), '^([0-9a-fA-F]{64})')
    if (-not $sidecarMatch.Success) { Fail "malformed checksum in $AssetName.sha256" }
    $expectedSha = $sidecarMatch.Groups[1].Value.ToLowerInvariant()
    if ($IndexSha -and $IndexSha -ne $expectedSha) {
        Fail "Release sidecar does not match the rpi-pages profile index for $AssetName (sidecar=$expectedSha, index=$IndexSha); aborting"
    }

    Write-Host ''
    $script:LastDownloadUrl = ''
    try {
        $archivePath = Invoke-AssetDownload -Asset $AssetName -Urls @($candidates) `
            -ExpectedSha $expectedSha -PartsDir $partsDir
    } catch {
        Fail $_.Exception.Message
    }
    if ($IndexSha -and (Get-FileSha256 $archivePath) -ne $IndexSha) {
        Fail "downloaded $AssetName does not match the rpi-pages profile index; aborting"
    }
    Write-Host 'sha256 integrity check passed'

    # ---- safe extraction + install ------------------------------------------

    $extractDir = Join-Path $TmpDir 'x'
    $bundleMembers = @(Get-ZipMemberNames -Path $archivePath)
    if (-not (Test-BundleMembers -Members $bundleMembers -ProfileName $ProfileValue -Version $Version -Target $Target)) {
        Fail "archive $AssetName contains unregistered or missing members; aborting"
    }
    Expand-SafeZipArchive -Path $archivePath -Destination $extractDir
    $extracted = Join-Path $extractDir 'rpi.exe'
    if (-not (Test-Path -LiteralPath $extracted -PathType Leaf)) {
        Fail 'archive did not contain an rpi.exe binary'
    }

    try {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
        $probe = Join-Path $Prefix ('.rpi-write-test-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType File -Path $probe | Out-Null
        Remove-Item $probe
    } catch {
        Fail "install directory $Prefix is not writable - re-run from an elevated PowerShell (Run as administrator)"
    }

    $InstallPath = Join-Path $Prefix 'rpi.exe'
    $stageBin = Join-Path $Prefix ".rpi.stage-$PID.exe"
    Copy-Item $extracted $stageBin -Force
    $PrefixAbs = (Resolve-Path $Prefix).Path
    $InstallPath = Join-Path $PrefixAbs 'rpi.exe'
    $existingManifestPath = Join-Path $PrefixAbs 'rpi.install.json'
    if (Test-Path -LiteralPath $existingManifestPath) {
        try {
            $existingManifest = Get-Content -LiteralPath $existingManifestPath -Raw | ConvertFrom-Json
            $currentVersion = [string]$existingManifest.version
            if ($currentVersion -eq $Version) {
                Write-Host ''
                Write-Host "rpi $currentVersion already installed; reinstalling"
            } elseif ($currentVersion) {
                Write-Host ''
                Write-Host "Upgrading rpi $currentVersion -> $Version (stable)"
            }
        } catch {
            Write-Warning "cannot read the existing install manifest; continuing anyway"
        }
    }
    $InstalledAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $manifest = [ordered]@{
        version     = $Version
        target      = $Target
        installedAt = $InstalledAt
        sourceUrl   = [string]$script:LastDownloadUrl
        sha256      = $expectedSha
        installPath = $InstallPath
        method      = 'binary'
    }
    $ManifestPath = Join-Path $PrefixAbs 'rpi.install.json'
    $stageManifest = "$ManifestPath.stage-$PID"
    [IO.File]::WriteAllText($stageManifest, ($manifest | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))
    Move-Item -LiteralPath $stageBin -Destination $InstallPath -Force
    Move-Item -LiteralPath $stageManifest -Destination $ManifestPath -Force

    $pluginResult = $null
    if ($ProfileValue -ne 'mini') {
        $AgentDir = if ($env:RPI_CODING_AGENT_DIR) { $env:RPI_CODING_AGENT_DIR } else { Join-Path $env:USERPROFILE '.rpi\agent' }
        $pluginResult = Install-ProfilePlugins -BundleDir $extractDir -Version $Version `
            -ProfileName $ProfileValue -Target $Target -AgentDir $AgentDir
        $settingsPath = Join-Path $AgentDir 'settings.json'
        if ($pluginResult.Failed -eq 0 -and -not (Test-Path -LiteralPath $settingsPath)) {
            $defaults = Join-Path $extractDir 'settings.defaults.json'
            if (Test-Path -LiteralPath $defaults -PathType Leaf) {
                New-Item -ItemType Directory -Force -Path $AgentDir | Out-Null
                $stageSettings = "$settingsPath.stage-$PID"
                Copy-Item -LiteralPath $defaults -Destination $stageSettings -Force
                Move-Item -LiteralPath $stageSettings -Destination $settingsPath -Force
                Write-Host "seeded default settings: $settingsPath"
            }
        } elseif (Test-Path -LiteralPath $settingsPath) {
            Write-Host ''
            Write-Host "existing settings left untouched: $settingsPath"
        } else {
            Write-Warning 'skipping settings seeding because not all plugins were installed'
        }
    }

    # ---- guidance -----------------------------------------------------------

    Write-Host ''
    Write-Host "rpi $Version installed - stable - profile $ProfileValue"
    Write-Host "  binary:   $InstallPath"
    Write-Host "  asset:    $AssetName"
    Write-Host "  manifest: $ManifestPath"
    if ($pluginResult -and $ProfileValue -ne 'mini') {
        $pluginsText = (@(Get-ProfilePluginList -ProfileName $ProfileValue) | ForEach-Object { "$_ $Version" }) -join ' - '
        Write-Host "  plugins:  $pluginsText"
        Write-Host "            $($pluginResult.Installed)/$($pluginResult.Total) installed"
        if ($pluginResult.Failed -eq 0) {
            Write-Host "            registered in $(Join-Path $AgentDir 'settings.json')"
        }
    }
    if ($pluginResult -and $pluginResult.Failed -gt 0) {
        Write-Warning "$($pluginResult.Failed) of $($pluginResult.Total) plugins failed"
        foreach ($name in $pluginResult.FailedNames) {
            Write-Warning "  retry: rpi install $name"
        }
        if (Test-TruthyValue $env:RPI_INSTALL_REQUIRE_PLUGINS) {
            exit 1
        }
    }
    Write-Host ''
    Write-Host 'next steps:'
    if ($ProfileValue -ne 'mini') {
        Write-Host '  rpi                  start rpi'
        Write-Host '  rpi update --all     update rpi + plugins   (self: rpi update --self)'
        Write-Host '  rpi list             see installed plugins'
        Write-Host '  rpi remove <name>    remove one plugin (e.g. rpi remove rpi-todo)'
        Write-Host '  rpi self-uninstall   remove rpi (--purge deletes ~/.rpi)'
    } else {
        Write-Host '  update:    rpi update --self'
        Write-Host '  uninstall: rpi self-uninstall   (add --purge to also delete ~/.rpi)'
    }

    Update-RpiPath $InstallPath $PrefixAbs
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
