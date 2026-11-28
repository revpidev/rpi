//! The unified process exit outlet (V19-09, ADR-0050 design §11.1).
//!
//! Every exit of the `rpi` process goes through here — the two binary
//! entry points after `run_app` returns, and the library paths that used
//! to call `process::exit` directly (`app.rs`, the interactive mode's
//! fatal-error exit, the Unix signal restores). On Windows the outlet is
//! what makes the first-boot confirmation work: it marks the exit as
//! normal (a Ctrl+C handler marks it earlier), confirms an applied update
//! on a successful one-shot run, records the first normal exit after a
//! confirmation and releases the process pin. A *fatal* exit — the TUI's
//! `handle_fatal_runtime_error`, a runtime that failed to start — is not
//! marked, so a new version that dies that way during its first boot is
//! treated as the crash it is.
//!
//! On every other platform the outlet is a pass-through: the process exit
//! code and timing are exactly what `std::process::exit` produced before.

/// Exit normally (user-initiated, one-shot command, signal restore).
pub fn exit(code: i32) -> ! {
    on_exit(code, false);
    std::process::exit(code)
}

/// Exit after a fatal runtime error: not a normal exit.
pub fn exit_fatal(code: i32) -> ! {
    on_exit(code, true);
    std::process::exit(code)
}

fn on_exit(code: i32, fatal: bool) {
    #[cfg(windows)]
    {
        crate::core::windows_update::startup::on_exit(code, fatal);
    }
    #[cfg(not(windows))]
    {
        let _ = (code, fatal);
    }
}
