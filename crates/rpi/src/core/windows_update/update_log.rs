//! `%LOCALAPPDATA%\rpi\logs\update.log` (R7.6.4).
//!
//! One line per event, ISO 8601 UTC timestamp first. The file is appended
//! to in place (`OpenOptions::append`) and rotated to `update.log.1` once it
//! passes 1 MiB (one generation). Logging never changes an outcome: a
//! failed write is dropped. Only paths, versions, pids and error texts are
//! logged — never credentials, environment values or file contents.

use std::path::{Path, PathBuf};

use super::fs::UpdateFs;
use super::layout::{UPDATE_LOG_MAX_BYTES, UPDATE_LOG_ROTATED_SUFFIX};

/// Format one log line.
pub fn format_line(timestamp: &str, event: &str, detail: &str) -> String {
    let detail: String = detail.lines().next().unwrap_or_default().trim().to_owned();
    if detail.is_empty() {
        format!("{timestamp} {event}\n")
    } else {
        format!("{timestamp} {event} {detail}\n")
    }
}

/// Append one event; `None` when the log is disabled (no path).
pub fn log_event(fs: &dyn UpdateFs, log_path: Option<&Path>, event: &str, detail: &str) {
    let Some(log_path) = log_path else {
        return;
    };
    let _ = append_line(fs, log_path, &format_line(&fs.now_iso8601(), event, detail));
}

/// Append `line`, rotating first when the file is over the cap. The first
/// write failure (unwritable directory, full disk) is the caller's to
/// ignore; the line is simply lost. One `stat`-length query plus one
/// in-place append per event: no read-modify-write of the whole file.
pub fn append_line(fs: &dyn UpdateFs, log_path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = log_path.parent() {
        fs.create_dir_all(dir)?;
    }
    rotate_if_needed(fs, log_path)?;
    fs.append(log_path, line.as_bytes())
}

/// Move an oversized log to `update.log.1` (replacing the previous one).
pub fn rotate_if_needed(fs: &dyn UpdateFs, log_path: &Path) -> std::io::Result<()> {
    if fs.file_len(log_path)? < UPDATE_LOG_MAX_BYTES {
        return Ok(());
    }
    let mut rotated = log_path.as_os_str().to_owned();
    rotated.push(UPDATE_LOG_ROTATED_SUFFIX);
    fs.rename(log_path, PathBuf::from(rotated).as_path())
}

/// A log sink for code that only has a path: `None` disables it.
pub fn sink<'a>(fs: &'a dyn UpdateFs, log_path: Option<&'a Path>) -> impl Fn(&str) + 'a {
    move |message: &str| {
        let (event, detail) = message.split_once(": ").unwrap_or((message, ""));
        log_event(fs, log_path, event, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::fs::SpyFs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn log_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "rpi-wu-log-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ))
            .join("logs/update.log")
    }

    #[test]
    fn lines_carry_a_timestamp_and_one_detail_line() {
        assert_eq!(
            format_line("2026-11-28T00:00:00Z", "staged", "rpi 0.1.9-rc.4"),
            "2026-11-28T00:00:00Z staged rpi 0.1.9-rc.4\n"
        );
        assert_eq!(format_line("t", "event", ""), "t event\n");
        assert_eq!(
            format_line("t", "event", " first line\nsecond line "),
            "t event first line\n"
        );
    }

    #[test]
    fn events_append_rotate_and_survive_a_disabled_log() {
        let fs = SpyFs::new();
        fs.set_now("2026-11-28T00:00:00Z");
        let path = log_path();
        log_event(&fs, None, "ignored", "no log path");
        log_event(&fs, Some(&path), "staged", "0.1.9-rc.4");
        log_event(&fs, Some(&path), "committed", "");
        let text = String::from_utf8(fs.read(&path).unwrap()).unwrap();
        assert_eq!(
            text,
            "2026-11-28T00:00:00Z staged 0.1.9-rc.4\n2026-11-28T00:00:00Z committed\n"
        );
        let sink = sink(&fs, Some(&path));
        sink("transaction: step failed (x)");
        let text = String::from_utf8(fs.read(&path).unwrap()).unwrap();
        assert!(
            text.ends_with("2026-11-28T00:00:00Z transaction step failed (x)\n"),
            "{text}"
        );

        // Rotation: a file over the cap moves to `update.log.1`.
        let big = "x".repeat(UPDATE_LOG_MAX_BYTES as usize + 1);
        fs.write_atomic(&path, big.as_bytes()).unwrap();
        log_event(&fs, Some(&path), "after", "rotate");
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        assert_eq!(fs.read(&rotated).unwrap(), big.as_bytes());
        assert_eq!(
            String::from_utf8(fs.read(&path).unwrap()).unwrap(),
            "2026-11-28T00:00:00Z after rotate\n"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
