//! Port of the clipboard WRITE chain — `packages/coding-agent/src/utils/
//! clipboard.ts` @ `19451accd` plus `utils/wsl.ts` (#9618 `3349e1db1` +
//! `60e7e76bd`; #9688 `6dff740fa`, V15-11 FR-A).
//!
//! Chain (upstream order, native helper excluded per D-104):
//! 1. platform commands — `pbcopy` (macOS) / `clip` (Windows) /
//!    `termux-clipboard-set` + `wl-copy` + `xclip`/`xsel` (Linux, env-gated),
//!    each bounded by a 5s timeout;
//! 2. WSL interop (#9688) — OSC 52 first inside Windows Terminal
//!    (`WT_SESSION`), else the Windows clipboard via a UTF-8 temp file +
//!    `wslpath -w` + `powershell.exe Set-Clipboard`;
//! 3. OSC 52 gating (#9618/#9688) — remote sessions (SSH/mosh) always emit;
//!    headless Linux (no `DISPLAY`/`WAYLAND_DISPLAY`/Termux) emits because
//!    the terminal is the only route; desktop sessions report the failure
//!    with platform-specific guidance instead of an unverified success.
//!
//! The read side (paste, `readClipboardText`) keeps its existing home in
//! `commands_selectors.rs` — #9618/#9688 changed writes only.
//!
//! Deviations (D-104, pre-ruled 2026-09-20): upstream's bundled native
//! clipboard helper (`getNativeClipboard`, #9163 `caf6dfe73`) is not
//! ported — no external native dependency; rpi keeps the "platform
//! commands + OSC 52" chain. Before this port rpi wrote OSC 52
//! unconditionally with no command writers at all (the pre-ruling's
//! chain description is implemented here, first evidence in §7 of the
//! task file).
//!
//! Sync/async: upstream runs the native writer and command fallbacks on
//! worker threads; rpi runs them inline under the same timeouts — the
//! established read-side precedent (`run_clipboard_command`,
//! commands_selectors.rs) blocks the drain the same bounded way.

use std::time::{Duration, Instant};

/// `MAX_OSC52_ENCODED_LENGTH` (clipboard.ts:14).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// The `runClipboardCommand` seam: `(program, args, stdin input, timeout
/// ms) -> stdout bytes` — `None` = spawn failure / timeout / non-zero
/// exit (upstream resolves `undefined`).
type RunClipboardCommand<'a> =
    &'a mut dyn FnMut(&str, &[&str], Option<&str>, u64) -> Option<Vec<u8>>;

/// Command timeout (`timeoutMs: 5000` in every `runClipboardCommand` write
/// call, clipboard.ts:96/118).
const COMMAND_TIMEOUT_MS: u64 = 5000;
/// `wslpath -w` timeout (`timeoutMs: 1000`, clipboard.ts:39).
const WSLPATH_TIMEOUT_MS: u64 = 1000;

/// Platform shape of upstream `os.platform()` for the clipboard chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardPlatform {
    Darwin,
    Windows,
    Linux,
    Other,
}

/// The environment inputs of the chain decision (upstream reads
/// `process.env` live per call; rpi snapshots the same keys per call, with
/// an override seam for tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardEnv {
    pub platform: ClipboardPlatform,
    /// `SSH_CONNECTION || SSH_CLIENT || MOSH_CONNECTION` (isRemoteSession).
    pub remote_session: bool,
    /// `TERMUX_VERSION`.
    pub termux: bool,
    /// `WAYLAND_DISPLAY`.
    pub wayland: bool,
    /// `DISPLAY`.
    pub x11: bool,
    /// `isWSL`: `WSL_DISTRO_NAME || WSLENV || /proc/version ~ microsoft|wsl`.
    pub wsl: bool,
    /// `WT_SESSION` (Windows Terminal).
    pub windows_terminal: bool,
}

impl ClipboardEnv {
    /// Live snapshot from the process environment (the production path).
    pub(crate) fn from_env() -> ClipboardEnv {
        let platform = if cfg!(target_os = "macos") {
            ClipboardPlatform::Darwin
        } else if cfg!(target_os = "windows") {
            ClipboardPlatform::Windows
        } else if cfg!(target_os = "linux") {
            ClipboardPlatform::Linux
        } else {
            ClipboardPlatform::Other
        };
        let truthy = |key: &str| std::env::var(key).is_ok_and(|value| !value.is_empty());
        ClipboardEnv {
            platform,
            remote_session: truthy("SSH_CONNECTION")
                || truthy("SSH_CLIENT")
                || truthy("MOSH_CONNECTION"),
            termux: truthy("TERMUX_VERSION"),
            wayland: truthy("WAYLAND_DISPLAY"),
            x11: truthy("DISPLAY"),
            wsl: is_wsl(truthy),
            windows_terminal: truthy("WT_SESSION"),
        }
    }
}

/// `isWSL` (wsl.ts:5-15, #9688 `6dff740fa`): env keys first, then the
/// `/proc/version` probe (`/microsoft|wsl/i`).
fn is_wsl(truthy: impl Fn(&str) -> bool) -> bool {
    if truthy("WSL_DISTRO_NAME") || truthy("WSLENV") {
        return true;
    }
    match std::fs::read_to_string("/proc/version") {
        Ok(release) => {
            let lowered = release.to_lowercase();
            lowered.contains("microsoft") || lowered.contains("wsl")
        }
        Err(_) => false,
    }
}

/// `emitOsc52` (clipboard.ts:24-30): build the OSC 52 write; `None` when
/// the encoded payload exceeds the cap (upstream returns `false`).
pub(crate) fn emit_osc52_with_cap(text: &str) -> Result<String, String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string());
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

/// The chain decision, parameterized for tests: `run_command` mirrors
/// upstream `runClipboardCommand` (`Some` = exit 0, `None` = spawn
/// failure / timeout / non-zero; `input` piped to stdin), `emit_osc52`
/// stands for the terminal write (`false` = oversized).
pub(crate) fn copy_to_clipboard_via(
    text: &str,
    env: &ClipboardEnv,
    run_command: RunClipboardCommand<'_>,
    emit_osc52: &mut dyn FnMut(&str) -> bool,
) -> Result<(), String> {
    let mut copied = false;
    // Upstream builds the command list up front (clipboard.ts:88-99); on
    // Linux the entries are env-gated. `Other` platforms take the same
    // `else` branch but none of the three env flags is set, so the list is
    // empty and the chain falls through to OSC 52 / the generic error.
    let commands: Vec<(&str, Vec<&str>)> = match env.platform {
        ClipboardPlatform::Darwin => vec![("pbcopy", vec![])],
        ClipboardPlatform::Windows => vec![("clip", vec![])],
        ClipboardPlatform::Linux | ClipboardPlatform::Other => {
            let mut commands = Vec::new();
            if env.termux {
                commands.push(("termux-clipboard-set", vec![]));
            }
            if env.wayland {
                commands.push(("wl-copy", vec![]));
            }
            if env.x11 {
                commands.push(("xclip", vec!["-selection", "clipboard"]));
                commands.push(("xsel", vec!["--clipboard", "--input"]));
            }
            commands
        }
    };
    for (program, args) in &commands {
        if run_command(program, args, Some(text), COMMAND_TIMEOUT_MS).is_some() {
            copied = true;
            break;
        }
    }

    // WSL interop (#9688): Windows Terminal supports OSC 52 — prefer it
    // over the slower PowerShell round trip.
    let mut osc52_emitted = false;
    if !copied && env.platform == ClipboardPlatform::Linux && env.wsl {
        if env.windows_terminal {
            osc52_emitted = emit_osc52(text);
        }
        copied = osc52_emitted || copy_via_windows_clipboard(text, run_command);
    }

    // OSC 52 cannot be verified, so a desktop session with a display
    // reports the failure instead (#9618). Without a display the terminal
    // is the only clipboard route (containers, WSL without WSLg), and
    // remote sessions always emit to reach the client clipboard (#9688).
    let headless =
        env.platform == ClipboardPlatform::Linux && !env.x11 && !env.wayland && !env.termux;
    let mut oversized = false;
    if !osc52_emitted && (env.remote_session || (!copied && headless)) {
        if emit_osc52(text) {
            copied = true;
        } else {
            oversized = true;
        }
    }
    if copied {
        return Ok(());
    }
    if oversized {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string());
    }
    if env.platform == ClipboardPlatform::Linux {
        if env.termux {
            return Err(
                "Clipboard unavailable: install the Termux:API app and `termux-api` package"
                    .to_string(),
            );
        }
        if env.wayland {
            return Err(
                "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                    .to_string(),
            );
        }
        if env.x11 {
            return Err(
                "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".to_string(),
            );
        }
    }
    Err("Clipboard unavailable".to_string())
}

/// `copyViaWindowsClipboard` (clipboard.ts:31-53, #9688): WSL without WSLg
/// has no Linux display, so the Windows clipboard is written through
/// interop. PowerShell reads the text from a file because `clip.exe` and
/// PowerShell stdin decode piped bytes with the console code page, which
/// mangles non-ASCII UTF-8.
fn copy_via_windows_clipboard(text: &str, run_command: RunClipboardCommand<'_>) -> bool {
    let tmp_file = std::env::temp_dir().join(format!("rpi-wsl-clip-{}.txt", wsl_clip_uuid()));
    let write = write_private_temp_file(&tmp_file, text.as_bytes());
    let result = (|| {
        write.ok()?;
        let win_path = run_command(
            "wslpath",
            &["-w", &tmp_file.display().to_string()],
            None,
            WSLPATH_TIMEOUT_MS,
        )
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|value| value.trim().to_string())?;
        if win_path.is_empty() {
            return None;
        }
        let script = format!(
            "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
            win_path.replace('\'', "''")
        );
        run_command(
            "powershell.exe",
            &["-NoProfile", "-Command", &script],
            None,
            COMMAND_TIMEOUT_MS,
        )
        .map(|_| ())
    })();
    let _ = std::fs::remove_file(&tmp_file);
    result.is_some()
}

/// `randomUUID()` stand-in (no uuid dependency; same shape as
/// `clipboard_uuid` in commands_selectors.rs).
fn wsl_clip_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Create the WSL clipboard temp file with mode 0600 ATOMICALLY on Unix
/// (upstream `writeFileSync(tmpFile, text, { mode: 0o600 })`): the previous
/// write-then-chmod left a umask-permission window (and a persistent 0644
/// file if the process died in between). Windows mounts have no POSIX mode.
fn write_private_temp_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// `runClipboardCommand` (clipboard-command.ts) for the write path.
/// Stdio policy (upstream): a call WITHOUT stdin input is a query —
/// stdout is piped and the bytes are returned; a call WITH input is a
/// writer (`clipboard writers can daemonize — do not give them output
/// pipes to retain`) — stdout is ignored entirely, so a daemonized
/// writer's inherited handle can never hang the drain, and the success
/// return is empty bytes. `None` = spawn failure / timeout / non-zero
/// exit.
pub(crate) fn run_clipboard_write_command(
    program: &str,
    args: &[&str],
    input: Option<&str>,
    timeout_ms: u64,
) -> Option<Vec<u8>> {
    let is_query = input.is_none();
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(if is_query {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        })
        .stdout(if is_query {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // The writer may exit before consuming all input (stdin errors are
    // ignored, upstream: `child.stdin?.on("error", () => {})`). The
    // write runs on its own thread so the timeout loop stays in charge:
    // a wedged writer that stops reading stdin cannot park the UI thread
    // on a full pipe — after the kill, the broken pipe fails the write
    // and the thread exits (the join is skipped on the timeout path).
    if let Some(text) = input {
        let mut stdin = child.stdin.take();
        let bytes = text.as_bytes().to_vec();
        // Detached on purpose (upstream never awaits the write).
        std::thread::spawn(move || {
            use std::io::Write;
            if let Some(mut stdin) = stdin.take() {
                let _ = stdin.write_all(&bytes);
                let _ = stdin.flush();
            }
        });
    }
    // Query path: drain stdout on a reader thread (the read-side
    // precedent in commands_selectors.rs). The command has already
    // exited by the time we join, and queries never daemonize, so the
    // join cannot outlive the timeout loop.
    let reader = if is_query {
        let mut stdout = child.stdout.take()?;
        Some(std::thread::spawn(move || {
            use std::io::Read;
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            bytes
        }))
    } else {
        None
    };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // The stdin writer stays DETACHED (upstream never awaits it): after
    // the child exits the write either completes or fails on the broken
    // pipe — its outcome is irrelevant to the caller either way. Only the
    // QUERY reader is joined, and only on the success path (the child has
    // exited, so the pipe closes promptly).
    status.filter(|status| status.success()).map(|_| {
        reader
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default()
    })
}

/// Test-injectable command runner (boxed for storage in the UI state;
/// `clipboard_env_override` precedent). Production code leaves it `None`.
pub(crate) type ClipboardRunnerOverride =
    Box<dyn FnMut(&str, &[&str], Option<&str>, u64) -> Option<Vec<u8>> + Send>;

/// The production entry: live environment snapshot, real command runner,
/// OSC 52 delivered through `write_osc52` (the caller's terminal handle).
pub(crate) fn copy_to_clipboard(
    text: &str,
    env: &ClipboardEnv,
    write_osc52: &mut dyn FnMut(&str),
) -> Result<(), String> {
    copy_to_clipboard_with_runner(text, env, None, write_osc52)
}

/// [`copy_to_clipboard`] with an injectable command runner (test seam):
/// `None` runs the real platform commands; `Some` lets tests exercise the
/// full chain deterministically without touching the host clipboard.
pub(crate) fn copy_to_clipboard_with_runner(
    text: &str,
    env: &ClipboardEnv,
    runner: Option<&mut ClipboardRunnerOverride>,
    write_osc52: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let mut production = |program: &str, args: &[&str], input: Option<&str>, timeout_ms: u64| {
        run_clipboard_write_command(program, args, input, timeout_ms)
    };
    let run: RunClipboardCommand<'_> = match runner {
        Some(runner) => runner,
        None => &mut production,
    };
    copy_to_clipboard_via(text, env, run, &mut |_text| {
        emit_osc52_with_cap(text)
            .map(|payload| write_osc52(&payload))
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    //! Intent ports of `test/clipboard.test.ts` @ `19451accd`
    //! (`describe("copyToClipboard")` — the matrix behind #9618/#9688),
    //! with the native-helper cases collapsed (D-104: no
    //! `getNativeClipboard`, so the chain starts at the platform commands).

    use super::*;

    fn env(platform: ClipboardPlatform) -> ClipboardEnv {
        ClipboardEnv {
            platform,
            remote_session: false,
            termux: false,
            wayland: false,
            x11: false,
            wsl: false,
            windows_terminal: false,
        }
    }

    /// Runner that records calls and answers from a per-program table.
    struct Recorder {
        calls: Vec<(String, Vec<String>)>,
        succeed: Vec<&'static str>,
        stdout: Vec<(&'static str, &'static str)>,
    }

    impl Recorder {
        fn new(succeed: &[&'static str]) -> Recorder {
            Recorder {
                calls: Vec::new(),
                succeed: succeed.to_vec(),
                stdout: Vec::new(),
            }
        }

        fn with_stdout(mut self, program: &'static str, out: &'static str) -> Recorder {
            self.stdout.push((program, out));
            self
        }

        fn run(
            &mut self,
            program: &str,
            args: &[&str],
            _input: Option<&str>,
            _t: u64,
        ) -> Option<Vec<u8>> {
            self.calls.push((
                program.to_string(),
                args.iter().map(|arg| arg.to_string()).collect(),
            ));
            if let Some((_, out)) = self.stdout.iter().find(|(name, _)| *name == program) {
                return Some(out.as_bytes().to_vec());
            }
            if self.succeed.contains(&program) {
                Some(Vec::new())
            } else {
                None
            }
        }

        fn names(&self) -> Vec<&str> {
            self.calls.iter().map(|(name, _)| name.as_str()).collect()
        }
    }

    #[test]
    fn linux_x11_command_success_skips_osc52() {
        let mut env = env(ClipboardPlatform::Linux);
        env.x11 = true;
        let mut recorder = Recorder::new(&["xclip"]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["xclip"]);
        assert_eq!(osc52, 0);
    }

    #[test]
    fn linux_x11_failure_reports_guidance_without_osc52() {
        // Regression for #9618: a desktop session with a display must not
        // report an unverified OSC 52 write as success.
        let mut env = env(ClipboardPlatform::Linux);
        env.x11 = true;
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(
            result,
            Err(
                "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".to_string()
            )
        );
        assert_eq!(recorder.names(), vec!["xclip", "xsel"]);
        assert_eq!(osc52, 0, "no OSC 52 on a desktop session");
    }

    #[test]
    fn wayland_failure_reports_wayland_guidance() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wayland = true;
        env.x11 = true;
        let mut recorder = Recorder::new(&[]);
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| true,
        );
        assert_eq!(
            result,
            Err(
                "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                    .to_string()
            )
        );
        assert_eq!(recorder.names(), vec!["wl-copy", "xclip", "xsel"]);
    }

    #[test]
    fn tries_xclip_and_xsel_after_wl_copy_fails() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wayland = true;
        env.x11 = true;
        let mut recorder = Recorder::new(&["xsel"]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["wl-copy", "xclip", "xsel"]);
        assert_eq!(osc52, 0);
    }

    #[test]
    fn remote_session_falls_back_to_osc52_when_commands_fail() {
        let mut env = env(ClipboardPlatform::Darwin);
        env.remote_session = true;
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(osc52, 1, "remote sessions always emit OSC 52");
    }

    #[test]
    fn oversized_payload_reports_the_osc52_limit() {
        let mut env = env(ClipboardPlatform::Darwin);
        env.remote_session = true;
        let mut recorder = Recorder::new(&[]);
        let text = "x".repeat(80_000);
        let result = copy_to_clipboard_via(
            &text,
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| false,
        );
        assert_eq!(
            result,
            Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string())
        );
    }

    #[test]
    fn headless_linux_falls_back_to_osc52() {
        // Regression for #9688: containers without X11/Wayland access.
        let env = env(ClipboardPlatform::Linux);
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert!(recorder.calls.is_empty(), "no commands on headless Linux");
        assert_eq!(osc52, 1);
    }

    #[test]
    fn wsl_without_display_writes_windows_clipboard_through_powershell() {
        // Regression for #9688: WSL with WSLg disabled.
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        let mut recorder = Recorder::new(&["powershell.exe"])
            .with_stdout("wslpath", "\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt\n");
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "héllo",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["wslpath", "powershell.exe"]);
        assert_eq!(osc52, 0);
        let (_, wslpath_args) = &recorder.calls[0];
        assert!(
            wslpath_args[1].contains("rpi-wsl-clip-"),
            "tmp file in args"
        );
        assert!(
            !std::path::Path::new(&wslpath_args[1]).exists(),
            "tmp file removed after the write"
        );
        let (_, powershell_args) = &recorder.calls[1];
        assert!(powershell_args[2].contains("Set-Clipboard"));
        assert!(powershell_args[2].contains("'\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt'"));
    }

    #[test]
    fn wsl_falls_back_to_osc52_when_interop_is_unavailable() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["wslpath"], "interop failed once");
        assert_eq!(osc52, 1);
    }

    #[test]
    fn wsl_in_windows_terminal_prefers_osc52_over_powershell() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        env.windows_terminal = true;
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert!(recorder.calls.is_empty());
        assert_eq!(osc52, 1);
    }

    #[test]
    fn wsl_in_windows_terminal_uses_powershell_for_oversized_payloads() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        env.windows_terminal = true;
        let mut recorder =
            Recorder::new(&["powershell.exe"]).with_stdout("wslpath", "C:\\clip.txt");
        let text = "x".repeat(80_000);
        let mut osc52_attempts = 0;
        let result = copy_to_clipboard_via(
            &text,
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52_attempts += 1;
                false
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["wslpath", "powershell.exe"]);
        assert_eq!(osc52_attempts, 1, "OSC 52 attempted once (oversized)");
    }

    #[test]
    fn wsl_with_a_display_prefers_linux_clipboard_tools() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        env.wayland = true;
        let mut recorder = Recorder::new(&["wl-copy"]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["wl-copy"]);
        assert_eq!(osc52, 0);
    }

    #[test]
    fn termux_failure_reports_termux_guidance() {
        let mut env = env(ClipboardPlatform::Linux);
        env.termux = true;
        let mut recorder = Recorder::new(&[]);
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| true,
        );
        assert_eq!(
            result,
            Err(
                "Clipboard unavailable: install the Termux:API app and `termux-api` package"
                    .to_string()
            )
        );
        assert_eq!(recorder.names(), vec!["termux-clipboard-set"]);
    }

    #[test]
    fn desktop_linux_without_display_tools_reports_no_display() {
        let env = env(ClipboardPlatform::Other);
        let result = copy_to_clipboard_via("hello", &env, &mut |_, _, _, _| None, &mut |_| false);
        assert_eq!(result, Err("Clipboard unavailable".to_string()));
    }

    #[test]
    fn darwin_pbcopy_success_skips_osc52() {
        let env = env(ClipboardPlatform::Darwin);
        let mut recorder = Recorder::new(&["pbcopy"]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(recorder.names(), vec!["pbcopy"]);
        assert_eq!(osc52, 0);
    }

    #[test]
    fn remote_osc52_emits_once_even_in_wsl_windows_terminal() {
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        env.windows_terminal = true;
        env.remote_session = true;
        let mut recorder = Recorder::new(&[]);
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "hello",
            &env,
            &mut |p, a, i, t| recorder.run(p, a, i, t),
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(osc52, 1, "emits exactly once");
        assert!(recorder.calls.is_empty());
    }

    // ------------------------------------------------------------------
    // Real-runner regressions (V15-11 review): the mock `Recorder` answers
    // from a table and can never catch a broken production runner — these
    // run `run_clipboard_write_command` against real executables.
    // ------------------------------------------------------------------

    #[cfg(unix)]
    fn executable_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    #[cfg(unix)]
    #[test]
    fn wsl_temp_file_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(format!("rpi-wsl-mode-test-{unique}.txt"));
        let _ = std::fs::remove_file(&path);
        write_private_temp_file(&path, b"secret").expect("write temp file");
        let mode = std::fs::metadata(&path)
            .expect("stat temp file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the WSL temp file must be 0600 from open");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_query_returns_stdout_bytes() {
        // The `wslpath -w` leg depends on this: a query call (no stdin
        // input) must return the command's real stdout.
        let dir = std::env::temp_dir().join(format!("rpi-clip-query-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let printf = executable_script(
            &dir,
            "printf-path",
            "#!/bin/sh\nprintf '\\\\wsl.localhost\\\\Ubuntu\\\\tmp\\\\clip.txt'\n",
        );
        let out = run_clipboard_write_command(
            printf.to_str().unwrap(),
            &["-w", "/tmp/clip.txt"],
            None,
            5_000,
        )
        .expect("query succeeds");
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            "\\wsl.localhost\\Ubuntu\\tmp\\clip.txt",
            "the query path must surface the real stdout bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_write_path_ignores_stdout_and_succeeds() {
        // A writer call (stdin input present) must not retain an output
        // pipe (daemonizing writers) and returns empty bytes on success.
        // The grandchild `sleep` INHERITS any retained stdout pipe: if the
        // runner regressed to piping + draining writer stdout, the read
        // would hang on the grandchild's handle and blow the time budget.
        let dir = std::env::temp_dir().join(format!("rpi-clip-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cat = executable_script(
            &dir,
            "cat-writer",
            "#!/bin/sh\ncat >/dev/null\n(sleep 20 &)\nexit 0\n",
        );
        let start = std::time::Instant::now();
        let out =
            run_clipboard_write_command(cat.to_str().unwrap(), &[], Some("hello clipboard"), 5_000)
                .expect("write succeeds");
        assert!(out.is_empty(), "the write path returns no stdout");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(4),
            "a retained stdout pipe would hang on the grandchild's handle (elapsed {:?})",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_timeout_returns_none() {
        let dir = std::env::temp_dir().join(format!("rpi-clip-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let sleep = executable_script(&dir, "sleep-forever", "#!/bin/sh\nsleep 30\n");
        let out = run_clipboard_write_command(sleep.to_str().unwrap(), &[], None, 150);
        assert!(out.is_none(), "timeout kills the command and returns None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn wsl_interop_runs_the_real_powershell_chain() {
        // End-to-end P0 regression: fake `wslpath` + recording
        // `powershell.exe` executed through the REAL production runner —
        // when the runner drops stdout, `win_path` comes back empty, the
        // PowerShell leg never runs, and this test goes red.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = std::env::temp_dir().join(format!("rpi-clip-wsl-{unique}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let args_file = dir.join("powershell-args.txt");
        let wslpath = executable_script(
            &dir,
            "wslpath",
            "#!/bin/sh\nprintf '\\\\wsl.localhost\\\\Ubuntu\\\\tmp\\\\clip.txt'\n",
        );
        let powershell = executable_script(
            &dir,
            "powershell.exe",
            &format!(
                "#!/bin/sh\nprintf '%s\\\
' \"$*\" > '{}'\nexit 0\n",
                args_file.display()
            ),
        );
        let mut env = env(ClipboardPlatform::Linux);
        env.wsl = true;
        let mut osc52 = 0;
        let result = copy_to_clipboard_via(
            "héllo",
            &env,
            &mut |program, args, input, timeout_ms| {
                let resolved = match program {
                    "wslpath" => wslpath.clone(),
                    "powershell.exe" => powershell.clone(),
                    other => std::path::PathBuf::from(other),
                };
                run_clipboard_write_command(resolved.to_str().unwrap(), args, input, timeout_ms)
            },
            &mut |_| {
                osc52 += 1;
                true
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(osc52, 0, "the PowerShell chain succeeded — no OSC 52");
        let recorded = std::fs::read_to_string(&args_file)
            .expect("powershell.exe ran (empty wslpath stdout would skip it)");
        assert!(recorded.contains("Set-Clipboard"));
        assert!(recorded.contains("'\\wsl.localhost\\Ubuntu\\tmp\\clip.txt'"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
