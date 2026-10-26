//! tmux keyboard-setup check — port of `checkTmuxKeyboardSetup`
//! (`packages/coding-agent/src/modes/interactive/interactive-mode.ts:
//! 1281-1326` @ `4a080252f`, call site `:1180-1185`).
//!
//! Upstream runs the check asynchronously at interactive startup and warns
//! once per startup when the tmux configuration mangles modified Enter keys.
//! The process plumbing differs from upstream only in the spawn mechanism
//! ([`super::commands_selectors::run_clipboard_command`], the established
//! read-side subprocess pattern); the gate, the parallel queries, the
//! 2000ms timeout, the exact warning strings, and the silent fallback
//! semantics are ported verbatim (#65).

use std::time::Duration;

/// Upstream `setTimeout(..., 2000)` (interactive-mode.ts:1290-1293).
pub(crate) const TMUX_CHECK_TIMEOUT: Duration = Duration::from_millis(2000);

/// Verbatim upstream warning (`interactive-mode.ts:1318` @ `4a080252f`).
pub(crate) const EXTENDED_KEYS_OFF_WARNING: &str = "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux.";

/// Verbatim upstream warning (`interactive-mode.ts:1322` @ `4a080252f`).
pub(crate) const EXTENDED_KEYS_FORMAT_WARNING: &str = "tmux extended-keys-format is xterm. Pi works best with csi-u. Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux.";

/// `checkTmuxKeyboardSetup` (interactive-mode.ts:1281-1326 @ `4a080252f`):
/// `TMUX` gate, the two `tmux show -gv` queries in parallel, and the
/// misconfiguration warnings. `None` = don't warn (no tmux, timeout,
/// spawn failure, non-zero exit, or a correct configuration). A successful
/// query with empty output is a misconfiguration like upstream
/// (`code === 0 ? stdout.trim() : undefined`, :1304; guard :1317, warn :1318).
pub(crate) fn check_tmux_keyboard_setup(timeout: Duration) -> Option<String> {
    // `if (!process.env.TMUX) return undefined` (interactive-mode.ts:1282):
    // an empty value is falsy in JS.
    let in_tmux = std::env::var_os("TMUX").is_some_and(|value| !value.is_empty());
    check_tmux_keyboard_setup_with(in_tmux, timeout, &run_show_command)
}

/// The production command runner (`spawn`, `stdio: ["ignore", "pipe",
/// "ignore"]`, interactive-mode.ts:1285-1286): reused from the clipboard
/// read-side helper, which captures stdout with the same bounded
/// try-wait loop and ignores stderr.
fn run_show_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    super::commands_selectors::run_clipboard_command(program, args, timeout)
}

/// The tmux query runner seam (`(program, args, timeout) -> stdout bytes`),
/// injected in tests; production passes
/// [`super::commands_selectors::run_clipboard_command`]. `Sync` because the
/// two queries run on scoped threads.
type TmuxQueryRunner<'a> = &'a (dyn Fn(&str, &[&str], Duration) -> Option<Vec<u8>> + Sync);

/// [`check_tmux_keyboard_setup`] with the environment gate and the command
/// runner injected for tests. The two queries run in parallel
/// (`Promise.all`, interactive-mode.ts:1309-1312); each side gets its own
/// timeout, so a stuck `tmux` cannot extend the other query's budget.
fn check_tmux_keyboard_setup_with(
    in_tmux: bool,
    timeout: Duration,
    run: TmuxQueryRunner<'_>,
) -> Option<String> {
    if !in_tmux {
        return None;
    }

    let (extended_keys, extended_keys_format) = std::thread::scope(|scope| {
        let keys = scope.spawn(|| show_option("extended-keys", timeout, run));
        let format = scope.spawn(|| show_option("extended-keys-format", timeout, run));
        (keys.join().unwrap_or(None), format.join().unwrap_or(None))
    });

    // If we couldn't query tmux (timeout, sandbox, etc.), don't warn
    // (interactive-mode.ts:1314-1315).
    let extended_keys = extended_keys?;

    if extended_keys != "on" && extended_keys != "always" {
        return Some(EXTENDED_KEYS_OFF_WARNING.to_string());
    }

    if extended_keys_format.as_deref() == Some("xterm") {
        return Some(EXTENDED_KEYS_FORMAT_WARNING.to_string());
    }

    None
}

/// `runTmuxShow` (interactive-mode.ts:1284-1306): `tmux show -gv <option>`,
/// stdout trimmed, `None` on spawn failure / timeout / non-zero exit.
fn show_option(option: &str, timeout: Duration, run: TmuxQueryRunner<'_>) -> Option<String> {
    let bytes = run("tmux", &["show", "-gv", option], timeout)?;
    Some(String::from_utf8_lossy(&bytes).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Runner answering per option name; `None` = query failure.
    struct Stub {
        extended_keys: Option<&'static str>,
        extended_keys_format: Option<&'static str>,
        calls: Mutex<Vec<String>>,
        delay: Duration,
    }

    impl Stub {
        fn new(keys: Option<&'static str>, format: Option<&'static str>) -> Stub {
            Stub {
                extended_keys: keys,
                extended_keys_format: format,
                calls: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn slow(mut self, delay: Duration) -> Stub {
            self.delay = delay;
            self
        }

        fn run(&self, program: &str, args: &[&str], _timeout: Duration) -> Option<Vec<u8>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{program} {}", args.join(" ")));
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let option = args.last().copied().unwrap_or_default();
            let value = match option {
                "extended-keys" => self.extended_keys,
                "extended-keys-format" => self.extended_keys_format,
                _ => None,
            };
            value.map(|value| format!("{value}\n").into_bytes())
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    /// The two upstream strings inlined byte-for-byte with their source
    /// anchors; the constants must never drift from them.
    #[test]
    fn warnings_match_upstream_verbatim() {
        // interactive-mode.ts:1318 @ 4a080252f.
        let off = "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux.";
        // interactive-mode.ts:1322 @ 4a080252f.
        let format = "tmux extended-keys-format is xterm. Pi works best with csi-u. Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux.";
        assert_eq!(EXTENDED_KEYS_OFF_WARNING, off);
        assert_eq!(EXTENDED_KEYS_FORMAT_WARNING, format);
    }

    #[test]
    fn silent_outside_tmux_without_running_queries() {
        let calls = AtomicUsize::new(0);
        let run = |_: &str, _: &[&str], _: Duration| {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        };
        let warning = check_tmux_keyboard_setup_with(false, Duration::from_millis(50), &run);
        assert_eq!(warning, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no tmux call outside tmux");
    }

    #[test]
    fn warns_when_extended_keys_is_off() {
        let stub = Stub::new(Some("off"), Some("csi-u"));
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning.as_deref(), Some(EXTENDED_KEYS_OFF_WARNING));
        let mut calls = stub.calls();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                "tmux show -gv extended-keys".to_string(),
                "tmux show -gv extended-keys-format".to_string(),
            ]
        );
    }

    #[test]
    fn accepts_on_and_always() {
        for value in ["on", "always"] {
            let stub = Stub::new(Some(value), Some("csi-u"));
            let warning =
                check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                    stub.run(p, a, t)
                });
            assert_eq!(warning, None, "extended-keys {value} is accepted");
        }
    }

    #[test]
    fn warns_when_extended_keys_format_is_xterm() {
        let stub = Stub::new(Some("on"), Some("xterm"));
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning.as_deref(), Some(EXTENDED_KEYS_FORMAT_WARNING));
    }

    #[test]
    fn trims_the_query_output() {
        let stub = Stub::new(Some(" on \n"), Some(" csi-u \n"));
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning, None);
    }

    #[test]
    fn silent_when_extended_keys_query_fails() {
        let stub = Stub::new(None, Some("xterm"));
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning, None, "don't warn when the query failed");
    }

    /// Upstream `code === 0 ? stdout.trim() : undefined`
    /// (interactive-mode.ts:1304) plus the `!== "on" && !== "always"` check
    /// (`:1317`): a successful query whose output is empty is not an
    /// accepted value, so it warns. (Review B1: the task §4 "空输出→无警告"
    /// wording contradicted upstream; the implementation follows upstream
    /// and this test pins it.)
    #[test]
    fn warns_when_the_successful_query_output_is_empty() {
        let stub = Stub::new(Some(""), Some("csi-u"));
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning.as_deref(), Some(EXTENDED_KEYS_OFF_WARNING));
    }

    #[test]
    fn silent_when_only_the_format_query_misses() {
        let stub = Stub::new(Some("on"), None);
        let warning =
            check_tmux_keyboard_setup_with(true, Duration::from_millis(50), &|p, a, t| {
                stub.run(p, a, t)
            });
        assert_eq!(warning, None, "format undefined !== \"xterm\"");
    }

    #[test]
    fn probes_run_in_parallel() {
        // Each query blocks for 1s; a sequential implementation would take
        // >= 2s while `Promise.all` (interactive-mode.ts:1309-1312) waits
        // only for the slower one.
        let stub = Stub::new(Some("on"), Some("csi-u")).slow(Duration::from_secs(1));
        let start = Instant::now();
        let warning = check_tmux_keyboard_setup_with(true, Duration::from_secs(2), &|p, a, t| {
            stub.run(p, a, t)
        });
        let elapsed = start.elapsed();
        assert_eq!(warning, None);
        assert!(
            elapsed < Duration::from_millis(1900),
            "the two queries must overlap (elapsed {elapsed:?})"
        );
    }
}
