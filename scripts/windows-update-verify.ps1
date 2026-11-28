<#
.SYNOPSIS
    Windows verification driver for the startup self-update (V19-09, ADR-0050).

.DESCRIPTION
    Drives the acceptance matrix of rpi-docs plan/v0.1.9/V19-09 section 5 on
    a real Windows machine. The runtime behavior of the update was developed
    without Windows, so every phase here both performs the automatable steps
    and asserts the on-disk state the design promises. Phases that need a
    person at the keyboard (an interactive start, a kill from Task Manager)
    print the exact steps and then verify the outcome.

    Run it from PowerShell (7 or 5.1) on a machine that has rpi installed:
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -List
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase baseline
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase stage
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase switched
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase extensions
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase rollback
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase cleanup
      powershell -ExecutionPolicy Bypass -File scripts\windows-update-verify.ps1 -Phase all

    Each phase prints "ok N - ..." / "not ok N - ..." lines and a count at the
    end; the exit code is 1 when a check failed. A transcript of every run is
    appended to %LOCALAPPDATA%\rpi\verify\verify-<timestamp>.log. The script
    only reads rpi's files and runs rpi's own commands; it never deletes or
    rewrites anything itself.

.PARAMETER Phase
    baseline   - the installed state before any update (version, layout, no update state).
    stage      - runs `rpi update --self --rc` (or --version) and checks the staged state.
    switched   - after the first start of the new version: version, result state, old image.
    extensions - runs `rpi update --all --rc` / `--extensions` and checks the generational layout.
    rollback   - after a forced first-start crash: previous version back, block list, messages.
    cleanup    - `rpi update --cleanup` exit codes and the removed leftovers.
    all        - every phase in order (the interactive steps still pause for input).

.PARAMETER FromVersion
    The version installed before the update (default: what `rpi --version`
    prints at the start of the baseline phase).

.PARAMETER ToVersion
    The version to update to (default: the version of this source tree, read
    from Cargo.toml).

.PARAMETER Channel
    stable or rc (default rc: the channel that serves the candidate).

.PARAMETER List
    Print the matrix with the phase that covers each item and exit.
#>
[CmdletBinding()]
param(
    [ValidateSet('baseline', 'stage', 'switched', 'extensions', 'rollback', 'cleanup', 'all')]
    [string]$Phase = 'all',
    [string]$FromVersion = '',
    [string]$ToVersion = '',
    [ValidateSet('stable', 'rc')]
    [string]$Channel = 'rc',
    [switch]$List
)

$ErrorActionPreference = 'Stop'
$script:Tests = 0
$script:Failed = 0
$script:Transcript = $null

function Check-True {
    param([bool]$Condition, [string]$Name)
    $script:Tests++
    if ($Condition) {
        Write-Host "ok $script:Tests - $Name"
        Add-Log "ok $script:Tests - $Name"
    } else {
        Write-Host "not ok $script:Tests - $Name"
        Add-Log "not ok $script:Tests - $Name"
        $script:Failed++
    }
}

function Check-Equal {
    param([string]$Expected, [string]$Actual, [string]$Name)
    $script:Tests++
    if ([string]::Equals($Expected, $Actual, [System.StringComparison]::Ordinal)) {
        Write-Host "ok $script:Tests - $Name"
        Add-Log "ok $script:Tests - $Name"
    } else {
        Write-Host "not ok $script:Tests - $Name"
        Write-Host "    expected: [$Expected]"
        Write-Host "    actual:   [$Actual]"
        Add-Log "not ok $script:Tests - $Name (expected [$Expected], actual [$Actual])"
        $script:Failed++
    }
}

function Add-Log {
    param([string]$Line)
    if ($script:Transcript) {
        [IO.File]::AppendAllText($script:Transcript, $Line + "`r`n", (New-Object System.Text.UTF8Encoding $false))
    }
}

function Say {
    param([string]$Text)
    Write-Host ''
    Write-Host $Text
    Add-Log "## $Text"
}

function Read-JsonFile {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    try { return (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json) } catch { return $null }
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# ---- the matrix ------------------------------------------------------------

function Show-Matrix {
    $rows = @(
        @('rename the running exe and place the new one; unsupported path degrades to manual steps', 'stage + switched (unsupported path: see install.ps1 preflight)'),
        @('a directory with a loaded DLL can be neither renamed nor deleted', 'windows-tests workflow: a_directory_with_a_loaded_library_is_neither_renamable_nor_deletable'),
        @('pointer atomic write leaves no half state under fault injection', 'Linux unit tests: generations::pointer_flip_retries_transient_failures_and_leaves_no_temp_file'),
        @('two instances coexist; old instance unaffected; subagent child starts no transaction', 'extensions + manual (keep one rpi open)'),
        @('trampoline: prompt returns after the new instance exits; exit code; quoted args; kill; Ctrl+C', 'switched (automated exit code) + manual in cmd/PowerShell/Windows Terminal/VS Code'),
        @('exe gap between the two renames', 'manual: see the "two renames" note printed by the rollback phase'),
        @('pipe modes: rpi --mode rpc / --mode json through the trampoline', 'manual (switched phase prints the commands)'),
        @('first-start watchdog: --version / --help / argument error / failing one-shot / Ctrl+C / crash', 'rollback (+ switched for the non-crash counter-cases)'),
        @('--force downgrade and --version pin apply at the next start; a re-run installer invalidates the pending', 'manual: stage with -ToVersion <older> -Channel stable --force... see the note'),
        @('target below 0.1.9-rc.4 switches without the first-start check', 'manual (only when an older RC asset exists)'),
        @('re-stage the rolled-back version with --force; resume an interrupted rollback', 'rollback + manual'),
        @('three cleanup notices; update --cleanup', 'switched (reminder/in-use) + cleanup'),
        @('pre-rc.4 instance running during GC: its old generation is neither renamed nor half-deleted (live by the refused rename, in-use warning)', 'extensions + manual (keep a pre-rc.4 rpi open; update.log says the rename was refused; the next start after it exits cleans up)'),
        @('existing flat installs keep loading; update --extensions migrates; old dir cleaned', 'extensions'),
        @('install.ps1: fresh generational install; parked-exe preflight; re-run with rpi closed', 'manual (installer prompts)'),
        @('Defender interference', 'manual: note any "still in use"/"access denied" warning and the update.log lines'),
        @('startup cost against the budget', 'manual: Measure-Command rpi --version, 30 runs, before/after'),
        @('Linux/macOS zero change', 'Linux gates of the V19-09 evidence')
    )
    Write-Host 'V19-09 section 5 matrix - item -> phase / evidence'
    foreach ($row in $rows) {
        Write-Host ('  - ' + $row[0])
        Write-Host ('      ' + $row[1])
    }
}

# ---- phases ----------------------------------------------------------------

function Invoke-Baseline {
    Say 'Phase baseline: the installed state before the update'
    $version = Get-RpiVersion
    if (-not $FromVersion) { $FromVersion = $version }
    Check-True ([bool]$version) 'rpi --version prints a version'
    Write-Host "  installed: $version (prefix $($script:Prefix))"
    $manifest = Read-JsonFile (Join-Path $script:Prefix 'rpi.install.json')
    Check-True ($null -ne $manifest) 'rpi.install.json exists next to rpi.exe'
    if ($manifest) {
        Check-Equal $version ([string]$manifest.version) 'the manifest names the installed version'
    }
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'pending.json'))) 'no pending update is staged'
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'undo.json'))) 'no undo.json is left behind'
    $state = Read-JsonFile (Join-Path $script:UpdateDir 'update-result.json')
    if ($state) {
        Write-Host ("  update-result state: " + $state.state)
        Check-True ($state.state -ne 'applied') 'no update waits for its first start'
    }
    $parked = @(Get-ChildItem -LiteralPath $script:Prefix -File -Filter 'rpi.exe.old-*' -ErrorAction SilentlyContinue)
    Check-Equal '0' ([string]$parked.Count) 'no parked rpi.exe.old-* image exists'
    if (Test-Path -LiteralPath $script:Extensions) {
        $containers = @(Get-ChildItem -LiteralPath $script:Extensions -Directory | Where-Object { -not $_.Name.StartsWith('.') })
        $flat = @($containers | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'rpi-extension.json') })
        $generational = @($containers | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'current.json') })
        Write-Host ("  extensions: " + $containers.Count + " container(s), " + $flat.Count + " flat, " + $generational.Count + " generational")
    }
    $listing = Invoke-Rpi @('list')
    Write-Host $listing.Output
}

function Invoke-Stage {
    Say "Phase stage: rpi update --self $($script:ChannelFlag -join ' ') (requires an installed rpi of at least 0.1.9-rc.4 and a target newer than it: -ToVersion <version>; an older installed rpi has no staging code - install 0.1.9-rc.4 with install.ps1 first)"
    $before = Get-Sha256 $script:Exe
    $result = Invoke-Rpi (@('update', '--self') + $script:ChannelFlag)
    Write-Host $result.Output
    Check-Equal '0' ([string]$result.ExitCode) 'rpi update --self exits 0'
    Check-True ($result.Output -match ('Downloaded rpi ' + [regex]::Escape($ToVersion) + '\. It takes effect the next time rpi starts\.')) 'the success line names the next start'
    Check-Equal $before (Get-Sha256 $script:Exe) 'rpi.exe is byte-for-byte unchanged'
    $pending = Read-JsonFile (Join-Path $script:UpdateDir 'pending.json')
    Check-True ($null -ne $pending) '.update\pending.json exists'
    if ($pending) {
        Check-Equal $ToVersion ([string]$pending.targetVersion) 'pending names the target version'
        Check-Equal $script:FromVersion ([string]$pending.fromVersion) 'pending names the installed version'
        Check-True (-not [bool]$pending.applied) 'pending is not applied yet'
        Check-Equal ($(if ($script:Rc) { 'rc' } else { 'stable' })) ([string]$pending.channel) 'pending records the channel'
        Check-True (Test-Path -LiteralPath ([string]$pending.binary.stagedPath)) 'the staged executable exists'
        Check-Equal ([string]$pending.binary.sha256) (Get-Sha256 ([string]$pending.binary.stagedPath)) 'the staged executable matches its digest'
    }
    Check-Equal $script:FromVersion (Get-RpiVersion) 'rpi --version still prints the installed version'
    $manifest = Read-JsonFile (Join-Path $script:Prefix 'rpi.install.json')
    if ($manifest) { Check-Equal $script:FromVersion ([string]$manifest.version) 'the install manifest is untouched' }
    $state = Read-JsonFile (Join-Path $script:UpdateDir 'update-result.json')
    if ($state) { Check-Equal 'staged' ([string]$state.state) 'update-result is staged' }
    Check-True (Test-Path -LiteralPath $script:UpdateLog) 'update.log exists'
    # Idempotent second run: nothing is downloaded again.
    $again = Invoke-Rpi (@('update', '--self') + $script:ChannelFlag)
    Write-Host $again.Output
    Check-Equal '0' ([string]$again.ExitCode) 'a second rpi update --self exits 0'
    Check-True ($again.Output -match 'already downloaded') 'the second run reports the staged update'
    Say 'Interactive step: start rpi now from cmd, PowerShell, Windows Terminal and the VS Code terminal (one after another). Expect: the new version starts, the prompt returns only when rpi exits, Ctrl+C reaches the new instance only. For the TUI the first start shows "Updated rpi <old> -> <new>. ..." in the startup notice. Then run: -Phase switched'
}

function Invoke-Switched {
    Say 'Phase switched: the new version after its first start'
    $version = Get-RpiVersion
    Check-Equal $ToVersion $version 'rpi --version prints the new version'
    $result = Invoke-Rpi @('--version')
    Check-Equal '0' ([string]$result.ExitCode) 'the exit code passes through (0)'
    $bad = Invoke-Rpi @('--definitely-not-a-flag')
    Check-True ($bad.ExitCode -ne 0) 'an argument error exits non-zero through the trampoline'
    $manifest = Read-JsonFile (Join-Path $script:Prefix 'rpi.install.json')
    if ($manifest) { Check-Equal $ToVersion ([string]$manifest.version) 'the install manifest names the new version' }
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'pending.json'))) 'pending.json is gone (confirmed)'
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'undo.json'))) 'undo.json is gone (confirmed)'
    $state = Read-JsonFile (Join-Path $script:UpdateDir 'update-result.json')
    if ($state) { Check-Equal 'confirmed' ([string]$state.state) 'update-result is confirmed' }
    $boot = @(Get-ChildItem -LiteralPath (Join-Path $script:UpdateDir 'boot') -File -ErrorAction SilentlyContinue)
    Check-Equal '0' ([string]$boot.Count) 'no boot record remains'
    # --version confirmed and exited normally, so the next start removed the old image.
    $parked = @(Get-ChildItem -LiteralPath $script:Prefix -File -Filter 'rpi.exe.old-*' -ErrorAction SilentlyContinue)
    Check-Equal '0' ([string]$parked.Count) 'the previous rpi.exe.old-* image was removed after the first normal exit'
    $pins = Join-Path $script:Extensions '.pins'
    if (Test-Path -LiteralPath $pins) {
        $live = @(Get-ChildItem -LiteralPath $pins -File -Filter '*.json' -ErrorAction SilentlyContinue)
        Check-Equal '0' ([string]$live.Count) 'no pin of an exited process remains'
    }
    if (Test-Path -LiteralPath $script:UpdateLog) {
        $text = Get-Content -LiteralPath $script:UpdateLog -Raw
        Check-True ($text -match 'committed') 'update.log records the commit'
        Check-True ($text -match 'confirm') 'update.log records the confirmation'
        Check-True ($text -notmatch '(?i)api[_-]?key|bearer |sk-ant') 'update.log holds no credential-looking value'
    }
    Say 'Pipe modes through the trampoline (manual, a pipe parent must not see rpi end early and stdout must carry only protocol bytes):'
    Write-Host '  rpi --mode json -p "say hi" | Select-Object -First 3'
    Write-Host '  (an RPC client) rpi --mode rpc   -- the JSON-RPC server must answer; stdout carries only protocol bytes'
}

function Invoke-Extensions {
    Say "Phase extensions: rpi update --all $($script:ChannelFlag -join ' ')"
    $result = Invoke-Rpi (@('update', '--all') + $script:ChannelFlag)
    Write-Host $result.Output
    Check-Equal '0' ([string]$result.ExitCode) 'rpi update --all exits 0'
    $list = Invoke-Rpi @('list')
    Write-Host $list.Output
    $containers = @(Get-ChildItem -LiteralPath $script:Extensions -Directory -ErrorAction SilentlyContinue | Where-Object { -not $_.Name.StartsWith('.') })
    $generational = @($containers | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'current.json') })
    Check-True ($generational.Count -gt 0) 'at least one extension resolves through a current.json pointer'
    foreach ($container in $generational) {
        $pointer = Read-JsonFile (Join-Path $container.FullName 'current.json')
        Check-Equal '1' ([string]$pointer.schemaVersion) ($container.Name + ': pointer schemaVersion is 1')
        Check-True (Test-Path -LiteralPath (Join-Path (Join-Path $container.FullName ([string]$pointer.path)) 'rpi-extension.json')) ($container.Name + ': the pointer names an existing generation')
        Check-Equal (Get-Sha256 (Join-Path (Join-Path $container.FullName ([string]$pointer.path)) 'rpi-extension.json')) ([string]$pointer.manifestSha256) ($container.Name + ': pointer manifest sha256 matches')
        Check-True ($list.Output -match [regex]::Escape($container.Name)) ($container.Name + ' is listed')
    }
    $staging = Join-Path $script:Extensions '.staging'
    Check-True (-not (Test-Path -LiteralPath $staging)) 'the .staging root is gone once nothing waits in it'
    $again = Invoke-Rpi (@('update', '--extensions') + $script:ChannelFlag)
    Write-Host $again.Output
    Check-Equal '0' ([string]$again.ExitCode) 'rpi update --extensions exits 0 when everything is current'
    Say 'Two instances (manual): keep one rpi open in a window, run "rpi update --extensions <channel> --version <a newer rc>" in another, start a second rpi and check: the second runs the new generation, the first keeps running, .pins holds one <pid>.json per running instance, and the first instance''s startup (after it restarts) no longer warns. Then run: -Phase cleanup'
}

function Invoke-Rollback {
    Say 'Phase rollback: after a forced first-start crash'
    Write-Host 'Manual steps this phase verifies:'
    Write-Host '  1. Stage an update (rpi update --self <channel flag> --version <ToVersion> --force reinstalls the current version):'
    Write-Host "       rpi update --self --version $ToVersion --force"
    Write-Host '  2. Start rpi interactively and, while the TUI is still coming up (before the first frame), end the NEWEST rpi.exe'
    Write-Host '     in Task Manager (Details, by creation time) - the child the trampoline started. Alternatively close the'
    Write-Host '     console window during the first start (the trampoline-absent path: the next start performs the rollback).'
    Write-Host '  3. Expect: "The update to rpi <ToVersion> failed and was rolled back to rpi <ToVersion>." (a same-version re-stage: <new> and <old> are the same version) and, for step 2 with Task Manager,'
    Write-Host '     the previous image starting again by itself (rpi.exe.old-<ToVersion> is restored to rpi.exe).'
    Write-Host 'Then run: -Phase rollback'
    $version = Get-RpiVersion
    Check-Equal $ToVersion $version 'rpi --version prints the restored (same) version again'
    $blocked = Read-JsonFile (Join-Path $script:UpdateDir 'blocked.json')
    Check-True ($null -ne $blocked) '.update\blocked.json exists'
    if ($blocked) { Check-True (@($blocked.versions) -contains $ToVersion) 'the failed version is on the block list' }
    $state = Read-JsonFile (Join-Path $script:UpdateDir 'update-result.json')
    if ($state) {
        Check-Equal 'rolled_back' ([string]$state.state) 'update-result is rolled_back'
        Check-Equal $ToVersion ([string]$state.fromVersion) 'result names the restored version'
    }
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'pending.json'))) 'pending.json is gone'
    Check-True (-not (Test-Path -LiteralPath (Join-Path $script:UpdateDir 'undo.json'))) 'undo.json is gone'
    $manifest = Read-JsonFile (Join-Path $script:Prefix 'rpi.install.json')
    if ($manifest) { Check-Equal $ToVersion ([string]$manifest.version) 'the install manifest is the restored one' }
    foreach ($container in @(Get-ChildItem -LiteralPath $script:Extensions -Directory -ErrorAction SilentlyContinue | Where-Object { -not $_.Name.StartsWith('.') })) {
        $pointer = Read-JsonFile (Join-Path $container.FullName 'current.json')
        if ($pointer) {
            $generation = Join-Path $container.FullName ([string]$pointer.path)
            $manifestVersion = [string](Read-JsonFile (Join-Path $generation 'rpi-extension.json')).version
            Check-Equal ([string]$pointer.version) $manifestVersion ($container.Name + ': pointer and generation agree after the rollback')
        }
    }
    # An exact pin carries its own channel; without --force a blocked version is refused.
    $refused = Invoke-Rpi @('update', '--self', '--version', $ToVersion)
    Write-Host $refused.Output
    Check-Equal '1' ([string]$refused.ExitCode) 'an update pinned to the blocked version is refused without --force'
    Check-True ($refused.Output -match 'blocked list') 'the refusal names the block list'
    Say 'Counter-cases that must NOT roll back (manual, re-stage with --force first each time): rpi --version, rpi --help, a mistyped flag, a failing one-shot command, Ctrl+C during the first TUI frame, rpi --resume with the picker cancelled (Esc). After each, rpi --version must print the new version and update-result.json must be confirmed (or still applied for the failing one-shot command).'
}

function Invoke-Cleanup {
    Say 'Phase cleanup: rpi update --cleanup'
    $result = Invoke-Rpi @('update', '--cleanup')
    Write-Host $result.Output
    $state = Read-JsonFile (Join-Path $script:UpdateDir 'update-result.json')
    $parked = @(Get-ChildItem -LiteralPath $script:Prefix -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'rpi.exe.old-*' -or $_.Name -like 'rpi.exe.bad-*' })
    if ($parked.Count -eq 0) {
        Check-Equal '0' ([string]$result.ExitCode) 'nothing left: exit 0'
        Check-True ($result.Output -match 'Nothing left to clean up') 'nothing left is reported'
    } else {
        Write-Host ("  old images present: " + ($parked | ForEach-Object { $_.Name }) -join ', ')
        Check-Equal '1' ([string]$result.ExitCode) 'files still in use or stuck: exit 1'
    }
    if ($state) {
        Check-True ($state.state -ne 'applied') 'no update waits for its first start'
    }
    Say 'With another rpi instance open (manual): rpi update --cleanup must exit 1 and report the instance count; after that instance exits the next call exits 0. rpi self-uninstall must refuse while the other instance is open.'
}

# ---- main ------------------------------------------------------------------

if ($List) {
    Show-Matrix
    exit 0
}

if ($env:OS -ne 'Windows_NT') {
    Write-Host 'This driver runs on Windows only (it inspects the Windows update layout).'
    exit 1
}

# ---- locate the installation -----------------------------------------------

$script:Rpi = (Get-Command rpi -ErrorAction SilentlyContinue | Select-Object -First 1)
if (-not $script:Rpi) {
    Write-Host 'rpi is not on PATH; start a shell where the installer added it.'
    exit 1
}
$script:Exe = [string]$script:Rpi.Source
if (-not $script:Exe) { $script:Exe = [string]$script:Rpi.Path }
$script:Prefix = Split-Path -Parent $script:Exe
$script:UpdateDir = Join-Path $script:Prefix '.update'
$script:AgentDir = if ($env:RPI_CODING_AGENT_DIR) { $env:RPI_CODING_AGENT_DIR } else { Join-Path $env:USERPROFILE '.rpi\agent' }
$script:Extensions = Join-Path $script:AgentDir 'extensions'
$script:UpdateLog = Join-Path $env:LOCALAPPDATA 'rpi\logs\update.log'

function Invoke-Rpi {
    param([string[]]$Arguments)
    # Windows PowerShell 5.1 turns redirected stderr lines into error records
    # that would stop the script under $ErrorActionPreference = 'Stop'.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $script:Exe @Arguments 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    Add-Log ("$ rpi " + ($Arguments -join ' ') + "`r`n" + $output + "(exit $code)`r`n")
    return [pscustomobject]@{ ExitCode = $code; Output = $output }
}

function Get-RpiVersion {
    $result = Invoke-Rpi @('--version')
    return ([string]$result.Output).Trim()
}

if (-not $ToVersion) {
    $cargoToml = Join-Path (Split-Path -Parent $PSScriptRoot) 'Cargo.toml'
    if (Test-Path -LiteralPath $cargoToml) {
        $match = [regex]::Match((Get-Content -LiteralPath $cargoToml -Raw), '(?m)^version\s*=\s*"([^"]+)"')
        if ($match.Success) { $ToVersion = $match.Groups[1].Value }
    }
}
if (-not $ToVersion) { $ToVersion = '0.1.9-rc.4' }
$script:Rc = ($Channel -eq 'rc')
$script:ChannelFlag = if ($script:Rc) { @('--rc') } else { @() }

$verifyDir = Join-Path $env:LOCALAPPDATA 'rpi\verify'
New-Item -ItemType Directory -Force -Path $verifyDir | Out-Null
$script:Transcript = Join-Path $verifyDir ('verify-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + $Phase + '.log')
Add-Log ("# windows-update-verify phase={0} from={1} to={2} channel={3} exe={4}" -f $Phase, $FromVersion, $ToVersion, $Channel, $script:Exe)
Write-Host ("transcript: " + $script:Transcript)

$order = @('baseline', 'stage', 'switched', 'extensions', 'rollback', 'cleanup')
$phases = if ($Phase -eq 'all') { $order } else { @($Phase) }
foreach ($name in $phases) {
    if ($name -eq 'baseline' -and -not $FromVersion) {
        # Capture the installed version before anything runs.
        $FromVersion = Get-RpiVersion
    }
    switch ($name) {
        'baseline' { Invoke-Baseline }
        'stage' { Invoke-Stage }
        'switched' { Invoke-Switched }
        'extensions' { Invoke-Extensions }
        'rollback' { Invoke-Rollback }
        'cleanup' { Invoke-Cleanup }
    }
    if ($Phase -eq 'all' -and $name -ne $order[-1]) {
        Write-Host ''
        Read-Host 'Press Enter after the manual steps of this phase (Ctrl+C to stop)'
    }
}

Write-Host ''
Write-Host "ran $script:Tests checks, $script:Failed failed"
Add-Log "ran $script:Tests checks, $script:Failed failed"
if ($script:Failed -ne 0) {
    exit 1
}
exit 0
