#!/usr/bin/env pwsh
# test-install-ps1-main.ps1 - end-to-end install.ps1 tests against a local fake
# Release. Complements test-install-ps1.ps1 (which loads only the pure helpers).
#
# The fake server is a Python script (python3 on Linux, python on Windows).
# Tests are skipped with a clear message when Python is unavailable. Runs on
# Windows PowerShell 5.1 and PowerShell 7+.
#
# Covered: mini/standard/full installs, plugin materialization, settings
# seeding, existing settings, corrupt archive rejection, a 206 without a
# response validator (must restart), and 429 Retry-After. Linux-only cases use
# a pty for the interactive prompt timeout.

$ErrorActionPreference = 'Stop'
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

function Get-FirstCommand {
    # A Windows runner can expose `python` twice (hosted tool cache + the Store
    # alias); a multiple-match Get-Command returns an array, and binding that to
    # Start-Process -FilePath turns into an unusable joined path. Take the first
    # PATH candidate only.
    param([string]$Name)
    $found = @(Get-Command $Name -ErrorAction SilentlyContinue)
    if ($found.Count -gt 0) { return $found[0] }
    return $null
}

if ($env:OS -eq 'Windows_NT') {
    $python = Get-FirstCommand 'python'
    if (-not $python) { $python = Get-FirstCommand 'python3' }
} else {
    $python = Get-FirstCommand 'python3'
    if (-not $python) { $python = Get-FirstCommand 'python' }
}
if (-not $python) {
    Write-Host 'ok - PowerShell main-flow tests # SKIP (python3/python unavailable)'
    exit 0
}
# `.Source` is the full path on ApplicationInfo in both editions; fall back in
# case a future shim exposes the command through another CommandInfo type.
$pythonExe = [string]$python.Source
if (-not $pythonExe) { $pythonExe = [string]$python.Path }
if (-not $pythonExe) { $pythonExe = [string]$python.Definition }

if ($PSVersionTable.PSVersion.Major -ge 6) {
    $shellExe = (Get-Command pwsh -ErrorAction SilentlyContinue).Source
    if (-not $shellExe) { $shellExe = $PSHOME + [IO.Path]::DirectorySeparatorChar + 'pwsh' }
} else {
    $shellExe = (Get-Command powershell -ErrorAction SilentlyContinue).Source
    if (-not $shellExe) { $shellExe = $PSHOME + [IO.Path]::DirectorySeparatorChar + 'powershell.exe' }
}
if (-not (Test-Path -LiteralPath $shellExe)) {
    Write-Host "cannot locate the PowerShell host at $shellExe"
    exit 1
}

$Version = '0.1.8-rc.4'
$Target = 'x86_64-pc-windows-msvc'
$StandardPlugins = @(
    'rpi-subagents', 'rpi-todo', 'rpi-ask-user-question',
    'rpi-smart-fetch', 'rpi-statusline', 'rpi-plan-mode'
)
$FullPlugins = $StandardPlugins + @('rpi-usage')

$tempRoot = Join-Path ([IO.Path]::GetTempPath()) ('rpi-install-main-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempRoot | Out-Null

$serverScript = Join-Path $tempRoot 'server.py'
$ptyScript = Join-Path $tempRoot 'pty_run.py'
$server = $null
$savedEnv = @{}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-PluginList {
    param([string]$ProfileName)
    if ($ProfileName -eq 'full') { return $FullPlugins }
    if ($ProfileName -eq 'standard') { return $StandardPlugins }
    return @()
}

function New-Bundle {
    param([string]$ProfileName, [string]$OutDir)
    $stage = Join-Path $tempRoot ("stage-$ProfileName")
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    [IO.File]::WriteAllText((Join-Path $stage 'rpi.exe'), "FAKE-RPI-$ProfileName")
    $plugins = @(Get-PluginList $ProfileName)
    if ($plugins.Count -gt 0) {
        $extDir = Join-Path $stage 'extensions'
        New-Item -ItemType Directory -Force -Path $extDir | Out-Null
        $settings = @{
            packages        = $plugins
            defaultSettings = @{ statusLine = @{ type = 'items'; items = @('live-total-tokens') } }
        }
        [IO.File]::WriteAllText((Join-Path $stage 'settings.defaults.json'), ($settings | ConvertTo-Json -Depth 6))
        foreach ($plugin in $plugins) {
            $pluginStage = Join-Path $tempRoot ("plugin-$ProfileName-$plugin")
            New-Item -ItemType Directory -Force -Path $pluginStage | Out-Null
            $manifest = @{
                name          = $plugin
                version       = $Version
                rpiAbi        = 1
                minHostVersion = $Version
            }
            [IO.File]::WriteAllText((Join-Path $pluginStage 'rpi-extension.json'), ($manifest | ConvertTo-Json -Compress))
            $manifestHash = Get-Sha256 (Join-Path $pluginStage 'rpi-extension.json')
            [IO.File]::WriteAllText((Join-Path $pluginStage 'SHA256SUMS'), "$manifestHash  rpi-extension.json`n")
            $rpixName = "$plugin-$Version-$Target.rpix"
            $rpixPath = Join-Path $extDir $rpixName
            # List the files explicitly: a "." archive member would be
            # rejected by Test-SafeTarArchive after its ./ normalization.
            & tar -czf $rpixPath -C $pluginStage rpi-extension.json SHA256SUMS 2>$null
            if ($LASTEXITCODE -ne 0) { throw "tar failed for $plugin" }
            $rpixHash = Get-Sha256 $rpixPath
            [IO.File]::WriteAllText("$rpixPath.sha256", "$rpixHash  $rpixName`n")
        }
    }
    if (-not (Test-Path -LiteralPath $OutDir)) {
        New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    }
    $suffix = if ($ProfileName -eq 'mini') { '' } else { "-$ProfileName" }
    $assetName = "rpi-$Version-$Target$suffix.zip"
    $assetPath = Join-Path $OutDir $assetName
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $assetPath)
    $zipHash = Get-Sha256 $assetPath
    [IO.File]::WriteAllText("$assetPath.sha256", "$zipHash  $assetName`n")
    return [pscustomobject]@{ Name = $assetName; Path = $assetPath; Sha = $zipHash }
}

$bundles = @{}
foreach ($profile in @('mini', 'standard', 'full')) {
    $bundle = New-Bundle -ProfileName $profile -OutDir (Join-Path $tempRoot 'assets')
    $bundles[$profile] = $bundle
}

function Write-ReleaseTree {
    param([string]$Root, [hashtable]$BundleMap, [string]$BaseUrl)
    $releaseDir = Join-Path $Root "releases/download/v$Version"
    New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
    $assets = @()
    foreach ($profile in @('mini', 'standard', 'full')) {
        $bundle = $BundleMap[$profile]
        Copy-Item -LiteralPath $bundle.Path -Destination (Join-Path $releaseDir $bundle.Name) -Force
        Copy-Item -LiteralPath "$($bundle.Path).sha256" -Destination (Join-Path $releaseDir "$($bundle.Name).sha256") -Force
        $size = (Get-Item -LiteralPath $bundle.Path).Length
        $assets += @{
            name                 = $bundle.Name
            size                 = $size
            browser_download_url = "$BaseUrl/releases/download/v$Version/$($bundle.Name)"
        }
        $assets += @{
            name                 = "$($bundle.Name).sha256"
            browser_download_url = "$BaseUrl/releases/download/v$Version/$($bundle.Name).sha256"
        }
    }
    $release = @{ tag_name = "v$Version"; assets = $assets }
    [IO.File]::WriteAllText((Join-Path $Root 'api-latest.json'), ($release | ConvertTo-Json -Depth 6))
    $versionDoc = @{ version = $Version; packageName = 'rpi'; note = 'fake release' }
    [IO.File]::WriteAllText((Join-Path $Root 'latest-version.json'), ($versionDoc | ConvertTo-Json -Depth 4))
    $indexDir = Join-Path $Root 'api/install-profiles'
    New-Item -ItemType Directory -Force -Path $indexDir | Out-Null
    $profileAssets = @{}
    $profileAssets[$bundles['standard'].Name] = $bundles['standard'].Sha
    $profileAssets[$bundles['full'].Name] = $bundles['full'].Sha
    $index = @{ schemaVersion = 1; version = $Version; assets = $profileAssets }
    [IO.File]::WriteAllText((Join-Path $indexDir "$Version.json"), ($index | ConvertTo-Json -Depth 5))
}

function Read-SharedText {
    # Windows keeps the Start-Process redirection files open with write access,
    # so [IO.File]::ReadAllText (which asks for FileShare.Read and therefore
    # denies writers) fails with "file is being used by another process" while
    # the child runs. Open with FileShare.ReadWrite instead and return '' on any
    # transient failure so callers keep polling.
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    try {
        $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
        try {
            $reader = New-Object System.IO.StreamReader($stream)
            try { return [string]$reader.ReadToEnd() } finally { $reader.Dispose() }
        } finally {
            $stream.Dispose()
        }
    } catch {
        return ''
    }
}

function Start-TestServer {
    param([string]$Root, [string]$Mode = 'normal')
    $log = Join-Path $Root 'server.log'
    $marker = Join-Path $Root 'server.marker'
    $portFile = Join-Path $Root 'server.port'
    $stdout = Join-Path $Root 'server.out'
    $stderr = Join-Path $Root 'server.err'
    Remove-Item -LiteralPath $portFile -ErrorAction SilentlyContinue
    $script:savedEnv['RPI_TEST_ROOT'] = $env:RPI_TEST_ROOT
    $script:savedEnv['RPI_TEST_MODE'] = $env:RPI_TEST_MODE
    $script:savedEnv['RPI_TEST_LOG'] = $env:RPI_TEST_LOG
    $script:savedEnv['RPI_TEST_MARKER'] = $env:RPI_TEST_MARKER
    $env:RPI_TEST_ROOT = $Root
    $env:RPI_TEST_MODE = $Mode
    $env:RPI_TEST_LOG = $log
    $env:RPI_TEST_MARKER = $marker
    $proc = Start-Process -FilePath $pythonExe -ArgumentList ('-u "' + $serverScript + '"') `
        -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $env:RPI_TEST_ROOT = $script:savedEnv['RPI_TEST_ROOT']
    $env:RPI_TEST_MODE = $script:savedEnv['RPI_TEST_MODE']
    $env:RPI_TEST_LOG = $script:savedEnv['RPI_TEST_LOG']
    $env:RPI_TEST_MARKER = $script:savedEnv['RPI_TEST_MARKER']
    $port = 0
    for ($i = 0; $i -lt 100; $i++) {
        # The port file is written once and closed, so it is readable on every
        # platform; the redirection file is the fallback for old launches.
        $text = Read-SharedText -Path $portFile
        if ($text.Length -eq 0) { $text = Read-SharedText -Path $stdout }
        if ($text.Length -gt 0) {
            $match = [regex]::Match($text, 'PORT=(\d+)')
            if ($match.Success) { $port = [int]$match.Groups[1].Value; break }
        }
        Start-Sleep -Milliseconds 100
    }
    if ($port -le 0) {
        $err = Read-SharedText -Path $stderr
        $state = 'still running'
        try { if ($proc.HasExited) { $state = "exit=$($proc.ExitCode)" } } catch { $state = 'state unknown' }
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        throw "test HTTP server did not start ($pythonExe $state): $err"
    }
    return [pscustomobject]@{ Process = $proc; Port = $port; Log = $log; Marker = $marker; BaseUrl = "http://127.0.0.1:$port" }
}

function Stop-TestServer {
    param($Server)
    if ($Server -and $Server.Process) {
        Stop-Process -Id $Server.Process.Id -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-Installer {
    param(
        [string]$Prefix,
        [string]$ProfileName = '',
        [bool]$Consent = $false,
        [hashtable]$Env = @{}
    )
    $arguments = @('-NoProfile')
    if ($PSVersionTable.PSVersion.Major -lt 6) { $arguments += @('-ExecutionPolicy', 'Bypass') }
    $arguments += @('-File', $installer, '-Prefix', $Prefix)
    if ($ProfileName) { $arguments += @('-Profile', $ProfileName) }
    if ($Consent) { $arguments += '-Yes' }
    $saved = @{}
    foreach ($key in $Env.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        [Environment]::SetEnvironmentVariable($key, [string]$Env[$key])
    }
    try {
        $output = & $shellExe @arguments 2>&1 | Out-String
        $code = $LASTEXITCODE
        return [pscustomobject]@{ ExitCode = $code; Output = $output }
    } finally {
        foreach ($key in $saved.Keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key])
        }
    }
}

function New-InstallEnv {
    param([string]$BaseUrl, [string]$AgentDir, [string]$CacheDir)
    return @{
        'PROCESSOR_ARCHITECTURE'   = 'AMD64'
        'RPI_GITHUB_API_URL'       = "$BaseUrl/api-latest.json"
        'RPI_VERSION_CHECK_URL'    = "$BaseUrl/latest-version.json"
        'RPI_RELEASE_BASE_URL'     = "$BaseUrl/releases"
        'RPI_SITE_BASE_URL'        = $BaseUrl
        'RPI_CODING_AGENT_DIR'     = $AgentDir
        'RPI_INSTALL_CACHE_DIR'    = $CacheDir
        'RPI_INSTALL_ADD_PATH'     = '0'
        'RPI_NONINTERACTIVE'       = '1'
        'CI'                       = ''
    }
}

# ---- fake HTTP server and pty runner ---------------------------------------

[IO.File]::WriteAllText($serverScript, @'
import email.utils
import hashlib
import http.server
import os
import re
import sys
import time

ROOT = os.environ["RPI_TEST_ROOT"]
MODE = os.environ.get("RPI_TEST_MODE", "normal")
LOG = os.environ["RPI_TEST_LOG"]
MARKER = os.environ["RPI_TEST_MARKER"]
TIMES = LOG + ".times"


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, fmt, *args):
        line = fmt % args
        with open(LOG, "a") as handle:
            handle.write(line + "\n")
        # Monotonic per-request timestamps let tests measure real waits (for
        # example Retry-After) instead of the whole child-process runtime.
        with open(TIMES, "a") as handle:
            handle.write("%.6f %s\n" % (time.monotonic(), line))

    def serve(self, head_only):
        path = self.path.split("?", 1)[0].lstrip("/")
        file_path = os.path.join(ROOT, path)
        if not os.path.isfile(file_path):
            self.send_error(404)
            return
        with open(file_path, "rb") as handle:
            data = handle.read()
        etag = '"%s"' % hashlib.sha256(data).hexdigest()[:32]
        last_modified = email.utils.formatdate(os.path.getmtime(file_path), usegmt=True)
        status = 200
        start, end = 0, len(data) - 1
        body = data
        omit_validators = False
        range_header = self.headers.get("Range")
        if_range = self.headers.get("If-Range")
        if range_header and (if_range is None or if_range == etag):
            match = re.match(r"bytes=(\d+)-(\d*)$", range_header)
            if match:
                start = int(match.group(1))
                end = int(match.group(2)) if match.group(2) else len(data) - 1
                if start >= len(data):
                    self.send_response(416)
                    self.send_header("Content-Range", "bytes */%d" % len(data))
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                end = min(end, len(data) - 1)
                status = 206
                body = data[start : end + 1]

        if (MODE == "206-no-validator" and status == 206
                and file_path.endswith(".zip") and not os.path.exists(MARKER)):
            open(MARKER, "w").close()
            omit_validators = True
            # Wrong entity: appending it must fail the final sha256 check.
            body = b"X" * len(body)
        if (MODE == "429" and file_path.endswith(".zip")
                and not os.path.exists(MARKER)):
            open(MARKER, "w").close()
            self.send_response(429)
            self.send_header("Retry-After", "1")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if MODE == "slow" and file_path.endswith(".zip"):
            time.sleep(5)
        truncate_now = False
        if (MODE == "truncate" and file_path.endswith(".zip")
                and not os.path.exists(MARKER)):
            open(MARKER, "w").close()
            truncate_now = True

        self.send_response(status)
        if file_path.endswith(".sha256") or file_path.endswith(".json"):
            self.send_header("Content-Type", "text/plain; charset=utf-8")
        else:
            self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        if not omit_validators:
            self.send_header("ETag", etag)
            self.send_header("Last-Modified", last_modified)
        self.send_header("Accept-Ranges", "bytes")
        if status == 206:
            self.send_header("Content-Range", "bytes %d-%d/%d" % (start, end, len(data)))
        self.end_headers()
        if not head_only:
            if truncate_now:
                # Advertise the full length, then close after half the body so
                # the installer has to retry with Range + If-Range.
                self.wfile.write(body[: max(1, len(body) // 2)])
                self.wfile.flush()
                self.close_connection = True
            else:
                self.wfile.write(body)

    def do_GET(self):
        self.serve(False)

    def do_HEAD(self):
        self.serve(True)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
# Publish the port twice: stdout is unreadable on Windows while the parent keeps
# the redirection file locked, so the closed port file is the primary signal.
port = server.server_address[1]
with open(os.path.join(ROOT, "server.port"), "w") as handle:
    handle.write("PORT=%d\n" % port)
print("PORT=%d" % port, flush=True)
server.serve_forever()
'@)

[IO.File]::WriteAllText($ptyScript, @'
import os
import pty
import subprocess
import sys

command = sys.argv[1:]
master, slave = pty.openpty()
process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, env=os.environ)
os.close(slave)
output = b""
while True:
    try:
        chunk = os.read(master, 4096)
    except OSError:
        break
    if not chunk:
        break
    output += chunk
os.close(master)
process.wait()
sys.stdout.write(output.decode("utf-8", "replace"))
sys.exit(process.returncode)
'@)

try {
    $normalRoot = Join-Path $tempRoot 'www-normal'
    New-Item -ItemType Directory -Force -Path $normalRoot | Out-Null
    $normal = Start-TestServer -Root $normalRoot -Mode 'normal'
    try {
        Write-ReleaseTree -Root $normalRoot -BundleMap $bundles -BaseUrl $normal.BaseUrl

        # ---- mini ----------------------------------------------------------
        $miniPrefix = Join-Path $tempRoot 'prefix-mini'
        $miniAgent = Join-Path $tempRoot 'agent-mini'
        $miniCache = Join-Path $tempRoot 'cache-mini'
        $result = Invoke-Installer -Prefix $miniPrefix -ProfileName 'mini' `
            -Env (New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir $miniAgent -CacheDir $miniCache)
        if ($result.ExitCode -ne 0) {
            Write-Host '--- mini install output ---'
            Write-Host $result.Output
            if (Test-Path -LiteralPath $normal.Log) { Write-Host '--- server log ---'; Write-Host (Read-SharedText -Path $normal.Log) }
        }
        Check-Equal '0' ([string]$result.ExitCode) 'mini install exits 0'
        Check-True (Test-Path -LiteralPath (Join-Path $miniPrefix 'rpi.exe')) 'mini installs the binary'
        Check-True (Test-Path -LiteralPath (Join-Path $miniPrefix 'rpi.install.json')) 'mini writes the manifest'
        Check-True (-not (Test-Path -LiteralPath $miniAgent)) 'mini does not create an agent dir'

        # ---- standard ------------------------------------------------------
        $stdPrefix = Join-Path $tempRoot 'prefix-standard'
        $stdAgent = Join-Path $tempRoot 'agent-standard'
        $stdCache = Join-Path $tempRoot 'cache-standard'
        $result = Invoke-Installer -Prefix $stdPrefix -ProfileName 'standard' -Consent $true `
            -Env (New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir $stdAgent -CacheDir $stdCache)
        Check-Equal '0' ([string]$result.ExitCode) 'standard install exits 0'
        if (-not ($result.Output -match '6/6 installed')) {
            Write-Host '--- standard install output ---'
            Write-Host $result.Output
        }
        Check-True ($result.Output -match '6/6 installed') 'standard reports 6/6 plugins'
        Check-True ($result.Output -match 'settings seeded:') 'standard reports seeded settings'
        Check-True (Test-Path -LiteralPath (Join-Path $stdAgent 'extensions')) 'standard creates the extension dir'
        $stdDirs = @()
        if (Test-Path -LiteralPath (Join-Path $stdAgent 'extensions')) {
            $stdDirs = @(Get-ChildItem -LiteralPath (Join-Path $stdAgent 'extensions') -Directory)
        }
        Check-Equal '6' ([string]$stdDirs.Count) 'standard materializes six plugin directories'
        $stdSettings = Join-Path $stdAgent 'settings.json'
        Check-True (Test-Path -LiteralPath $stdSettings) 'standard seeds settings.json'
        $stdSettingsText = ''
        if (Test-Path -LiteralPath $stdSettings) { $stdSettingsText = [string](Get-Content -LiteralPath $stdSettings -Raw) }
        Check-True ($stdSettingsText -match 'rpi-subagents') 'seeded settings list the plugins'
        Check-True ($stdSettingsText -match 'statusLine') 'seeded settings carry statusline defaults'
        Check-True (-not ($stdSettingsText -match 'rpi-usage')) 'standard settings exclude usage'

        # ---- full ----------------------------------------------------------
        $fullPrefix = Join-Path $tempRoot 'prefix-full'
        $fullAgent = Join-Path $tempRoot 'agent-full'
        $fullCache = Join-Path $tempRoot 'cache-full'
        $result = Invoke-Installer -Prefix $fullPrefix -ProfileName 'full' -Consent $true `
            -Env (New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir $fullAgent -CacheDir $fullCache)
        Check-Equal '0' ([string]$result.ExitCode) 'full install exits 0'
        Check-True ($result.Output -match '7/7 installed') 'full reports 7/7 plugins'
        $fullDirs = @()
        if (Test-Path -LiteralPath (Join-Path $fullAgent 'extensions')) {
            $fullDirs = @(Get-ChildItem -LiteralPath (Join-Path $fullAgent 'extensions') -Directory)
        }
        Check-Equal '7' ([string]$fullDirs.Count) 'full materializes seven plugin directories'
        $fullSettings = Join-Path $fullAgent 'settings.json'
        $fullSettingsText = ''
        if (Test-Path -LiteralPath $fullSettings) { $fullSettingsText = [string](Get-Content -LiteralPath $fullSettings -Raw) }
        Check-True ($fullSettingsText -match 'rpi-usage') 'full settings include usage'

        # ---- existing settings are untouched -------------------------------
        $existingPrefix = Join-Path $tempRoot 'prefix-existing'
        $existingAgent = Join-Path $tempRoot 'agent-existing'
        New-Item -ItemType Directory -Force -Path $existingAgent | Out-Null
        $existingSettings = Join-Path $existingAgent 'settings.json'
        $existingBytes = "{`n  `"packages`": [],`n  `"theme`": `"custom`"`n}`n"
        [IO.File]::WriteAllText($existingSettings, $existingBytes)
        $existingHash = Get-Sha256 $existingSettings
        $result = Invoke-Installer -Prefix $existingPrefix -ProfileName 'standard' -Consent $true `
            -Env (New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir $existingAgent -CacheDir (Join-Path $tempRoot 'cache-existing'))
        Check-Equal '0' ([string]$result.ExitCode) 'install with existing settings exits 0'
        Check-Equal $existingHash (Get-Sha256 $existingSettings) 'existing settings stay byte-identical'
        Check-True ($result.Output -match 'existing settings left untouched') 'existing settings are reported as untouched'
        Check-True (-not ($result.Output -match 'registered in')) 'summary does not claim existing settings were registered'
        Check-True ($result.Output -match 'plugins installed in:') 'summary names the extension directory instead'

        # ---- failed seeding is reported as failed ---------------------------
        # Two phases: install once so the plugin directories exist, then make
        # the agent dir itself read-only. Plugins live in extensions/ (kept
        # writable), so only the settings.json seed fails.
        $seedFailPrefix = Join-Path $tempRoot 'prefix-seedfail'
        $seedFailAgent = Join-Path $tempRoot 'agent-seedfail'
        $seedFailEnv = New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir $seedFailAgent -CacheDir (Join-Path $tempRoot 'cache-seedfail')
        $result = Invoke-Installer -Prefix $seedFailPrefix -ProfileName 'standard' -Consent $true -Env $seedFailEnv
        Check-Equal '0' ([string]$result.ExitCode) 'seed-failure setup install exits 0'
        $seedFailSettings = Join-Path $seedFailAgent 'settings.json'
        $seedFailBlocked = $false
        $restoreSeedFail = $null
        if ($env:OS -ne 'Windows_NT' -and (Get-Command chmod -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $seedFailSettings -Force -ErrorAction SilentlyContinue
            & chmod 755 (Join-Path $seedFailAgent 'extensions') 2>$null | Out-Null
            & chmod 555 $seedFailAgent 2>$null | Out-Null
            try {
                [IO.File]::WriteAllText((Join-Path $seedFailAgent 'probe-write'), 'x')
                Remove-Item -LiteralPath (Join-Path $seedFailAgent 'probe-write') -Force -ErrorAction SilentlyContinue
            } catch {
                $seedFailBlocked = $true
            }
            $restoreSeedFail = { & chmod 755 $seedFailAgent 2>$null | Out-Null }
        } elseif ($env:OS -eq 'Windows_NT' -and (Get-Command icacls -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $seedFailSettings -Force -ErrorAction SilentlyContinue
            $who = "$env:USERNAME"
            # Deny writes only on the agent root. OI/CI would inherit the deny
            # into extensions/ and fail plugin materialization before seeding.
            & icacls $seedFailAgent /deny "${who}:(W)" 2>$null | Out-Null
            try {
                [IO.File]::WriteAllText((Join-Path $seedFailAgent 'probe-write'), 'x')
                Remove-Item -LiteralPath (Join-Path $seedFailAgent 'probe-write') -Force -ErrorAction SilentlyContinue
            } catch {
                $seedFailBlocked = $true
            }
            $restoreSeedFail = { & icacls $seedFailAgent /remove:d $who 2>$null | Out-Null }
        }
        if ($seedFailBlocked) {
            # Prove the fixture still permits the exact plugin temporary-dir
            # operation; otherwise the test is not isolating settings seeding.
            $extensionProbe = Join-Path (Join-Path $seedFailAgent 'extensions') 'probe-write-dir'
            try {
                New-Item -ItemType Directory -Path $extensionProbe -ErrorAction Stop | Out-Null
                Remove-Item -LiteralPath $extensionProbe -Force -ErrorAction Stop
            } catch {
                if ($restoreSeedFail) { & $restoreSeedFail }
                throw "seed-failure fixture left extensions unwritable: $($_.Exception.Message)"
            }
            try {
                $result = Invoke-Installer -Prefix $seedFailPrefix -ProfileName 'standard' -Consent $true -Env $seedFailEnv
                Check-Equal '0' ([string]$result.ExitCode) 'failed seeding still exits 0'
                Check-True ($result.Output -match 'settings were NOT created') 'failed seeding is reported as not created'
                Check-True (-not ($result.Output -match 'existing settings were not rewritten')) 'failed seeding does not claim existing settings were kept'
                Check-True (-not (Test-Path -LiteralPath $seedFailSettings)) 'failed seeding leaves no settings.json'
            } finally {
                if ($restoreSeedFail) { & $restoreSeedFail }
            }
        } else {
            if ($restoreSeedFail) { & $restoreSeedFail }
            Write-Host 'ok - failed seeding summary # SKIP (could not make the agent dir read-only)'
        }

        # ---- corrupt archive is rejected -----------------------------------
        $badRoot = Join-Path $tempRoot 'www-corrupt'
        New-Item -ItemType Directory -Force -Path $badRoot | Out-Null
        $bad = Start-TestServer -Root $badRoot -Mode 'normal'
        try {
            Write-ReleaseTree -Root $badRoot -BundleMap $bundles -BaseUrl $bad.BaseUrl
            $badAsset = Join-Path $badRoot "releases/download/v$Version/$($bundles['mini'].Name)"
            [IO.File]::WriteAllText($badAsset, 'not a zip')
            $badPrefix = Join-Path $tempRoot 'prefix-corrupt'
            $result = Invoke-Installer -Prefix $badPrefix -ProfileName 'mini' `
                -Env (New-InstallEnv -BaseUrl $bad.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-corrupt') -CacheDir (Join-Path $tempRoot 'cache-corrupt'))
            Check-True ($result.ExitCode -ne 0) 'corrupt archive fails the install'
            Check-True (-not (Test-Path -LiteralPath (Join-Path $badPrefix 'rpi.exe'))) 'corrupt archive leaves no binary'
        } finally {
            Stop-TestServer $bad
        }

        # ---- oversized profile index is rejected before download -----------
        $hugeRoot = Join-Path $tempRoot 'www-huge-index'
        New-Item -ItemType Directory -Force -Path $hugeRoot | Out-Null
        $huge = Start-TestServer -Root $hugeRoot -Mode 'normal'
        try {
            Write-ReleaseTree -Root $hugeRoot -BundleMap $bundles -BaseUrl $huge.BaseUrl
            $hugeIndex = Join-Path $hugeRoot "api/install-profiles/$Version.json"
            [IO.File]::WriteAllText($hugeIndex, '{' + (' ' * 300000))
            $hugePrefix = Join-Path $tempRoot 'prefix-huge-index'
            $result = Invoke-Installer -Prefix $hugePrefix -ProfileName 'standard' -Consent $true `
                -Env (New-InstallEnv -BaseUrl $huge.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-huge') -CacheDir (Join-Path $tempRoot 'cache-huge'))
            Check-True ($result.ExitCode -ne 0) 'oversized profile index fails the install'
            Check-True ($result.Output -match 'too large') 'oversized profile index reports the byte limit'
            Check-True (-not (Test-Path -LiteralPath (Join-Path $hugePrefix 'rpi.exe'))) 'oversized index downloads no binary'
        } finally {
            Stop-TestServer $huge
        }

        # ---- 206 without a response validator ------------------------------
        $resumeRoot = Join-Path $tempRoot 'www-resume'
        New-Item -ItemType Directory -Force -Path $resumeRoot | Out-Null
        $resume = Start-TestServer -Root $resumeRoot -Mode '206-no-validator'
        try {
            Write-ReleaseTree -Root $resumeRoot -BundleMap $bundles -BaseUrl $resume.BaseUrl
            $resumePrefix = Join-Path $tempRoot 'prefix-resume'
            $resumeCache = Join-Path $tempRoot 'cache-resume'
            $parts = Join-Path $resumeCache 'parts'
            New-Item -ItemType Directory -Force -Path $parts | Out-Null
            $mini = $bundles['mini']
            $miniBytes = [IO.File]::ReadAllBytes($mini.Path)
            $half = [int]($miniBytes.Length / 2)
            $partial = New-Object byte[] $half
            [Array]::Copy($miniBytes, 0, $partial, 0, $half)
            [IO.File]::WriteAllBytes((Join-Path $parts $mini.Name), $partial)
            $etag = '"' + $mini.Sha.Substring(0, 32) + '"'
            $metaLines = @(
                "sha=$($mini.Sha)",
                "asset=$($mini.Name)",
                "version=$Version",
                "etag=$etag",
                'last_modified=',
                "url=$($resume.BaseUrl)/releases/download/v$Version/$($mini.Name)"
            )
            [IO.File]::WriteAllText((Join-Path $parts "$($mini.Name).meta"), (($metaLines -join "`n") + "`n"))
            $result = Invoke-Installer -Prefix $resumePrefix -ProfileName 'mini' `
                -Env (New-InstallEnv -BaseUrl $resume.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-resume') -CacheDir $resumeCache)
            $resumeLog = Read-SharedText -Path $resume.Log
            Check-Equal '0' ([string]$result.ExitCode) '206 without a validator restarts and succeeds'
            Check-True ($result.Output -match 'restarting from zero') '206 without a validator is reported as a restart'
            Check-True ($resumeLog -match ' 206 ') 'the first resume request used 206'
            Check-True ($resumeLog -match ' 200 ') 'the retry used a clean 200'
            Check-True (Test-Path -LiteralPath (Join-Path $resumePrefix 'rpi.exe')) 'the restarted download installs the binary'
        } finally {
            Stop-TestServer $resume
        }

        # ---- 429 Retry-After ----------------------------------------------
        $retryRoot = Join-Path $tempRoot 'www-retry'
        New-Item -ItemType Directory -Force -Path $retryRoot | Out-Null
        $retry = Start-TestServer -Root $retryRoot -Mode '429'
        try {
            Write-ReleaseTree -Root $retryRoot -BundleMap $bundles -BaseUrl $retry.BaseUrl
            $retryPrefix = Join-Path $tempRoot 'prefix-retry'
            $retryEnv = New-InstallEnv -BaseUrl $retry.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-retry') -CacheDir (Join-Path $tempRoot 'cache-retry')
            $retryEnv['RPI_INSTALL_ATTEMPTS'] = '2'
            $retryTimes = "$($retry.Log).times"
            if (Test-Path -LiteralPath $retryTimes) { Remove-Item -LiteralPath $retryTimes -Force }
            $result = Invoke-Installer -Prefix $retryPrefix -ProfileName 'mini' -Env $retryEnv
            Check-Equal '0' ([string]$result.ExitCode) '429 with Retry-After eventually succeeds'
            $zipGets = @()
            foreach ($line in ((Read-SharedText -Path $retryTimes) -split "`n")) {
                if ($line -match '\.zip HTTP') { $zipGets += $line }
            }
            Check-Equal '2' ([string]$zipGets.Count) '429 archive was requested exactly twice'
            if ($zipGets.Count -ge 2) {
                $firstAt = [double](([string]$zipGets[0]).Split(' ')[0])
                $secondAt = [double](([string]$zipGets[1]).Split(' ')[0])
                $delta = $secondAt - $firstAt
                Check-True ($delta -ge 0.8 -and $delta -lt 5) 'Retry-After delay measured server-side between the two archive GETs'
            }
            Check-True (Test-Path -LiteralPath (Join-Path $retryPrefix 'rpi.exe')) '429 retry installs the binary'
        } finally {
            Stop-TestServer $retry
        }

        # ---- failed target precheck leaves no staging file -----------------
        $blockedRoot = Join-Path $tempRoot 'www-blocked'
        New-Item -ItemType Directory -Force -Path $blockedRoot | Out-Null
        $blocked = Start-TestServer -Root $blockedRoot -Mode 'normal'
        try {
            Write-ReleaseTree -Root $blockedRoot -BundleMap $bundles -BaseUrl $blocked.BaseUrl
            $blockedPrefix = Join-Path $tempRoot 'prefix-blocked'
            New-Item -ItemType Directory -Force -Path $blockedPrefix | Out-Null
            # A directory where the manifest belongs fails the precheck that
            # runs before the binary is committed.
            New-Item -ItemType Directory -Force -Path (Join-Path $blockedPrefix 'rpi.install.json') | Out-Null
            $result = Invoke-Installer -Prefix $blockedPrefix -ProfileName 'mini' `
                -Env (New-InstallEnv -BaseUrl $blocked.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-blocked') -CacheDir (Join-Path $tempRoot 'cache-blocked'))
            Check-True ($result.ExitCode -ne 0) 'directory at the manifest path fails the install'
            $stageLeft = @(Get-ChildItem -LiteralPath $blockedPrefix -Force -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -like '.rpi.stage-*' })
            Check-Equal '0' ([string]$stageLeft.Count) 'failed precheck leaves no staging file behind'
            Check-True (-not (Test-Path -LiteralPath (Join-Path $blockedPrefix 'rpi.exe'))) 'failed precheck installs no binary'
        } finally {
            Stop-TestServer $blocked
        }

        # ---- truncated first download resumes -------------------------------
        $truncateRoot = Join-Path $tempRoot 'www-truncate'
        New-Item -ItemType Directory -Force -Path $truncateRoot | Out-Null
        $truncate = Start-TestServer -Root $truncateRoot -Mode 'truncate'
        try {
            Write-ReleaseTree -Root $truncateRoot -BundleMap $bundles -BaseUrl $truncate.BaseUrl
            $truncatePrefix = Join-Path $tempRoot 'prefix-truncate'
            $truncateEnv = New-InstallEnv -BaseUrl $truncate.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-truncate') -CacheDir (Join-Path $tempRoot 'cache-truncate')
            $truncateEnv['RPI_INSTALL_ATTEMPTS'] = '3'
            $result = Invoke-Installer -Prefix $truncatePrefix -ProfileName 'mini' -Env $truncateEnv
            $truncateLog = Read-SharedText -Path $truncate.Log
            Check-Equal '0' ([string]$result.ExitCode) 'truncated first download resumes and succeeds'
            Check-True ($result.Output -match 'download failed|premature|truncat') 'truncated download is reported'
            Check-True ($result.Output -match 'retrying in [0-9]+\.[0-9]{2}s') 'truncation retry uses the jittered backoff'
            Check-True ($truncateLog -match ' 206 ') 'truncated retry used a Range request'
            Check-True (Test-Path -LiteralPath (Join-Path $truncatePrefix 'rpi.exe')) 'truncated retry installs the binary'
        } finally {
            Stop-TestServer $truncate
        }

        # ---- connect/response-header timeout -------------------------------
        $slowRoot = Join-Path $tempRoot 'www-slow'
        New-Item -ItemType Directory -Force -Path $slowRoot | Out-Null
        $slow = Start-TestServer -Root $slowRoot -Mode 'slow'
        try {
            Write-ReleaseTree -Root $slowRoot -BundleMap $bundles -BaseUrl $slow.BaseUrl
            $slowPrefix = Join-Path $tempRoot 'prefix-slow'
            $slowEnv = New-InstallEnv -BaseUrl $slow.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-slow') -CacheDir (Join-Path $tempRoot 'cache-slow')
            $slowEnv['RPI_INSTALL_ATTEMPTS'] = '1'
            $slowEnv['RPI_INSTALL_CONNECT_TIMEOUT'] = '1'
            $slowEnv['RPI_INSTALL_STALL_TIMEOUT'] = '1'
            $slowWatch = [Diagnostics.Stopwatch]::StartNew()
            $result = Invoke-Installer -Prefix $slowPrefix -ProfileName 'mini' -Env $slowEnv
            $slowWatch.Stop()
            Check-True ($result.ExitCode -ne 0) 'slow response headers fail instead of hanging'
            Check-True ($slowWatch.Elapsed.TotalSeconds -lt 15) 'connect timeout bounds the slow-header attempt'
            Check-True (-not (Test-Path -LiteralPath (Join-Path $slowPrefix 'rpi.exe'))) 'slow-header failure leaves no binary'
        } finally {
            Stop-TestServer $slow
        }

        # ---- PowerShell prompt timeout (Linux pty) -------------------------
        $hasScript = Get-Command script -ErrorAction SilentlyContinue
        $hasTimeout = Get-Command timeout -ErrorAction SilentlyContinue
        if ($env:OS -ne 'Windows_NT' -and $hasScript -and $hasTimeout) {
            $promptPrefix = Join-Path $tempRoot 'prefix-prompt'
            $promptEnv = New-InstallEnv -BaseUrl $normal.BaseUrl -AgentDir (Join-Path $tempRoot 'agent-prompt') -CacheDir (Join-Path $tempRoot 'cache-prompt')
            $promptEnv['RPI_INSTALL_PROMPT_TIMEOUT'] = '1'
            $promptEnv['RPI_NONINTERACTIVE'] = ''
            $promptEnv['CI'] = ''
            $promptEnv['TERM'] = 'xterm'
            $saved = @{}
            foreach ($key in $promptEnv.Keys) {
                $saved[$key] = [Environment]::GetEnvironmentVariable($key)
                [Environment]::SetEnvironmentVariable($key, [string]$promptEnv[$key])
            }
            try {
                $ptyOutput = & $pythonExe $ptyScript $shellExe -NoProfile -File $installer -Prefix $promptPrefix 2>&1 | Out-String
                $ptyCode = $LASTEXITCODE
            } finally {
                foreach ($key in $saved.Keys) {
                    [Environment]::SetEnvironmentVariable($key, $saved[$key])
                }
            }
            Check-True ($ptyCode -ne 0) 'interactive prompt timeout exits non-zero'
            Check-True ($ptyOutput -match 'timed out') 'prompt timeout is announced'
            Check-True ($ptyOutput -match 'refusing to install native plugins') 'prompt timeout refuses native consent'
            Check-True (-not (Test-Path -LiteralPath (Join-Path $promptPrefix 'rpi.exe'))) 'prompt timeout installs nothing'
        } else {
            Write-Host 'ok - PowerShell prompt timeout # SKIP (pty/script/timeout unavailable)'
        }
    } finally {
        Stop-TestServer $normal
    }
} finally {
    if ($env:RPI_KEEP_TEST_TEMP -eq '1') {
        Write-Host "kept test temp: $tempRoot"
    } else {
        Remove-Item -Recurse -Force $tempRoot -ErrorAction SilentlyContinue
    }
}

Write-Host ''
Write-Host "ran $script:Tests checks, $script:Failed failed"
if ($script:Failed -ne 0) {
    exit 1
}