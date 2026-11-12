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
      RPI_INSTALL_CHANNEL    stable|rc (default stable; rc reads the RC version
                             endpoint and the matching Release)
      RPI_INSTALL_ADD_PATH   1/true/yes/on = add without prompting;
                             0/false/no/off = never add; unset = prompt when
                             the installer is interactive
      RPI_NONINTERACTIVE     1/true/yes/on = never prompt (explicit -AddPath
                             or RPI_INSTALL_ADD_PATH=1 still writes)
      RPI_GITHUB_API_URL     Release metadata API override
                             (stable default: https://api.github.com/repos/revpidev/rpi/releases/latest)
      RPI_VERSION_CHECK_URL  Channel version endpoint override
                             (stable: latest-version.json; rc: latest-rc-version.json)
      RPI_RELEASE_BASE_URL   Base used to construct GitHub download URLs
                             (default: https://github.com/revpidev/rpi/releases)
      RPI_SITE_BASE_URL      Official-site mirror base (default: https://revpi.dev);
                             assets are fetched from <site>/releases/download/...
      RPI_INSTALL_PROFILE    mini|standard|full (the -Profile flag wins)
      RPI_INSTALL_YES        1/true/yes/on = non-interactive native consent
      RPI_INSTALL_REQUIRE_PLUGINS  1 = exit 1 when any plugin fails
      RPI_INSTALL_NO_CACHE   1 = disable the download cache
      RPI_INSTALL_CACHE_DIR  cache root (default: %LOCALAPPDATA%\rpi\install)
      RPI_INSTALL_ATTEMPTS   attempts per source (default 5, 1-20)
      RPI_INSTALL_STALL_TIMEOUT  body read stall timeout seconds (default 30)
      RPI_INSTALL_CONNECT_TIMEOUT  connect/response-header timeout seconds (default 10)
      RPI_INSTALL_PROMPT_TIMEOUT  interactive prompt timeout seconds (default 60)
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
$InstallChannel = if ($env:RPI_INSTALL_CHANNEL) { $env:RPI_INSTALL_CHANNEL.Trim().ToLowerInvariant() } else { 'stable' }
$GitHubApiUrl = if ($env:RPI_GITHUB_API_URL) { $env:RPI_GITHUB_API_URL } else { '' }
$VersionCheckUrl = if ($env:RPI_VERSION_CHECK_URL) {
    $env:RPI_VERSION_CHECK_URL
} elseif ($InstallChannel -eq 'rc') {
    'https://revpi.dev/api/latest-rc-version.json'
} else {
    'https://revpi.dev/api/latest-version'
}
$ReleaseBaseUrl = if ($env:RPI_RELEASE_BASE_URL) { $env:RPI_RELEASE_BASE_URL } else { "https://github.com/$Repo/releases" }
$SiteBaseUrl = if ($env:RPI_SITE_BASE_URL) { $env:RPI_SITE_BASE_URL } else { 'https://revpi.dev' }
$Target = 'x86_64-pc-windows-msvc'

# Download tuning mirrors install.sh (install-packaging.md section 7). Invalid
# or out-of-range values warn and fall back to the contract defaults instead
# of turning into an unbounded wait.
function Get-InstallEnvInt {
    param([string]$Name, [int]$Default, [int]$Minimum, [int]$Maximum)
    $raw = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($raw)) { return $Default }
    $value = 0
    if (-not [int]::TryParse($raw.Trim(), [ref]$value) -or $value -lt $Minimum -or $value -gt $Maximum) {
        Write-Warning "invalid $Name=$raw; using $Default"
        return $Default
    }
    return $value
}

$MaxAttempts = Get-InstallEnvInt -Name 'RPI_INSTALL_ATTEMPTS' -Default 5 -Minimum 1 -Maximum 20
$StallTimeout = Get-InstallEnvInt -Name 'RPI_INSTALL_STALL_TIMEOUT' -Default 30 -Minimum 1 -Maximum 3600
$ConnectTimeout = Get-InstallEnvInt -Name 'RPI_INSTALL_CONNECT_TIMEOUT' -Default 10 -Minimum 1 -Maximum 300
$PromptTimeout = Get-InstallEnvInt -Name 'RPI_INSTALL_PROMPT_TIMEOUT' -Default 60 -Minimum 1 -Maximum 3600

function Fail([string]$Message) {
    Write-Host "error: $Message" -ForegroundColor Red
    exit 1
}

if ($InstallChannel -notin @('stable', 'rc')) {
    Fail "RPI_INSTALL_CHANNEL must be stable or rc (got: $InstallChannel)"
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
# Read a line with a deadline. The shell installer uses `read -t`; PowerShell
# has no built-in timeout, so poll the console keys directly. Two sentinels are
# returned instead of a blocking read: __timeout__ on deadline and
# __unavailable__ when the host cannot poll the console at all. Callers must
# treat both (and $null) as "no answer" and refuse consent.
function Read-HostWithTimeout {
    param([string]$Prompt, [int]$TimeoutSeconds = 60)
    if (-not (Test-InstallerInteractiveInput)) { return $null }
    if ($TimeoutSeconds -le 0) { $TimeoutSeconds = 60 }
    try {
        # Callers only reach this after Test-InstallerInteractiveInput, but a
        # host/test harness can mock that probe while redirecting the console.
        # Never fall back to Read-Host: it has no deadline, so an unanswered
        # prompt would block forever, and a caller that maps an empty result to
        # the default answer would then accept on the user's behalf. Report an
        # explicit "no answer" sentinel instead.
        if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) {
            return '__unavailable__'
        }
        [Console]::Write($Prompt)
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        $builder = New-Object System.Text.StringBuilder
        while ([DateTime]::UtcNow -lt $deadline) {
            if (-not [Console]::KeyAvailable) {
                Start-Sleep -Milliseconds 100
                continue
            }
            $key = [Console]::ReadKey($true)
            if ($key.Key -eq [ConsoleKey]::Enter) {
                [Console]::WriteLine()
                return $builder.ToString()
            }
            if ($key.Key -eq [ConsoleKey]::Backspace) {
                if ($builder.Length -gt 0) {
                    $builder.Length = $builder.Length - 1
                    [Console]::Write("`b `b")
                }
                continue
            }
            if ($key.KeyChar -and -not [char]::IsControl($key.KeyChar)) {
                [void]$builder.Append($key.KeyChar)
                [Console]::Write($key.KeyChar)
            }
        }
        [Console]::WriteLine()
        return '__timeout__'
    } catch {
        # ISE and other non-console hosts cannot poll keys. Return the sentinel
        # so callers refuse instead of degrading to an unbounded Read-Host.
        return '__unavailable__'
    }
}

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
                $answer = Read-HostWithTimeout -Prompt "Add $dir to your user PATH now? [Y/n] " -TimeoutSeconds $PromptTimeout
            } catch {
                $answer = '__unavailable__'
            }
            if ($null -eq $answer -or $answer -eq '__unavailable__') {
                Write-Warning 'could not read an answer for the PATH prompt; leaving the user PATH unchanged'
                Write-ManualPathNote $dir
                return
            }
            if ($answer -eq '__timeout__') {
                Write-Warning "PATH prompt timed out after ${PromptTimeout}s; leaving the user PATH unchanged"
                Write-ManualPathNote $dir
                return
            }
            $answer = ([string]$answer).Trim()
            if (-not $answer -or $answer -notmatch '^(n|no)\z') {
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

# Commit the staged binary and manifest as one transaction. Both stages live in
# the destination directory, so each replace is a same-volume atomic rename.
# The old files are copied to same-directory backups first; if the second
# replace fails, both old files are restored. A first install removes the new
# binary when the manifest replace fails. On a failed restore the backups are
# kept and named in the error so the user can recover manually.
function Commit-InstallFiles {
    param(
        [string]$StageBin,
        [string]$InstallPath,
        [string]$StageManifest,
        [string]$ManifestPath
    )
    $backupBin = "$InstallPath.backup-$PID"
    $backupManifest = "$ManifestPath.backup-$PID"
    $hadBin = Test-Path -LiteralPath $InstallPath -PathType Leaf
    $hadManifest = Test-Path -LiteralPath $ManifestPath -PathType Leaf
    $binMoved = $false
    $manifestMoved = $false
    Remove-Item -LiteralPath $backupBin, $backupManifest -Force -ErrorAction SilentlyContinue

    try {
        # File.Replace writes the previous destination to the third argument,
        # so the backup is created by the same atomic operation that installs
        # the new file. The null overload is not portable (it rejects an empty
        # path on .NET Core/Linux).
        if ($hadBin) {
            [IO.File]::Replace($StageBin, $InstallPath, $backupBin)
        } else {
            [IO.File]::Move($StageBin, $InstallPath)
        }
        $binMoved = $true

        if ($hadManifest) {
            [IO.File]::Replace($StageManifest, $ManifestPath, $backupManifest)
        } else {
            [IO.File]::Move($StageManifest, $ManifestPath)
        }
        $manifestMoved = $true
    } catch {
        $original = $_.Exception.Message
        $rollbackError = $null
        try {
            if ($hadBin) {
                # If Replace failed before swapping the destination, the old
                # file is still in place and there is no backup to restore.
                if (Test-Path -LiteralPath $backupBin -PathType Leaf) {
                    Move-Item -LiteralPath $backupBin -Destination $InstallPath -Force -ErrorAction Stop
                } elseif (-not (Test-Path -LiteralPath $InstallPath -PathType Leaf)) {
                    throw "the old binary and its backup are both missing"
                }
            } elseif ($binMoved -or (Test-Path -LiteralPath $InstallPath -PathType Leaf)) {
                Remove-Item -LiteralPath $InstallPath -Force -ErrorAction Stop
            }
            if ($hadManifest) {
                if (Test-Path -LiteralPath $backupManifest -PathType Leaf) {
                    Move-Item -LiteralPath $backupManifest -Destination $ManifestPath -Force -ErrorAction Stop
                } elseif (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
                    throw "the old manifest and its backup are both missing"
                }
            } elseif ($manifestMoved -or (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
                Remove-Item -LiteralPath $ManifestPath -Force -ErrorAction Stop
            }
        } catch {
            $rollbackError = $_.Exception.Message
        }
        if ($null -ne $rollbackError) {
            throw "install replace failed ($original); automatic rollback also failed ($rollbackError). Old files are preserved at $backupBin and $backupManifest"
        }
        Remove-Item -LiteralPath $backupBin, $backupManifest, $StageBin, $StageManifest -Force -ErrorAction SilentlyContinue
        throw "install replace failed ($original); the previous binary and manifest were restored"
    }

    Remove-Item -LiteralPath $backupBin, $backupManifest -Force -ErrorAction SilentlyContinue
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

# Numeric identifiers are compared by length and then ordinal order, so a
# semver component is never cast to a fixed-width integer (a 20-digit major
# would otherwise throw instead of comparing). Leading zeros are rejected by
# the parser, so length order is numeric order here.
function Compare-SemverNumericIdentifier {
    param([string]$Left, [string]$Right)
    if ($Left.Length -lt $Right.Length) { return -1 }
    if ($Left.Length -gt $Right.Length) { return 1 }
    return [string]::CompareOrdinal($Left, $Right)
}

# Parse and fully validate one semver value before any ordering decision.
# Returns $null for anything that is not a complete SemVer 2.0.0 string
# (including an empty prerelease/build identifier or a numeric prerelease
# identifier with a leading zero). Build metadata is validated but ignored
# for ordering.
function ConvertTo-SemverParts {
    param([string]$Value)
    if ([string]::IsNullOrEmpty($Value)) { return $null }
    # \z (not $) so a trailing LF cannot satisfy the anchor; the explicit
    # length check keeps the contract even if the pattern is edited later.
    $pattern = '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?\z'
    $match = [regex]::Match($Value, $pattern)
    if (-not $match.Success -or $match.Length -ne $Value.Length) { return $null }

    $pre = @()
    if ($match.Groups[4].Success) {
        $pre = @($match.Groups[4].Value.Split('.'))
        if ($pre.Count -eq 0) { return $null }
        foreach ($id in $pre) {
            if ([string]::IsNullOrEmpty($id)) { return $null }
            if ($id -notmatch '^[0-9A-Za-z-]+$') { return $null }
            if ($id -match '^[0-9]+$' -and $id.Length -gt 1 -and $id.StartsWith('0')) { return $null }
        }
    }

    if ($match.Groups[5].Success) {
        $build = @($match.Groups[5].Value.Split('.'))
        if ($build.Count -eq 0) { return $null }
        foreach ($id in $build) {
            if ([string]::IsNullOrEmpty($id)) { return $null }
            if ($id -notmatch '^[0-9A-Za-z-]+$') { return $null }
        }
    }

    return @{
        Major = $match.Groups[1].Value
        Minor = $match.Groups[2].Value
        Patch = $match.Groups[3].Value
        Pre   = $pre
    }
}

function Compare-SemverVersion {
    param([string]$Left, [string]$Right)
    # Both sides must be valid before either can be declared lower/higher.
    # The previous implementation returned 'lt'/'gt' for a stable-vs-prerelease
    # pair before validating the prerelease identifiers, so '0.1.0-01' was
    # treated as a compatible lower version instead of invalid.
    $l = ConvertTo-SemverParts -Value $Left
    if ($null -eq $l) { return 'invalid' }
    $r = ConvertTo-SemverParts -Value $Right
    if ($null -eq $r) { return 'invalid' }

    foreach ($field in @('Major', 'Minor', 'Patch')) {
        $cmp = Compare-SemverNumericIdentifier -Left $l[$field] -Right $r[$field]
        if ($cmp -lt 0) { return 'lt' }
        if ($cmp -gt 0) { return 'gt' }
    }

    $leftIds = @($l.Pre)
    $rightIds = @($r.Pre)
    if ($leftIds.Count -eq 0 -and $rightIds.Count -eq 0) { return 'eq' }
    if ($leftIds.Count -eq 0) { return 'gt' }
    if ($rightIds.Count -eq 0) { return 'lt' }

    $count = [Math]::Max($leftIds.Count, $rightIds.Count)
    for ($i = 0; $i -lt $count; $i++) {
        if ($i -ge $leftIds.Count) { return 'lt' }
        if ($i -ge $rightIds.Count) { return 'gt' }
        $a = $leftIds[$i]
        $b = $rightIds[$i]
        $aNumeric = $a -match '^[0-9]+$'
        $bNumeric = $b -match '^[0-9]+$'
        if ($aNumeric -and $bNumeric) {
            $cmp = Compare-SemverNumericIdentifier -Left $a -Right $b
            if ($cmp -lt 0) { return 'lt' }
            if ($cmp -gt 0) { return 'gt' }
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

# Minimal JSON tokenizer/parser for the flat profile index. It deliberately
# does not use ConvertFrom-Json: PowerShell 5.1 has no duplicate-key mode, and
# ConvertFrom-Json coerces `"schemaVersion": "1"` to an integer. This parser
# keeps JSON types explicit, rejects duplicate keys at every object level, and
# is then validated against the flat schema.
function Test-JsonDigit {
    param([char]$Ch)
    $code = [int]$Ch
    return (($code -ge 48) -and ($code -le 57))
}

function Skip-JsonWhitespace {
    param($State)
    while ($State.Pos -lt $State.Length) {
        $code = [int]$State.Text[$State.Pos]
        if ($code -eq 32 -or $code -eq 9 -or $code -eq 10 -or $code -eq 13) {
            $State.Pos++
        } else {
            break
        }
    }
}

function Expect-JsonChar {
    param($State, [char]$Expected)
    Skip-JsonWhitespace -State $State
    if ($State.Pos -ge $State.Length -or $State.Text[$State.Pos] -ne $Expected) {
        throw "expected '$Expected' at offset $($State.Pos)"
    }
    $State.Pos++
}

function Read-JsonString {
    param($State)
    Skip-JsonWhitespace -State $State
    if ($State.Pos -ge $State.Length -or $State.Text[$State.Pos] -ne '"') {
        throw "expected a string at offset $($State.Pos)"
    }
    $State.Pos++
    $builder = New-Object System.Text.StringBuilder
    while ($State.Pos -lt $State.Length) {
        $ch = $State.Text[$State.Pos]
        $State.Pos++
        if ($ch -eq '"') { return $builder.ToString() }
        if ([int]$ch -lt 0x20) { throw "control character in a string at offset $($State.Pos)" }
        if ([int]$ch -ne 92) {
            [void]$builder.Append($ch)
            continue
        }
        if ($State.Pos -ge $State.Length) { throw "unterminated escape at offset $($State.Pos)" }
        $esc = [string]$State.Text[$State.Pos]
        $State.Pos++
        switch -CaseSensitive ($esc) {
            '"' { [void]$builder.Append('"') }
            '\' { [void]$builder.Append('\') }
            '/' { [void]$builder.Append('/') }
            'b' { [void]$builder.Append([char]0x08) }
            'f' { [void]$builder.Append([char]0x0C) }
            'n' { [void]$builder.Append([char]0x0A) }
            'r' { [void]$builder.Append([char]0x0D) }
            't' { [void]$builder.Append([char]0x09) }
            'u' {
                if ($State.Pos + 4 -gt $State.Length) { throw "short unicode escape at offset $($State.Pos)" }
                $hex = $State.Text.Substring($State.Pos, 4)
                $State.Pos += 4
                if ($hex -notmatch '^[0-9A-Fa-f]{4}$') { throw "invalid unicode escape at offset $($State.Pos)" }
                $code = [Convert]::ToInt32($hex, 16)
                if ($code -gt 0x7F) {
                    throw "non-ASCII unicode escape is not allowed in the profile index"
                }
                [void]$builder.Append([char]$code)
            }
            default { throw "invalid escape '\\$esc' at offset $($State.Pos)" }
        }
    }
    throw "unterminated string at offset $($State.Pos)"
}

function Read-JsonNumber {
    param($State)
    Skip-JsonWhitespace -State $State
    $start = $State.Pos
    if ($State.Pos -lt $State.Length -and $State.Text[$State.Pos] -eq '-') { $State.Pos++ }
    if ($State.Pos -ge $State.Length -or -not (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
        throw "invalid number at offset $start"
    }
    if ($State.Text[$State.Pos] -eq '0') {
        $State.Pos++
        if ($State.Pos -lt $State.Length -and (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            throw "leading zero in a number at offset $start"
        }
    } else {
        while ($State.Pos -lt $State.Length -and (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            $State.Pos++
        }
    }
    if ($State.Pos -lt $State.Length -and $State.Text[$State.Pos] -eq '.') {
        $State.Pos++
        if ($State.Pos -ge $State.Length -or -not (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            throw "invalid fraction at offset $start"
        }
        while ($State.Pos -lt $State.Length -and (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            $State.Pos++
        }
    }
    if ($State.Pos -lt $State.Length -and
        ($State.Text[$State.Pos] -eq 'e' -or $State.Text[$State.Pos] -eq 'E')) {
        $State.Pos++
        if ($State.Pos -lt $State.Length -and
            ($State.Text[$State.Pos] -eq '+' -or $State.Text[$State.Pos] -eq '-')) {
            $State.Pos++
        }
        if ($State.Pos -ge $State.Length -or -not (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            throw "invalid exponent at offset $start"
        }
        while ($State.Pos -lt $State.Length -and (Test-JsonDigit -Ch $State.Text[$State.Pos])) {
            $State.Pos++
        }
    }
    return $State.Text.Substring($start, $State.Pos - $start)
}

function Read-JsonLiteral {
    param($State, [string]$Literal, $Value)
    Skip-JsonWhitespace -State $State
    if ($State.Pos + $Literal.Length -gt $State.Length -or
        $State.Text.Substring($State.Pos, $Literal.Length) -cne $Literal) {
        throw "invalid JSON literal at offset $($State.Pos)"
    }
    $State.Pos += $Literal.Length
    return $Value
}

function Read-JsonObject {
    param($State, [int]$Depth)
    Expect-JsonChar -State $State -Expected '{'
    $object = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    Skip-JsonWhitespace -State $State
    if ($State.Pos -lt $State.Length -and $State.Text[$State.Pos] -eq '}') {
        $State.Pos++
        return $object
    }
    while ($true) {
        $key = Read-JsonString -State $State
        if ($object.ContainsKey($key)) { throw "duplicate key '$key' at offset $($State.Pos)" }
        Expect-JsonChar -State $State -Expected ':'
        $value = Read-JsonValue -State $State -Depth ($Depth + 1)
        $object[$key] = $value
        Skip-JsonWhitespace -State $State
        if ($State.Pos -ge $State.Length) { throw "unterminated object at offset $($State.Pos)" }
        if ($State.Text[$State.Pos] -eq ',') {
            $State.Pos++
            continue
        }
        if ($State.Text[$State.Pos] -eq '}') {
            $State.Pos++
            return $object
        }
        throw "expected ',' or '}' at offset $($State.Pos)"
    }
}

function Read-JsonValue {
    param($State, [int]$Depth)
    if ($Depth -gt 8) { throw "JSON nesting is too deep" }
    Skip-JsonWhitespace -State $State
    if ($State.Pos -ge $State.Length) { throw "unexpected end of JSON" }
    $ch = $State.Text[$State.Pos]
    if ($ch -eq '{') { return Read-JsonObject -State $State -Depth $Depth }
    if ($ch -eq '[') { throw "arrays are not allowed in the profile index" }
    if ($ch -eq '"') {
        return [pscustomobject]@{ Kind = 'string'; Value = (Read-JsonString -State $State) }
    }
    if ($ch -eq 't') { return Read-JsonLiteral -State $State -Literal 'true' -Value ([pscustomobject]@{ Kind = 'bool'; Value = $true }) }
    if ($ch -eq 'f') { return Read-JsonLiteral -State $State -Literal 'false' -Value ([pscustomobject]@{ Kind = 'bool'; Value = $false }) }
    if ($ch -eq 'n') { return Read-JsonLiteral -State $State -Literal 'null' -Value ([pscustomobject]@{ Kind = 'null'; Value = $null }) }
    if ($ch -eq '-' -or (Test-JsonDigit -Ch $ch)) {
        return [pscustomobject]@{ Kind = 'number'; Raw = (Read-JsonNumber -State $State) }
    }
    throw "unexpected character at offset $($State.Pos)"
}

# Validate and read the flat rpi-pages profile index
# (install-packaging.md section 3). The parser above rejects duplicate keys at
# every object level; this function enforces the exact flat schema and JSON
# types (schemaVersion must be the number 1, not the string "1").
function ConvertFrom-ProfileIndex {
    param([string]$Text, [string]$Version, [string]$AssetName)
    $result = [pscustomobject]@{ Ok = $false; Sha = $null; Error = '' }
    if ([string]::IsNullOrWhiteSpace($Text)) {
        $result.Error = 'empty profile index'
        return $result
    }
    try {
        $byteCount = [System.Text.Encoding]::UTF8.GetByteCount($Text)
    } catch {
        $result.Error = 'profile index is not valid UTF-8'
        return $result
    }
    if ($byteCount -gt 262144) {
        $result.Error = 'profile index is too large'
        return $result
    }

    $state = [pscustomobject]@{ Text = $Text; Pos = 0; Length = $Text.Length }
    try {
        Skip-JsonWhitespace -State $state
        $root = Read-JsonValue -State $state -Depth 0
        Skip-JsonWhitespace -State $state
        if ($state.Pos -ne $state.Length) { throw "trailing data at offset $($state.Pos)" }
    } catch {
        $result.Error = "profile index is not valid JSON: $($_.Exception.Message)"
        return $result
    }

    if (-not ($root -is [System.Collections.Generic.Dictionary[string,object]])) {
        $result.Error = 'profile index must be a JSON object'
        return $result
    }
    foreach ($key in $root.Keys) {
        if ($key -cne 'schemaVersion' -and $key -cne 'version' -and
            $key -cne 'assets' -and $key -cne 'sizes') {
            $result.Error = "unexpected profile index key: $key"
            return $result
        }
    }
    if (-not $root.ContainsKey('schemaVersion')) {
        $result.Error = 'profile index is missing schemaVersion'
        return $result
    }
    $schema = $root['schemaVersion']
    if (-not ($schema -is [pscustomobject]) -or $schema.Kind -ne 'number' -or $schema.Raw -cne '1') {
        $result.Error = 'unsupported profile index schemaVersion: expected numeric 1'
        return $result
    }
    if (-not $root.ContainsKey('version')) {
        $result.Error = 'profile index is missing version'
        return $result
    }
    $indexVersion = $root['version']
    if (-not ($indexVersion -is [pscustomobject]) -or $indexVersion.Kind -ne 'string') {
        $result.Error = 'profile index version must be a JSON string'
        return $result
    }
    if ($indexVersion.Value -cne $Version) {
        $result.Error = "profile index version mismatch: index=$($indexVersion.Value) expected=$Version"
        return $result
    }
    if (-not $root.ContainsKey('assets')) {
        $result.Error = 'profile index has no assets object'
        return $result
    }
    $assets = $root['assets']
    if (-not ($assets -is [System.Collections.Generic.Dictionary[string,object]])) {
        $result.Error = 'profile index assets must be a flat JSON object'
        return $result
    }
    $targetSha = $null
    foreach ($asset in $assets.Keys) {
        if ($asset -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]*\z') {
            $result.Error = "profile index has an invalid asset name: $asset"
            return $result
        }
        $value = $assets[$asset]
        if (-not ($value -is [pscustomobject]) -or $value.Kind -ne 'string') {
            $result.Error = "profile index entry for $asset is not a JSON string"
            return $result
        }
        if ($value.Value -cnotmatch '^[0-9a-fA-F]{64}\z') {
            $result.Error = "profile index entry for $asset is not a 64-hex sha256"
            return $result
        }
        if ($asset -ceq $AssetName) { $targetSha = $value.Value.ToLowerInvariant() }
    }
    if ($null -eq $targetSha) {
        $result.Error = "asset entry missing from the profile index: $AssetName"
        return $result
    }
    if ($root.ContainsKey('sizes')) {
        $sizes = $root['sizes']
        if (-not ($sizes -is [System.Collections.Generic.Dictionary[string,object]])) {
            $result.Error = 'profile index sizes must be a flat JSON object'
            return $result
        }
        foreach ($asset in $sizes.Keys) {
            if (-not $assets.ContainsKey($asset)) {
                $result.Error = "profile index sizes has an entry for an unknown asset: $asset"
                return $result
            }
            $value = $sizes[$asset]
            if (-not ($value -is [pscustomobject]) -or $value.Kind -ne 'number' -or
                $value.Raw -cnotmatch '^(0|[1-9][0-9]*)\z') {
                $result.Error = "profile index size for $asset is not a non-negative integer"
                return $result
            }
        }
    }
    $result.Ok = $true
    $result.Sha = $targetSha
    return $result
}

# Persist the response validators next to the partial file. Written as soon as
# a transfer has a status/validator (and again on success) so an interrupted
# transfer or a mirror switch can resume with Range + If-Range.
function Write-AssetMetaFile {
    param(
        [string]$Path,
        [string]$Sha,
        [string]$Asset,
        [string]$Version,
        [string]$ETag,
        [string]$LastModified,
        [string]$Url
    )
    if (-not $ETag) { $ETag = '' }
    if (-not $LastModified) { $LastModified = '' }
    @(
        "sha=$Sha"
        "asset=$Asset"
        "version=$Version"
        "etag=$ETag"
        "last_modified=$LastModified"
        "url=$Url"
    ) | Set-Content -LiteralPath $Path -Encoding ASCII
}

# 206/200/416 decision for a partial file (install-packaging.md section 7).
function Get-RangeResumeDecision {
    param([int]$Status, [string]$CachedValidator, [string]$ResponseValidator, [bool]$HasPartial)
    if (-not $HasPartial) { return 'fresh' }
    if ($Status -eq 200) { return 'restart' }
    if ($Status -eq 416) { return 'verify-or-restart' }
    if ($Status -eq 206) {
        # A 206 may only be appended when this response carries a validator
        # and it equals the validator the partial was requested with. A
        # missing validator is not proof that the bytes belong to the same
        # entity, so it takes the same restart path as a changed one.
        if ($CachedValidator -and $ResponseValidator -and ($CachedValidator -eq $ResponseValidator)) {
            return 'append'
        }
        return 'restart'
    }
    return 'retry'
}

function Invoke-MetadataRequest {
    param(
        [string]$Url,
        [int]$TimeoutSeconds = 30,
        [int]$MaxBytes = 262144
    )
    if ($TimeoutSeconds -le 0) { $TimeoutSeconds = 30 }
    if ($MaxBytes -le 0) { $MaxBytes = 262144 }
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.UserAgent = 'rpi-installer'
    $request.Timeout = $TimeoutSeconds * 1000
    $request.ReadWriteTimeout = $TimeoutSeconds * 1000
    $response = $null
    $stream = $null
    $memory = $null
    try {
        $response = $request.GetResponse()
        if ($response.ContentLength -gt $MaxBytes) {
            throw "response is too large ($($response.ContentLength) bytes; limit $MaxBytes)"
        }
        $stream = $response.GetResponseStream()
        $memory = New-Object System.IO.MemoryStream
        $buffer = New-Object byte[] 16384
        $total = 0
        while ($true) {
            $read = $stream.Read($buffer, 0, $buffer.Length)
            if ($read -le 0) { break }
            $total += $read
            if ($total -gt $MaxBytes) {
                throw "response is too large (> $MaxBytes bytes)"
            }
            $memory.Write($buffer, 0, $read)
        }
        $bytes = $memory.ToArray()
        # The metadata is UTF-8 JSON; reject invalid byte sequences instead of
        # silently replacing them (the strict parser would fail later anyway).
        $utf8 = New-Object System.Text.UTF8Encoding -ArgumentList $false, $true
        try {
            return $utf8.GetString($bytes)
        } catch {
            throw 'response is not valid UTF-8'
        }
    } finally {
        if ($memory) { $memory.Dispose() }
        if ($stream) { $stream.Dispose() }
        if ($response) { $response.Close() }
    }
}

# Parse Retry-After as delta-seconds or an HTTP-date (RFC 7231). Returns $null
# when absent/unparseable; values are capped at 60s per install-packaging.md.
function Get-RetryAfterSeconds {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return $null }
    $seconds = 0
    if ([int]::TryParse($Value.Trim(), [ref]$seconds)) {
        if ($seconds -lt 0) { return 0 }
        return [Math]::Min(60, $seconds)
    }
    $when = [DateTimeOffset]::MinValue
    $styles = [Globalization.DateTimeStyles]::AssumeUniversal -bor
        [Globalization.DateTimeStyles]::AllowWhiteSpaces
    if ([DateTimeOffset]::TryParse($Value.Trim(), [Globalization.CultureInfo]::InvariantCulture, $styles, [ref]$when)) {
        $delta = ($when - [DateTimeOffset]::UtcNow).TotalSeconds
        if ($delta -lt 0) { return 0 }
        return [Math]::Min(60, [int][Math]::Ceiling($delta))
    }
    return $null
}

# Retry-After wins when present; otherwise exponential backoff (1/2/4/8s) with
# the contract's +/-20% jitter.
function Get-RetryDelaySeconds {
    param([int]$Attempt, [string]$RetryAfter)
    $retry = Get-RetryAfterSeconds -Value $RetryAfter
    if ($null -ne $retry) { return [double]$retry }
    $base = [Math]::Min(8.0, [Math]::Pow(2, [Math]::Max(0, $Attempt - 1)))
    $jitter = 0.8 + ((Get-Random -Minimum 0 -Maximum 1000) / 2500.0)
    return [Math]::Round($base * $jitter, 2)
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
        [int]$ConnectTimeout = 10,
        [string]$LockPath,
        [switch]$NoProgress
    )
    if ($ConnectTimeout -le 0) { $ConnectTimeout = 10 }
    if ($StallTimeout -le 0) { $StallTimeout = 30 }
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.UserAgent = 'rpi-installer'
    $request.AllowAutoRedirect = $true
    # Timeout bounds DNS/connect/response headers; the body is bounded by
    # ReadWriteTimeout (the stall timeout) and has no total timeout.
    $request.Timeout = $ConnectTimeout * 1000
    $request.ReadWriteTimeout = $StallTimeout * 1000
    if ($Offset -gt 0 -and $Validator) {
        $request.AddRange($Offset)
        $request.Headers['If-Range'] = $Validator
    }
    $script:LastTransferValidators = $null
    $response = $null
    try {
        try {
            $response = $request.GetResponse()
        } catch [System.Net.WebException] {
            $webResponse = $_.Exception.Response
            if (-not $webResponse) { throw }
            $status = [int]$webResponse.StatusCode
            $retryAfter = ''
            try { $retryAfter = [string]$webResponse.Headers['Retry-After'] } catch { }
            $webResponse.Close()
            return [pscustomobject]@{ Status = $status; ETag = $null; LastModified = $null; Truncated = $false; RetryAfter = $retryAfter }
        }
        $status = [int]$response.StatusCode
        $etag = [string]$response.Headers['ETag']
        $lastModified = [string]$response.Headers['Last-Modified']
        $responseValidator = if ($etag) { $etag } else { $lastModified }
        $appendAllowed = $true
        if ($status -eq 206 -and $Offset -gt 0) {
            if (-not $Validator -or -not $responseValidator -or ($Validator -ne $responseValidator)) {
                $appendAllowed = $false
            }
        }
        if ($appendAllowed -and (($status -eq 200 -and $Offset -eq 0) -or $status -eq 206) -and
            ($etag -or $lastModified)) {
            $script:LastTransferValidators = [pscustomobject]@{
                ETag = $etag
                LastModified = $lastModified
            }
        }
        if ($status -eq 200 -and $Offset -gt 0) {
            return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $false; RetryAfter = '' }
        }
        if ($status -eq 416) {
            return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $false; RetryAfter = '' }
        }
        if (-not $appendAllowed) {
            # Do not consume the body: the caller will discard the partial and
            # restart from zero. This also keeps a truncated/mixed partial from
            # being persisted with the new response validator.
            return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $false; RetryAfter = '' }
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
        return [pscustomobject]@{ Status = $status; ETag = $etag; LastModified = $lastModified; Truncated = $truncated; RetryAfter = '' }
    } finally {
        if ($response) { $response.Close() }
    }
}

# Seven-day cache GC (install-packaging.md section 7). An active transfer holds
# a fresh .lock; anything with that sibling is skipped. A missing part simply
# re-downloads, so deleting old bytes is safe.
function Invoke-CacheGc {
    param([string]$PartsDir, [switch]$Disabled)
    if ($Disabled) { return }
    if (-not (Test-Path -LiteralPath $PartsDir -PathType Container)) { return }
    $cutoff = [DateTime]::UtcNow.AddDays(-7)
    foreach ($file in @(Get-ChildItem -LiteralPath $PartsDir -File -ErrorAction SilentlyContinue)) {
        if ($file.Name.EndsWith('.lock')) { continue }
        # The active-transfer lock is <asset>.lock, not <asset>.meta.lock, so an
        # auxiliary file must probe the lock of the asset it belongs to.
        $lockProbe = $file.FullName
        if ($file.Name.EndsWith('.meta')) {
            $lockProbe = $lockProbe.Substring(0, $lockProbe.Length - 5)
        }
        if (Test-Path -LiteralPath ($lockProbe + '.lock')) { continue }
        if ($file.LastWriteTimeUtc -lt $cutoff) {
            Remove-Item -LiteralPath $file.FullName -Force -ErrorAction SilentlyContinue
        }
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
        [int]$StallTimeout = 30,
        [int]$ConnectTimeout = 10
    )
    if ($MaxAttempts -le 0) { $MaxAttempts = 5 }
    if ($StallTimeout -le 0) { $StallTimeout = 30 }
    if ($ConnectTimeout -le 0) { $ConnectTimeout = 10 }
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
                        -Validator $validator -StallTimeout $StallTimeout -ConnectTimeout $ConnectTimeout -LockPath $lock
                } catch {
                    Write-Warning "download failed from ${url}: $($_.Exception.Message)"
                    # .NET raises on a premature close when Content-Length is
                    # known; keep the bytes and the validators so the retry can
                    # resume with Range + If-Range instead of starting over.
                    if ((Test-Path -LiteralPath $part) -and (Get-Item -LiteralPath $part).Length -gt 0 -and
                        $script:LastTransferValidators) {
                        Write-AssetMetaFile -Path $meta -Sha $ExpectedSha -Asset $Asset -Version $Version `
                            -ETag $script:LastTransferValidators.ETag `
                            -LastModified $script:LastTransferValidators.LastModified -Url $url
                    }
                    if ($attempt -ge $MaxAttempts) { break }
                    $delay = Get-RetryDelaySeconds -Attempt $attempt -RetryAfter ''
                    Write-Warning "download failed from ${url}; retrying in $($delay.ToString('0.00', [System.Globalization.CultureInfo]::InvariantCulture))s"
                    Start-Sleep -Seconds $delay
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
                        $delay = Get-RetryDelaySeconds -Attempt $attempt -RetryAfter $transfer.RetryAfter
                        Write-Warning "download failed from ${url}; retrying in $($delay.ToString('0.00', [System.Globalization.CultureInfo]::InvariantCulture))s"
                        Start-Sleep -Seconds $delay
                        continue
                    }
                }
                if ($transfer.Truncated) {
                    # The decision above already guaranteed a fresh 200 or a
                    # 206 with the same validator, so these bytes are a
                    # resumable prefix and may be persisted.
                    if (Test-Path -LiteralPath $part) {
                        Write-AssetMetaFile -Path $meta -Sha $ExpectedSha -Asset $Asset -Version $Version `
                            -ETag $transfer.ETag -LastModified $transfer.LastModified -Url $url
                    }
                    if ($attempt -ge $MaxAttempts) {
                        Write-Warning "download truncated from $url after $attempt attempts; trying the next mirror"
                        break
                    }
                    # Same jittered 1/2/4/8s schedule as every other retry; a
                    # flat pause would defeat the backoff contract.
                    $delay = Get-RetryDelaySeconds -Attempt $attempt -RetryAfter ''
                    Write-Warning "download truncated; retrying in $($delay.ToString('0.00', [System.Globalization.CultureInfo]::InvariantCulture))s with a resume request"
                    Start-Sleep -Seconds $delay
                    continue
                    continue
                }
                if ($transfer.Status -eq 206 -or $transfer.Status -eq 200) {
                    if (-not (Test-Path -LiteralPath $part) -or (Get-Item -LiteralPath $part).Length -eq 0) {
                        Write-Warning "empty download from $url"
                        continue
                    }
                    Write-AssetMetaFile -Path $meta -Sha $ExpectedSha -Asset $Asset -Version $Version `
                        -ETag $transfer.ETag -LastModified $transfer.LastModified -Url $url
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
                Start-Sleep -Seconds (Get-RetryDelaySeconds -Attempt $attempt -RetryAfter $transfer.RetryAfter)
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
        if (-not $seen.Add($name)) { return $false }
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

# Seed settings.json from the bundle's settings.defaults.json. This is a
# convenience after a successful binary/plugin install: any directory,
# permission or lock error is a warning and returns $false, never a fatal
# installer exit. The caller still reports the successful install.
function Seed-DefaultSettings {
    param(
        [string]$DefaultsPath,
        [string]$SettingsPath,
        [string]$AgentDir
    )
    $stageSettings = "$SettingsPath.stage-$PID"
    try {
        New-Item -ItemType Directory -Force -Path $AgentDir -ErrorAction Stop | Out-Null
        Copy-Item -LiteralPath $DefaultsPath -Destination $stageSettings -Force -ErrorAction Stop
        Move-Item -LiteralPath $stageSettings -Destination $SettingsPath -Force -ErrorAction Stop
        return $true
    } catch {
        Write-Warning "could not seed default settings at ${SettingsPath}: $($_.Exception.Message)"
        Write-Warning 'the binary and plugins are installed; create settings.json manually or rerun the installer'
        return $false
    } finally {
        # Cleanup must never throw: the caller's contract is "seeding is only a
        # warning". Test-Path throws under $ErrorActionPreference='Stop' when
        # the stage parent is itself inaccessible, so silence both calls.
        try {
            if (Test-Path -LiteralPath $stageSettings -ErrorAction SilentlyContinue) {
                Remove-Item -LiteralPath $stageSettings -Force -ErrorAction SilentlyContinue
            }
        } catch {
        }
    }
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

function Resolve-InstallVersion {
    param(
        [string]$Channel,
        [string]$GitHubApiUrl,
        [string]$VersionCheckUrl,
        [string]$Repo
    )

    if ($Channel -eq 'rc') {
        try {
            $infoDoc = Invoke-RestMethod -Uri $VersionCheckUrl
        } catch {
            Fail "RC version endpoint $VersionCheckUrl is unreachable: $($_.Exception.Message)"
        }
        $Version = ([string]$infoDoc.version) -replace '^v', ''
        if (-not $Version) { Fail "RC version endpoint $VersionCheckUrl did not return a version" }
        $ReleaseAssets = @()
        $releaseApi = if ($GitHubApiUrl) {
            $GitHubApiUrl
        } else {
            "https://api.github.com/repos/$Repo/releases/tags/v$Version"
        }
        try {
            $release = Invoke-RestMethod -Uri $releaseApi -Headers @{ 'User-Agent' = 'rpi-installer' }
            $releaseVersion = ([string]$release.tag_name) -replace '^v', ''
            if ($releaseVersion -eq $Version) {
                $ReleaseAssets = @($release.assets)
            } else {
                Write-Warning "GitHub release API returned v$releaseVersion for RC v$Version; using naming-rule URLs"
            }
        } catch {
            Write-Warning 'GitHub release API unavailable; using naming-rule download URLs'
        }
        return [pscustomobject]@{
            Version = $Version
            ReleaseAssets = $ReleaseAssets
            ResolvedVia = 'RC version endpoint'
        }
    }

    $api = if ($GitHubApiUrl) { $GitHubApiUrl } else { "https://api.github.com/repos/$Repo/releases/latest" }
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'rpi-installer' }
        $Version = ([string]$release.tag_name) -replace '^v', ''
        if ($Version) {
            return [pscustomobject]@{
                Version = $Version
                ReleaseAssets = @($release.assets)
                ResolvedVia = 'GitHub API'
            }
        }
    } catch { }

    Write-Warning 'GitHub API unavailable; falling back to version endpoint'
    try {
        $infoDoc = Invoke-RestMethod -Uri $VersionCheckUrl
    } catch {
        Fail "version endpoint $VersionCheckUrl is unreachable: $($_.Exception.Message)"
    }
    $Version = ([string]$infoDoc.version) -replace '^v', ''
    if (-not $Version) { Fail "version endpoint $VersionCheckUrl did not return a version" }
    return [pscustomobject]@{
        Version = $Version
        ReleaseAssets = @()
        ResolvedVia = 'version endpoint'
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

$resolution = Resolve-InstallVersion -Channel $InstallChannel -GitHubApiUrl $GitHubApiUrl -VersionCheckUrl $VersionCheckUrl -Repo $Repo
$Version = $resolution.Version
$ReleaseAssets = @($resolution.ReleaseAssets)
Write-Host "latest release: v$Version (via $($resolution.ResolvedVia))"

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
        $answer = Read-HostWithTimeout -Prompt "Install these native plugins with profile '$ProfileValue'? [y/N] " -TimeoutSeconds $PromptTimeout
        if ($answer -eq '__timeout__') {
            $Consent = $false
            $ProfileSource = 'timeout'
            Write-Warning "profile prompt timed out after ${PromptTimeout}s; native plugins were not consented"
        } elseif ($null -eq $answer -or $answer -eq '__unavailable__') {
            $Consent = $false
            $ProfileSource = 'unavailable'
            Write-Warning 'could not read an answer for the native plugin prompt; native plugins were not consented'
        } else {
            $Consent = [bool]($answer -and $answer.Trim() -match '^(y|yes)\z')
        }
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
            $choice = Read-HostWithTimeout -Prompt 'Profile [1/2/3, Enter = standard] ' -TimeoutSeconds $PromptTimeout
            if ($null -eq $choice -or $choice -eq '__unavailable__') { $choice = '__timeout__' }
            if ($choice -eq '__timeout__') {
                $ProfileValue = 'standard'
                $Consent = $false
                $ProfileSource = 'timeout'
                Write-Warning "profile prompt timed out after ${PromptTimeout}s; native plugins were not consented"
                $validChoice = $true
                continue
            }
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
# Staging files live in the install prefix, not in TmpDir. Track them so every
# failure path (Fail calls exit, which still runs this finally) removes them
# instead of leaving .rpi.stage-<pid> residue next to the installed binary.
$stageBin = $null
$stageManifest = $null

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
    Invoke-CacheGc -PartsDir $partsDir -Disabled:$NoCacheFlag

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
            -ExpectedSha '' -PartsDir $partsDir -MaxAttempts $MaxAttempts `
            -StallTimeout $StallTimeout -ConnectTimeout $ConnectTimeout
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
            -ExpectedSha $expectedSha -PartsDir $partsDir -MaxAttempts $MaxAttempts `
            -StallTimeout $StallTimeout -ConnectTimeout $ConnectTimeout
    } catch {
        Fail $_.Exception.Message
    }
    if ($IndexSha -and (Get-FileSha256 $archivePath) -ne $IndexSha) {
        Fail "downloaded $AssetName does not match the rpi-pages profile index; aborting"
    }
    Write-Host 'sha256 integrity check passed'

    # ---- safe extraction + install ------------------------------------------

    $extractDir = Join-Path $TmpDir 'x'
    try {
        $bundleMembers = @(Get-ZipMemberNames -Path $archivePath)
    } catch {
        Fail "archive $AssetName is not a readable zip: $($_.Exception.Message)"
    }
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
        New-Item -ItemType File -Path $probe -ErrorAction Stop | Out-Null
        # Linux pwsh treats dotfiles as hidden and refuses Remove-Item without
        # -Force; the write probe must not turn a writable directory into a
        # false "not writable" failure.
        Remove-Item -LiteralPath $probe -Force -ErrorAction Stop
    } catch {
        Fail "install directory $Prefix is not writable - re-run from an elevated PowerShell (Run as administrator)"
    }

    $InstallPath = Join-Path $Prefix 'rpi.exe'
    $PrefixAbs = (Resolve-Path $Prefix).Path
    $InstallPath = Join-Path $PrefixAbs 'rpi.exe'
    $existingManifestPath = Join-Path $PrefixAbs 'rpi.install.json'
    foreach ($installTarget in @($InstallPath, $existingManifestPath)) {
        $item = Get-Item -LiteralPath $installTarget -Force -ErrorAction SilentlyContinue
        if ($item -and $item.PSIsContainer) {
            Fail "refusing to replace non-file path: $installTarget"
        }
    }
    # Stage only after the target precheck passes: a Fail above must not leave
    # a staging file behind in the install prefix.
    $stageBin = Join-Path $PrefixAbs ".rpi.stage-$PID.exe"
    Copy-Item $extracted $stageBin -Force
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
    Commit-InstallFiles -StageBin $stageBin -InstallPath $InstallPath `
        -StageManifest $stageManifest -ManifestPath $ManifestPath

    $pluginResult = $null
    # Settings outcome is a three-way state: seeded, existing and failed must
    # not share a message (a failed fresh seed has no settings.json at all).
    $settingsState = 'skipped'
    if ($ProfileValue -ne 'mini') {
        $AgentDir = if ($env:RPI_CODING_AGENT_DIR) { $env:RPI_CODING_AGENT_DIR } else { Join-Path $env:USERPROFILE '.rpi\agent' }
        $pluginResult = Install-ProfilePlugins -BundleDir $extractDir -Version $Version `
            -ProfileName $ProfileValue -Target $Target -AgentDir $AgentDir
        $settingsPath = Join-Path $AgentDir 'settings.json'
        if ($pluginResult.Failed -eq 0 -and -not (Test-Path -LiteralPath $settingsPath)) {
            $defaults = Join-Path $extractDir 'settings.defaults.json'
            if (Test-Path -LiteralPath $defaults -PathType Leaf) {
                if (Seed-DefaultSettings -DefaultsPath $defaults -SettingsPath $settingsPath -AgentDir $AgentDir) {
                    $settingsState = 'seeded'
                    Write-Host "seeded default settings: $settingsPath"
                } else {
                    $settingsState = 'failed'
                }
            }
        } elseif (Test-Path -LiteralPath $settingsPath) {
            $settingsState = 'existing'
            Write-Host ''
            Write-Host "existing settings left untouched: $settingsPath"
        } else {
            Write-Warning 'skipping settings seeding because not all plugins were installed'
        }
    }

    # ---- guidance -----------------------------------------------------------

    Write-Host ''
    Write-Host "rpi $Version installed - $InstallChannel - profile $ProfileValue"
    Write-Host "  binary:   $InstallPath"
    Write-Host "  asset:    $AssetName"
    Write-Host "  manifest: $ManifestPath"
    if ($pluginResult -and $ProfileValue -ne 'mini') {
        $pluginsText = (@(Get-ProfilePluginList -ProfileName $ProfileValue) | ForEach-Object { "$_ $Version" }) -join ' - '
        Write-Host "  plugins:  $pluginsText"
        Write-Host "            $($pluginResult.Installed)/$($pluginResult.Total) installed"
        if ($pluginResult.Failed -eq 0) {
            switch ($settingsState) {
                'seeded' {
                    Write-Host "            settings seeded: $(Join-Path $AgentDir 'settings.json')"
                }
                'existing' {
                    Write-Host "            plugins installed in: $(Join-Path $AgentDir 'extensions')"
                    Write-Host '            existing settings were not rewritten; run rpi install <name> to track an untracked plugin.'
                }
                'failed' {
                    Write-Host "            plugins installed in: $(Join-Path $AgentDir 'extensions')"
                    Write-Host "            settings were NOT created; add one manually at $(Join-Path $AgentDir 'settings.json') or rerun the installer."
                }
                default {
                    Write-Host "            plugins installed in: $(Join-Path $AgentDir 'extensions')"
                }
            }
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
    foreach ($leftover in @($stageBin, $stageManifest)) {
        if ($leftover) {
            Remove-Item -LiteralPath $leftover -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
