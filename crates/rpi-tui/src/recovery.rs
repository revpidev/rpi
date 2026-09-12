//! Terminal state recovery (coding-standards §8.5, T11 scope).
//!
//! Semantic port of the recovery paths in
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! @ pi 0.82.1 (2efa728):
//! - [`install_panic_hook`] mirrors `uncaughtCrash`
//!   (interactive-mode.ts:3613-3639): restore the terminal (`ui.stop()` in a
//!   `try {} catch {}`) BEFORE printing the error, so a panic never leaves
//!   the terminal in raw mode with a hidden cursor.
//! - [`spawn_signal_restore`] mirrors the SIGTERM/SIGHUP branch of
//!   `registerSignalHandlers` + `shutdown({ fromSignal: true })`
//!   (interactive-mode.ts:3558-3573, 3646-3663): restore the terminal, then
//!   `process.exit(0)`.
//!
//! Intentional placement difference: upstream wires these in the coding-agent
//! layer because Node signal/exception handlers are process-global callbacks
//! registered next to the event loop. This port keeps them in rpi-tui because
//! §8.5 assigns terminal restore to the TUI layer and a Rust panic hook is
//! process-global state that must capture the live [`TuiMainScreen`] handle. The
//! graceful-shutdown orchestration around the restore (extension cleanup,
//! `drainInput`, session shutdown events) stays with the interactive mode
//! (T12), exactly as upstream splits it.
//!
//! Intentional behavior differences (documented in the item docstrings):
//! - The panic hook does NOT exit the process: upstream's `process.exit(1)`
//!   follows the synchronous `console.error` output, whereas a Rust panic
//!   continues unwinding (exit code 101 on `main`) after the hook returns.
//!   Exiting from the hook would also kill the process on panics in
//!   unrelated worker threads, which Rust semantics deliberately do not.
//! - Rust addition: when the panicking thread holds the `TuiMainScreen` lock
//!   (mid-render panic), [`restore_terminal`] falls back to a fixed restore
//!   byte sequence written straight to stdout instead of deadlocking
//!   (upstream's single-threaded event loop cannot hit this case), and
//!   queues a full stop op on the `TuiMainScreen` so the event loop's next tick writes
//!   the complete restore sequence and stops rendering over it.

use std::io::{self, Write};

use crate::tui::TuiStopOptions;
use crate::tui_handle::TuiHandle;
use crate::tui_main_screen::TuiMainScreen;

/// Fixed best-effort restore sequence for the locked-TuiMainScreen fallback in
/// [`restore_terminal`], mirroring the writes of `TuiMainScreen::stop_internal` +
/// `ProcessTerminal::stop` (tui.ts:689-714, terminal.ts:396-449) in the same
/// order, minus the state-dependent parts the fallback cannot know
/// (progress keepalive clear, cursor repositioning):
/// 0. `\x1b[?1049l` — exit alternate screen (T32: the panic hook may fire
///    while the fullscreen renderer is active; this restores the main screen)
/// 1. `\x1b[?2031l` — color scheme notifications off (`TuiMainScreen::stop_internal`)
/// 2. `\x1b[?2004l` — bracketed paste off
/// 3. `\x1b[<u` — pop Kitty keyboard protocol
/// 4. `\x1b[>4;0m` — modifyOtherKeys off
/// 5. `\x1b[?25h` — show cursor
///
/// Raw mode restore (`crossterm::terminal::disable_raw_mode`) is a syscall,
/// not part of this byte sequence.
const MINIMAL_RESTORE_SEQUENCE: &str =
    "\x1b[?1049l\x1b[?2031l\x1b[?2004l\x1b[<u\x1b[>4;0m\x1b[?25h";

/// Restore the terminal via `tui.stop()`; fall back to a fixed restore
/// sequence + raw-mode reset when the `TuiMainScreen` lock is held by the panicking
/// thread. Never panics: upstream wraps the restore in `try {} catch {}`
/// (interactive-mode.ts:3624-3628), so every step here ignores errors.
///
/// The fallback also queues a full stop op on the `TuiMainScreen`: the event loop's
/// next [`TuiMainScreen::tick`] pending-op drain runs it, writing the complete restore
/// sequence and setting `stopped` so later renders short-circuit instead of
/// clobbering the minimal restore above. When the panic happened on the
/// event-loop thread itself nothing drains the op — no renders follow
/// either, so the minimal sequence stands.
pub fn restore_terminal(tui: &TuiMainScreen) {
    if tui.try_stop(TuiStopOptions::default()) {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(MINIMAL_RESTORE_SEQUENCE.as_bytes());
    let _ = stdout.flush();
    // The locked-TuiMainScreen case cannot consult `was_raw`; a TUI that was started
    // put the terminal into raw mode, so disabling is the right default.
    let _ = crossterm::terminal::disable_raw_mode();
    tui.queue_stop(TuiStopOptions::default());
}

/// Install a process-global panic hook that restores the terminal BEFORE
/// delegating to the previously installed hook (the default hook prints the
/// panic message), matching upstream `uncaughtCrash` ordering: `ui.stop()`
/// first, `console.error(error)` second (interactive-mode.ts:3624-3636).
///
/// Call once, after constructing the [`TuiMainScreen`] and before/after `start()`
/// (the hook only needs the handle; stop on a never-started TUI is a safe
/// no-op). Re-installing chains onto whatever hook is current, so installing
/// twice wraps the hooks — install once per process.
///
/// Unlike upstream, the hook does not exit the process; the panic keeps
/// unwinding after the chained hook returns (see header note).
pub fn install_panic_hook(tui: &TuiMainScreen) {
    let previous_hook = std::panic::take_hook();
    let tui = tui.clone();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal(&tui);
        previous_hook(panic_info);
    }));
}

/// [`install_panic_hook`] for a [`TuiHandle`] — the production wiring used
/// by the interactive mode (`run_interactive_mode`), covering both renderer
/// variants incl. the alt-screen/fullscreen mode via
/// [`restore_terminal_handle`]. Same chaining contract as
/// [`install_panic_hook`] (install once per process; re-installing chains).
pub fn install_panic_hook_for_handle(handle: &TuiHandle) {
    let previous_hook = std::panic::take_hook();
    let handle = handle.clone();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal_handle(&handle);
        previous_hook(panic_info);
    }));
}

/// [`restore_terminal`] dispatching through a [`TuiHandle`]: tries the
/// non-blocking full stop on whichever renderer is live, falls back to the
/// fixed restore sequence (which exits alt-screen via `\x1b[?1049l`) when
/// the lock is held, and queues a full stop op for the event loop to drain.
/// [`restore_terminal`] for a [`TuiHandle`] — covers both renderer variants
/// (`Renderer::Main` regular mode and `Renderer::Alt` fullscreen/alt-screen
/// mode). `TuiHandle::try_stop` is non-blocking end-to-end (handle lock +
/// renderer lock); when either is held by the panicking thread the fixed
/// fallback sequence below runs instead, and its `\x1b[?1049l` prefix also
/// exits a fullscreen alt-screen, so both variants are covered on the
/// fallback path too.
pub fn restore_terminal_handle(handle: &TuiHandle) {
    if handle.try_stop(TuiStopOptions::default()) {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(MINIMAL_RESTORE_SEQUENCE.as_bytes());
    let _ = stdout.flush();
    // The locked case cannot consult `was_raw`; a TUI that was started put
    // the terminal into raw mode, so disabling is the right default.
    let _ = crossterm::terminal::disable_raw_mode();
    handle.queue_stop(TuiStopOptions::default());
}

/// Spawn a tokio task that restores the terminal on SIGTERM/SIGHUP and then
/// exits with code 0 — the upstream interactive-mode signal path
/// (`shutdown({ fromSignal: true })` ends in `process.exit(0)`,
/// interactive-mode.ts:3571; upstream registers SIGTERM on all platforms and
/// SIGHUP on non-Windows). The 143/129 exit codes of upstream print/rpc
/// modes do not apply: those modes never start a TUI.
///
/// Returns `None` when no tokio runtime is current (same convention as the
/// SIGWINCH forwarder in `terminal.rs`). Call from the interactive-mode
/// entry once a runtime exists. The task never resolves on its own; abort
/// the returned handle on orderly shutdown to mirror upstream's
/// `unregisterSignalHandlers`.
#[cfg(unix)]
pub fn spawn_signal_restore(tui: &TuiMainScreen) -> Option<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{signal, SignalKind};

    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let tui = tui.clone();
    Some(runtime.spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(_) => return,
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(stream) => stream,
            Err(_) => return,
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sighup.recv() => {}
        }
        restore_terminal(&tui);
        std::process::exit(0);
    }))
}

/// No signal restore on non-unix platforms (upstream likewise registers
/// SIGHUP only off Windows; SIGTERM restore there stays with T12).
#[cfg(not(unix))]
pub fn spawn_signal_restore(_tui: &TuiMainScreen) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::terminal::ProcessTerminal;

    /// Byte-capturing sink (same pattern as the `terminal.rs` tests:
    /// upstream monkey-patches `process.stdout.write`; here it is injected).
    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<String>>>);

    impl SharedWriter {
        fn bytes(&self) -> String {
            self.0.lock().unwrap().join("")
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Restores the default panic hook on drop: tests replace the
    /// process-global hook, and a failing assertion mid-test must not leak a
    /// silencing hook into later tests.
    struct PanicHookGuard;

    impl Drop for PanicHookGuard {
        fn drop(&mut self) {
            let _ = std::panic::take_hook();
        }
    }

    /// Serializes the tests that replace the process-global panic hook —
    /// cargo runs tests on parallel threads by default and concurrent
    /// set_hook/take_hook pairs would chain onto the wrong hook.
    static HOOK_TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// §8.5 / T11 self-test: after an induced panic the terminal restore
    /// sequence must already be written before the chained (default-output)
    /// hook runs. Runs the full real path: `TuiMainScreen::start` on a
    /// `ProcessTerminal` with an injected writer, then an actual panicking
    /// thread (caught via `join`).
    #[test]
    fn panic_hook_restores_terminal_before_chained_hook() {
        let _serial = HOOK_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _hook_guard = PanicHookGuard;
        let writer = SharedWriter::default();
        let terminal = ProcessTerminal::with_writer(writer.clone());
        let tui = TuiMainScreen::new(Box::new(terminal));
        tui.start();

        // Stand-in for the default hook: records whether the restore bytes
        // were already written when it ran. Kept silent on purpose — the
        // induced panic must not print into the test log.
        let restore_seen = Arc::new(AtomicBool::new(false));
        let chained_ran = Arc::new(AtomicBool::new(false));
        {
            let writer = writer.clone();
            let restore_seen = Arc::clone(&restore_seen);
            let chained_ran = Arc::clone(&chained_ran);
            std::panic::set_hook(Box::new(move |_| {
                let bytes = writer.bytes();
                if bytes.contains("\x1b[?2004l") && bytes.contains("\x1b[?25h") {
                    restore_seen.store(true, Ordering::SeqCst);
                }
                chained_ran.store(true, Ordering::SeqCst);
            }));
        }
        install_panic_hook(&tui);

        let panicked = std::thread::spawn(|| panic!("intentional T11 §8.5 recovery test panic"))
            .join()
            .is_err();
        assert!(panicked, "the induced panic must propagate to join");

        assert!(chained_ran.load(Ordering::SeqCst), "chained hook must run");
        assert!(
            restore_seen.load(Ordering::SeqCst),
            "terminal restore bytes must precede the chained hook output"
        );

        // Byte-level assertions on the restore sequence written by
        // `TuiMainScreen::stop_internal` + `ProcessTerminal::stop`.
        let bytes = writer.bytes();
        assert!(bytes.contains("\x1b[?25h"), "cursor shown: {bytes:?}");
        assert!(
            bytes.contains("\x1b[?2004l"),
            "bracketed paste off: {bytes:?}"
        );
        assert!(
            bytes.contains("\x1b[<u"),
            "kitty keyboard protocol popped: {bytes:?}"
        );
    }

    /// The fixed fallback sequence covers every protocol/state flag the
    /// engine can enable (locked-TuiMainScreen fallback of `restore_terminal`).
    #[test]
    fn minimal_restore_sequence_covers_terminal_state() {
        for sequence in [
            "\x1b[?2031l",
            "\x1b[?2004l",
            "\x1b[<u",
            "\x1b[>4;0m",
            "\x1b[?25h",
        ] {
            assert!(MINIMAL_RESTORE_SEQUENCE.contains(sequence));
        }
    }

    /// P0-1 wiring regression: the production entry (`run_interactive_mode`)
    /// installs the handle-oriented hook, so the same guarantee must hold
    /// through a [`TuiHandle`] wrapping the regular renderer.
    #[test]
    fn panic_hook_for_handle_restores_terminal_regular_mode() {
        let _serial = HOOK_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _hook_guard = PanicHookGuard;
        let writer = SharedWriter::default();
        let terminal = ProcessTerminal::with_writer(writer.clone());
        let tui = TuiMainScreen::new(Box::new(terminal));
        let handle = crate::tui_handle::TuiHandle::from_main(tui);
        handle.start();
        install_panic_hook_for_handle(&handle);

        let panicked =
            std::thread::spawn(|| panic!("intentional handle-variant recovery test panic"))
                .join()
                .is_err();
        assert!(panicked, "the induced panic must propagate to join");

        let bytes = writer.bytes();
        assert!(bytes.contains("\x1b[?25h"), "cursor shown: {bytes:?}");
        assert!(
            bytes.contains("\x1b[?2004l"),
            "bracketed paste off: {bytes:?}"
        );
    }

    /// P0-1 wiring regression, alt-screen variant: a panic while the
    /// fullscreen renderer is live must leave the main screen (`\x1b[?1049l`)
    /// with the cursor visible — the fallback sequence and the full stop
    /// both exit the alternate screen.
    #[test]
    fn panic_hook_for_handle_restores_terminal_alt_screen_mode() {
        let _serial = HOOK_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _hook_guard = PanicHookGuard;
        let writer = SharedWriter::default();
        let terminal = ProcessTerminal::with_writer(writer.clone());
        let alt = crate::tui_alt_screen::TuiAltScreen::new(Box::new(terminal));
        let handle = crate::tui_handle::TuiHandle::from_alt(alt);
        handle.start();
        install_panic_hook_for_handle(&handle);

        let panicked = std::thread::spawn(|| panic!("intentional alt-screen recovery test panic"))
            .join()
            .is_err();
        assert!(panicked, "the induced panic must propagate to join");

        let bytes = writer.bytes();
        assert!(
            bytes.contains("\x1b[?1049l"),
            "alt screen exited: {bytes:?}"
        );
        assert!(bytes.contains("\x1b[?25h"), "cursor shown: {bytes:?}");
    }
}
