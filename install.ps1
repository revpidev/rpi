<#
.SYNOPSIS
    Official installer for the rpi CLI on Windows.

.DESCRIPTION
    Downloads the latest rpi release asset for x86_64-pc-windows-msvc from
    GitHub Releases, verifies its SHA-256 (an integrity check against
    corrupted downloads — it does NOT protect against tampered release
    assets; signing is a planned follow-up, see ADR-0011), installs the
    binary, and writes the install manifest (rpi.install.json) next to it.

    Usage:
      powershell -ExecutionPolicy Bypass -File install.ps1
      .\install.ps1 -Prefix C:\tools\rpi

    One-liner:
      irm https://revpi.dev/install.ps1 | iex

.PARAMETER Prefix
    Install directory (default: $env:LOCALAPPDATA\Programs\rpi).

.NOTES
    Environment overrides (for mirrors and tests):
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

# Preferred path: GitHub API — tag_name gives the version, assets give the
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
    # naming rule. Fatal on failure — there is no further fallback.
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
# naming-rule GitHub URL, then the official-site mirror — deduplicated.
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
            # The sidecar URL is <archive>.sha256 by construction — on
            # GitHub and on the site mirror alike.
            Invoke-WebRequest -Uri "$candidate.sha256" -OutFile $ShaPath
        } catch {
            Write-Warning "download failed from ${candidate}: $($_.Exception.Message); trying the next mirror"
            continue
        }

        $expected = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLower()
        $actual = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            # An integrity failure is fatal — never switch mirrors on a bad
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
        Fail "install directory $Prefix is not writable — re-run from an elevated PowerShell (Run as administrator)"
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

    $pathEntries = $env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }
    if ($pathEntries -notcontains $PrefixAbs.TrimEnd('\')) {
        Write-Host ""
        Write-Host "note: $PrefixAbs is not in your PATH; add it with:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$PrefixAbs', 'User')"
        Write-Host "(then open a new terminal for the change to take effect)"
    }
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
