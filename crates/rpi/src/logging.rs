//! Tracing initialization (coding-standards §16).
//!
//! The workspace logs through `tracing`; this module is the single
//! subscriber installation point:
//! - default level `INFO`, overridable via `RPI_LOG` (preferred) or
//!   `RUST_LOG` (standards §16.3);
//! - stderr sink for print/json/rpc modes;
//! - file sink (`<agent_dir>/logs/rpi.log`) while the interactive TUI runs
//!   — the TUI must not write to stderr (standards §16.1).

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

/// Shared, swappable log sink. Defaults to stderr; the interactive mode
/// swaps in a file writer so TUI rendering never races log output on
/// stderr.
#[derive(Clone)]
pub struct LogSink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Default for LogSink {
    fn default() -> Self {
        LogSink(Arc::new(Mutex::new(Box::new(std::io::stderr()))))
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSinkGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogSinkGuard { inner: &self.0 }
    }
}

/// One line's write handle (locks the sink per line — log lines are small,
/// contention is negligible).
pub struct LogSinkGuard<'a> {
    inner: &'a Mutex<Box<dyn Write + Send>>,
}

impl Write for LogSinkGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

impl Drop for LogSink {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.0.lock() {
            let _ = guard.flush();
        }
    }
}

impl LogSink {
    /// Install the global subscriber and return the sink handle (the
    /// interactive mode swaps its writer).
    ///
    /// `tui_mode`: file sink under `<agent_dir>/logs/rpi.log` (created on
    /// demand, append mode); anything else keeps stderr.
    pub fn install(tui_mode: bool) -> LogSink {
        let sink = LogSink::default();
        if tui_mode {
            if let Some(file) = open_log_file() {
                *sink.0.lock().unwrap_or_else(|e| e.into_inner()) =
                    Box::new(std::io::BufWriter::new(file));
            } else {
                // Log file unavailable (unwritable agent dir): degrade to
                // stderr with a one-line notice instead of failing startup.
                eprintln!("Warning: could not open log file; falling back to stderr");
            }
        }
        let filter = std::env::var("RPI_LOG")
            .ok()
            .map(EnvFilter::new)
            .or_else(|| EnvFilter::try_from_default_env().ok())
            .unwrap_or_else(|| EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(sink.clone())
            .init();
        sink
    }

    /// Swap the sink's writer (interactive mode enters/exits).
    pub fn set_writer(&self, writer: Box<dyn Write + Send>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = writer;
    }

    /// Restore the stderr writer (interactive mode exits).
    pub fn restore_stderr(&self) {
        self.set_writer(Box::new(std::io::stderr()));
    }
}

/// `<agent_dir>/logs/rpi.log`, parent created on demand.
fn open_log_file() -> Option<std::fs::File> {
    let logs_dir = crate::config::get_agent_dir().join("logs");
    std::fs::create_dir_all(&logs_dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("rpi.log"))
        .ok()
}
