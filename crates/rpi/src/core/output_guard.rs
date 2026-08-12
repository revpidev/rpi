//! Raw stdout writer with backpressure — port of
//! `packages/coding-agent/src/core/output-guard.ts` @ pi 0.84.1+ (4181f66).
//!
//! Semantic correspondence (intentional fusion, T18):
//!
//! - Upstream `writeRawStdout` (output-guard.ts:85-93) appends every write to
//!   a single promise chain, so all producers (session events, RPC responses,
//!   extension UI frames) share one ordered write path.
//! - Upstream `waitForRawStdoutBackpressure` (output-guard.ts:95-103) lets a
//!   producer wait until the chain has drained; print/rpc modes hang it off
//!   `session.agent.subscribe(...)`, i.e. the agent loop's ordered listener
//!   barrier stalls until stdout is writable again.
//!
//! rpi fuses the two into one blocking write: [`RawStdout::write`] holds a
//! mutex (the single ordered write path) and returns only after the bytes
//! were handed to the OS (the drain wait). Session event listeners run
//! synchronously inside the agent loop's listener barrier
//! (`rpi-agent::agent::process_events` awaits every listener in subscription
//! order), so a blocked write stalls the event source itself — events are
//! never dropped, never merged, and there is no intermediate buffer to grow
//! (upstream keeps at most the in-flight chain; rpi keeps nothing).
//!
//! Write-error handling: upstream's chain catch exits the process with code 1
//! (output-guard.ts:90-92). A library cannot `process::exit`, so rpi records
//! the first error instead and fires [`RawStdout::error_notify`]; rpc mode
//! selects on that notification to break its main loop and exit 1 immediately
//! (T18b), while print mode maps a recorded error to exit code 1 at the
//! natural end of the run.
//!
//! Intentional differences:
//! - No `takeOverStdout`/`restoreStdout` (TUI stderr redirection is an
//!   interactive-mode concern and lives there).
//! - No ENOBUFS/EAGAIN retry loop: `std::io::Write` on a blocking fd already
//!   waits for writability, which is the retry loop's net effect.

use std::io::Write;
use std::sync::{Arc, Mutex};

struct RawStdoutInner {
    out: Box<dyn Write + Send>,
    /// First write error, if any (see module docs; upstream exits 1).
    error: Option<std::io::Error>,
}

/// Shared, cloneable handle to the single ordered stdout write path.
///
/// Cheap to clone; every clone writes through the same mutex, so lines from
/// different producers never interleave mid-record.
#[derive(Clone)]
pub struct RawStdout {
    inner: Arc<Mutex<RawStdoutInner>>,
    /// Fired (once, via `notify_one`'s stored permit) when the first
    /// write/flush error is recorded (T18b); see [`Self::error_notify`].
    error_notify: Arc<tokio::sync::Notify>,
}

impl RawStdout {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        RawStdout {
            inner: Arc::new(Mutex::new(RawStdoutInner { out, error: None })),
            error_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// `writeRawStdout` + `waitForRawStdoutBackpressure` fused (see module
    /// docs). Blocks until the bytes are written; a full pipe therefore
    /// applies backpressure to the caller. Empty writes are skipped
    /// (output-guard.ts:86-88). After a recorded error, later writes are
    /// dropped (the fd is broken; upstream would have exited).
    pub fn write(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.error.is_some() {
            return;
        }
        if let Err(error) = inner.out.write_all(text.as_bytes()) {
            inner.error = Some(error);
            drop(inner);
            self.error_notify.notify_one();
        }
    }

    /// `flushRawStdout` (output-guard.ts:105-107): the blocking write above
    /// already drains before returning, so this only flushes the writer.
    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = inner.out.flush() {
            if inner.error.is_none() {
                inner.error = Some(error);
                drop(inner);
                self.error_notify.notify_one();
            }
        }
    }

    /// Notify handle fired when the first write/flush error is recorded
    /// (T18b): mode main loops select on it to exit 1 as soon as the write
    /// path breaks, matching upstream's `process.exit(1)` the moment the
    /// write chain rejects (output-guard.ts:90-92). `notify_one` stores a
    /// permit, so a consumer that starts waiting after the error was
    /// recorded still observes it.
    pub fn error_notify(&self) -> Arc<tokio::sync::Notify> {
        self.error_notify.clone()
    }

    /// Whether a write/flush has failed (modes map this to exit code 1,
    /// matching upstream's `process.exit(1)` on chain rejection).
    pub fn has_error(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared capture buffer (test double for the fd).
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_from_clones_stay_ordered_and_intact() {
        let buf = SharedBuf::default();
        let raw = RawStdout::new(Box::new(buf.clone()));
        let clone = raw.clone();
        raw.write("first\n");
        clone.write("second\n");
        raw.flush();
        assert!(!raw.has_error());
        let bytes = buf.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(String::from_utf8(bytes).expect("utf8"), "first\nsecond\n");
    }

    #[test]
    fn empty_writes_are_skipped() {
        let buf = SharedBuf::default();
        let raw = RawStdout::new(Box::new(buf.clone()));
        raw.write("");
        assert!(buf.0.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert!(!raw.has_error());
    }

    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn first_error_is_recorded_and_later_writes_dropped() {
        let raw = RawStdout::new(Box::new(BrokenPipe));
        raw.write("boom\n");
        assert!(raw.has_error());
        // Must not panic; further writes are no-ops.
        raw.write("more\n");
        raw.flush();
        assert!(raw.has_error());
    }

    #[tokio::test]
    async fn first_error_fires_error_notify_with_stored_permit() {
        // T18b: rpc mode's main loop selects on this notification to exit 1
        // immediately. `notify_one` stores a permit, so a consumer that
        // starts waiting only after the error was recorded still observes it.
        let raw = RawStdout::new(Box::new(BrokenPipe));
        let notify = raw.error_notify();
        raw.write("boom\n");
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .expect("error notify fires after the first recorded error");
    }
}
