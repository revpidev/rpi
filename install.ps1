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

    Download fallback order (ADR-0011 revision, for networks where GitHub is
    unreachable): 1) the browser_download_url pair returned by the GitHub API,
    2) the URL constructed by naming rule from RPI_RELEASE_BASE_URL, 3) the
    official-site mirror under RPI_SITE_BASE_URL. A failed DOWNLOAD moves on
    to the next candidate; a failed sha256 integrity check is always fatal.
#>
[CmdletBinding()]
param(
    [string]$Prefix = $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'Programs\rpi' } else { Join-Path $env:USERPROFILE '.local\bin' }),
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

# ---- platform check ----------------------------------------------------------

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    Fail "unsupported CPU architecture: $env:PROCESSOR_ARCHITECTURE (only $Target builds are published)"
}
Write-Host "detected platform: $Target"

# TLS 1.2 for Windows PowerShell 5.1 (default there is TLS 1.0).
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

# ---- version / asset URL resolution ------------------------------------------

$Version = $null
$ArchiveUrl = $null
$ChecksumUrl = $null

# Preferred path: GitHub API - tag_name gives the version, assets give the
# exact browser_download_url (no name construction).
try {
    $release = Invoke-RestMethod -Uri $GitHubApiUrl -Headers @{ 'User-Agent' = 'rpi-installer' }
    $v = ([string]$release.tag_name) -replace '^v', ''
    if ($v) {
        $name = "rpi-$v-$Target.zip"
        $asset = $release.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
        $shaAsset = $release.assets | Where-Object { $_.name -eq "$name.sha256" } | Select-Object -First 1
        if ($asset -and $shaAsset) {
            $Version = $v
            $ArchiveUrl = $asset.browser_download_url
            $ChecksumUrl = $shaAsset.browser_download_url
        }
    }
} catch { }

if ($ArchiveUrl) {
    Write-Host "latest release: v$Version (via GitHub API)"
} else {
    # Fallback path: version endpoint ({"version": ...}) + URL construction by
    # naming rule. Fatal on failure - there is no further fallback.
    Write-Warning "GitHub API unavailable; falling back to version endpoint"
    try {
        $infoDoc = Invoke-RestMethod -Uri $VersionCheckUrl
    } catch {
        Fail "version endpoint $VersionCheckUrl is unreachable: $($_.Exception.Message)"
    }
    $Version = ([string]$infoDoc.version) -replace '^v', ''
    if (-not $Version) { Fail "version endpoint $VersionCheckUrl did not return a version" }
    $ArchiveUrl = "$ReleaseBaseUrl/download/v$Version/rpi-$Version-$Target.zip"
    $ChecksumUrl = "$ArchiveUrl.sha256"
    Write-Host "latest release: v$Version (via version endpoint)"
}

$ArchiveName = "rpi-$Version-$Target.zip"

# Ordered download candidates (see the script header for the fallback
# order): the API's browser_download_url first (API path), then the
# naming-rule GitHub URL, then the official-site mirror - deduplicated.
$Candidates = New-Object System.Collections.Generic.List[string]
foreach ($u in @(
        $ArchiveUrl,
        "$ReleaseBaseUrl/download/v$Version/$ArchiveName",
        "$SiteBaseUrl/releases/download/v$Version/$ArchiveName")) {
    if ($u -and -not $Candidates.Contains($u)) { $Candidates.Add($u) }
}

# ---- download + integrity check -----------------------------------------------

$TmpDir = Join-Path ([IO.Path]::GetTempPath()) ("rpi-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $ArchiveName
    $ShaPath = "$ZipPath.sha256"

    $DownloadSource = $null
    foreach ($candidate in $Candidates) {
        Write-Host "downloading $candidate"
        try {
            Invoke-WebRequest -Uri $candidate -OutFile $ZipPath
            # The sidecar URL is <archive>.sha256 by construction - on
            # GitHub and on the site mirror alike.
            Invoke-WebRequest -Uri "$candidate.sha256" -OutFile $ShaPath
        } catch {
            Write-Warning "download failed from ${candidate}: $($_.Exception.Message); trying the next mirror"
            continue
        }

        $expected = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLower()
        $actual = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            # An integrity failure is fatal - never switch mirrors on a bad
            # checksum.
            Fail "sha256 integrity check failed for $ArchiveName; aborting"
        }
        $DownloadSource = $candidate
        break
    }
    if (-not $DownloadSource) {
        Fail "all download candidates failed for $ArchiveName (tried: $($Candidates -join ', '))"
    }
    $ArchiveUrl = $DownloadSource
    Write-Host "sha256 integrity check passed"

    # ---- install ---------------------------------------------------------------

    $ExtractDir = Join-Path $TmpDir 'x'
    Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir
    $Extracted = Join-Path $ExtractDir 'rpi.exe'
    if (-not (Test-Path $Extracted)) { Fail "archive did not contain an rpi.exe binary" }

    try {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
        # writability probe
        $probe = Join-Path $Prefix ('.rpi-write-test-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType File -Path $probe | Out-Null
        Remove-Item $probe
    } catch {
        Fail "install directory $Prefix is not writable - re-run from an elevated PowerShell (Run as administrator)"
    }

    $InstallPath = Join-Path $Prefix 'rpi.exe'
    Copy-Item $Extracted $InstallPath -Force

    $InstallPath = (Resolve-Path $InstallPath).Path
    $PrefixAbs = Split-Path $InstallPath -Parent
    $InstalledAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

    $manifest = [ordered]@{
        version     = $Version
        target      = $Target
        installedAt = $InstalledAt
        sourceUrl   = $ArchiveUrl
        sha256      = $expected
        installPath = $InstallPath
        method      = 'binary'
    }
    $ManifestPath = Join-Path $PrefixAbs 'rpi.install.json'
    # Write UTF-8 without BOM so the manifest parses identically on every
    # platform (PS 5.1's "utf8" encoding emits a BOM).
    [IO.File]::WriteAllText($ManifestPath, ($manifest | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))

    # ---- guidance ----------------------------------------------------------------

    Write-Host ""
    Write-Host "rpi $Version installed to $InstallPath"
    Write-Host "  target:   $Target"
    Write-Host "  manifest: $ManifestPath"
    Write-Host ""
    Write-Host "next steps:"
    Write-Host "  update:    rpi update --self"
    Write-Host "  uninstall: rpi self-uninstall   (add --purge to also delete ~/.rpi)"

    Update-RpiPath $InstallPath $PrefixAbs
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
