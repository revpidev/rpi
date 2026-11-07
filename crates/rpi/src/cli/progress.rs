//! Single-line download progress renderer (V18-12 FR-B, rpi-own).
//!
//! Progress goes to stderr (results stay on stdout) so pipes and CI never
//! see carriage returns or ANSI. TTY mode redraws one line at most every
//! 100ms; non-TTY mode emits 25/50/75/100% or 5s checkpoints.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::download::{DownloadEvent, DownloadObserver};
use crate::extensions::llama::client::format_bytes;

const THROTTLE: Duration = Duration::from_millis(100);

fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
        {
            return Some(usize::from(size.ws_col));
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse().ok())
}
const NON_TTY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
struct RenderOptions {
    tty: bool,
    width: usize,
    color: bool,
}

impl RenderOptions {
    fn detect() -> Self {
        let tty = std::io::stderr().is_terminal();
        let width = terminal_width().unwrap_or(80);
        let color = tty && std::env::var_os("NO_COLOR").is_none();
        Self { tty, width, color }
    }
}

#[derive(Debug)]
struct Inner {
    start: Instant,
    last_render: Instant,
    last_percent: Option<u64>,
    last_non_tty: Instant,
    line_open: bool,
    last_bytes: u64,
    last_time: Instant,
    spinner: usize,
}

/// `DownloadObserver` implementation. `render()` is pure enough to unit
/// test at fixed widths; the default constructor detects the terminal.
pub struct ProgressRenderer {
    options: RenderOptions,
    inner: Mutex<Inner>,
}

impl Default for ProgressRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressRenderer {
    pub fn new() -> Self {
        Self::with_options(RenderOptions::detect())
    }

    pub fn for_tests(tty: bool, width: usize, color: bool) -> Self {
        Self::with_options(RenderOptions { tty, width, color })
    }

    fn with_options(options: RenderOptions) -> Self {
        let now = Instant::now();
        Self {
            options,
            inner: Mutex::new(Inner {
                start: now,
                last_render: now - THROTTLE,
                last_percent: None,
                last_non_tty: now - NON_TTY_INTERVAL,
                line_open: false,
                last_bytes: 0,
                last_time: now,
                spinner: 0,
            }),
        }
    }

    fn clear_line(&self, inner: &mut Inner) {
        if self.options.tty && inner.line_open {
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
            inner.line_open = false;
        }
    }

    fn write_line(&self, text: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{text}");
        let _ = stderr.flush();
    }

    fn render_progress(&self, inner: &mut Inner, downloaded: u64, total: Option<u64>) {
        let now = Instant::now();
        let ratio = total
            .filter(|total| *total > 0)
            .map(|total| (downloaded as f64 / total as f64).clamp(0.0, 1.0));
        if !self.options.tty {
            let elapsed = now.duration_since(inner.last_non_tty);
            if let Some((text, milestone)) =
                non_tty_checkpoint(downloaded, total, ratio, inner.last_percent, elapsed)
            {
                inner.last_percent = milestone;
                inner.last_non_tty = now;
                self.write_line(&text);
            }
            return;
        }
        if !tty_should_render(now.duration_since(inner.last_render), ratio) {
            return;
        }
        inner.last_render = now;
        inner.line_open = true;
        let elapsed = now.duration_since(inner.start).as_secs_f64().max(0.001);
        let speed = if downloaded >= inner.last_bytes {
            (downloaded - inner.last_bytes) as f64
                / now.duration_since(inner.last_time).as_secs_f64().max(0.001)
        } else {
            downloaded as f64 / elapsed
        };
        if downloaded > 0 {
            inner.last_bytes = downloaded;
            inner.last_time = now;
        }
        let line = render_line(
            self.options.width,
            downloaded,
            total,
            ratio,
            speed,
            inner.spinner,
            self.options.color,
        );
        inner.spinner = inner.spinner.wrapping_add(1);
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "\r\x1b[2K{line}");
        let _ = stderr.flush();
    }
}

/// Non-TTY checkpoints: 25/50/75/100% or a 5s heartbeat. Returns the line
/// and the milestone to remember (None for unknown totals).
pub(crate) fn non_tty_checkpoint(
    downloaded: u64,
    total: Option<u64>,
    ratio: Option<f64>,
    last_percent: Option<u64>,
    elapsed: Duration,
) -> Option<(String, Option<u64>)> {
    let percent = ratio.map(|ratio| (ratio * 100.0) as u64);
    let milestone = percent
        .map(|percent| percent / 25 * 25)
        .filter(|milestone| *milestone > 0);
    let crossed = milestone.is_some() && milestone != last_percent;
    let timed_out = elapsed >= NON_TTY_INTERVAL;
    if !(crossed || timed_out || percent == Some(100)) {
        return None;
    }
    let text = match total {
        Some(total) => format!(
            "Downloading {} / {} ({:.0}%)",
            format_bytes(downloaded as f64),
            format_bytes(total as f64),
            ratio.unwrap_or(0.0) * 100.0
        ),
        None => format!("Downloading {}", format_bytes(downloaded as f64)),
    };
    Some((text, milestone.or(percent)))
}

fn tty_should_render(elapsed: Duration, ratio: Option<f64>) -> bool {
    elapsed >= THROTTLE || ratio == Some(1.0)
}

impl DownloadObserver for ProgressRenderer {
    fn on_download_event(&self, event: &DownloadEvent) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        match event {
            DownloadEvent::Connecting { url, attempt } => {
                self.clear_line(&mut inner);
                let suffix = if *attempt > 1 {
                    format!(" (attempt {attempt})")
                } else {
                    String::new()
                };
                self.write_line(&format!("Connecting…{suffix} {url}"));
            }
            DownloadEvent::Progress { downloaded, total } => {
                self.render_progress(&mut inner, *downloaded, *total);
            }
            DownloadEvent::Retry {
                url,
                attempt,
                reason,
                downloaded,
                delay,
            } => {
                self.clear_line(&mut inner);
                self.write_line(&format!(
                    "Retrying {url} (attempt {attempt}, kept {}): {reason}{}",
                    format_bytes(*downloaded as f64),
                    if delay.is_zero() {
                        String::new()
                    } else {
                        format!(" — waiting {:.1}s", delay.as_secs_f64())
                    }
                ));
            }
            DownloadEvent::Switching {
                from,
                to,
                reason,
                kept_bytes,
            } => {
                self.clear_line(&mut inner);
                self.write_line(&format!(
                    "Switching from {from} to {to} (kept {}): {reason}",
                    format_bytes(*kept_bytes as f64)
                ));
            }
            DownloadEvent::Verifying { file } => {
                self.clear_line(&mut inner);
                self.write_line(&format!("Verifying {}…", file.display()));
            }
            DownloadEvent::Installing { target } => {
                self.clear_line(&mut inner);
                self.write_line(&format!("Installing {}…", target.display()));
            }
            DownloadEvent::Done { file, bytes } => {
                self.clear_line(&mut inner);
                self.write_line(&format!(
                    "Downloaded {} ({})",
                    file.display(),
                    format_bytes(*bytes as f64)
                ));
            }
            DownloadEvent::Failed { message } => {
                self.clear_line(&mut inner);
                self.write_line(&format!("Download failed: {message}"));
            }
        }
    }
}

/// Pure single-line formatter (width tiers: 100 / 80 / 60 / 50).
pub fn render_line(
    width: usize,
    downloaded: u64,
    total: Option<u64>,
    ratio: Option<f64>,
    bytes_per_second: f64,
    spinner: usize,
    color: bool,
) -> String {
    let paint = |text: String| {
        if color {
            format!("\x1b[32m{text}\x1b[0m")
        } else {
            text
        }
    };
    let width = width.max(10);
    let percent = ratio.map(|ratio| format!("{:>3.0}%", ratio * 100.0));
    let speed = format!("{}/s", format_bytes(bytes_per_second.max(0.0)));
    let (done_line, total_line) = match total {
        Some(total) => (format_bytes(downloaded as f64), format_bytes(total as f64)),
        None => (format_bytes(downloaded as f64), "?".to_owned()),
    };
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][spinner % 10];
    if width >= 100 {
        let remaining = total
            .filter(|total| *total > downloaded && bytes_per_second > 1.0)
            .map(|total| (total - downloaded) as f64 / bytes_per_second);
        let eta = remaining
            .map(|seconds| format!(" · ETA {}s", seconds.ceil() as u64))
            .unwrap_or_default();
        format!(
            "{spinner} {done_line} / {total_line}  {} {speed}{eta}",
            paint(percent.clone().unwrap_or_default())
        )
    } else if width >= 80 {
        format!(
            "{spinner} {done_line} / {total_line}  {} {speed}",
            paint(percent.clone().unwrap_or_default())
        )
    } else if width >= 60 {
        format!(
            "{spinner} {done_line} / {total_line}  {}",
            paint(percent.clone().unwrap_or_default())
        )
    } else {
        format!("{spinner} {}", paint(percent.unwrap_or(done_line)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_line_degrades_by_width_and_color() {
        let full = render_line(120, 512, Some(1024), Some(0.5), 2048.0, 0, false);
        assert!(full.contains("512 B / 1.00 KiB"), "full: {full}");
        assert!(full.contains("50%"), "full: {full}");
        assert!(full.contains("2.00 KiB/s"), "full: {full}");
        assert!(full.contains("ETA"), "full: {full}");

        let no_eta = render_line(80, 512, Some(1024), Some(0.5), 2048.0, 1, false);
        assert!(!no_eta.contains("ETA"), "80: {no_eta}");
        assert!(no_eta.contains("2.00 KiB/s"), "80: {no_eta}");

        let no_speed = render_line(60, 512, Some(1024), Some(0.5), 2048.0, 2, false);
        assert!(!no_speed.contains("/s"), "60: {no_speed}");
        assert!(no_speed.contains("50%"), "60: {no_speed}");

        let percent_only = render_line(40, 512, Some(1024), Some(0.5), 2048.0, 3, false);
        assert!(!percent_only.contains("KiB /"), "40: {percent_only}");
        assert!(percent_only.contains("50%"), "40: {percent_only}");

        let colored = render_line(60, 512, Some(1024), Some(0.5), 2048.0, 0, true);
        assert!(colored.contains("\x1b[32m"), "color: {colored}");
        assert!(colored.contains("\x1b[0m"), "color: {colored}");

        let unknown_total = render_line(100, 300, None, None, 0.0, 4, false);
        assert!(unknown_total.contains("300 B"), "{unknown_total}");
        assert!(unknown_total.contains('?'), "{unknown_total}");
    }

    #[test]
    fn non_tty_checkpoints_are_quarterly_or_five_seconds() {
        let start = Duration::ZERO;
        assert!(non_tty_checkpoint(0, Some(1024), Some(0.0), None, start).is_none());
        assert!(
            non_tty_checkpoint(256, Some(1024), Some(0.25), None, start)
                .unwrap()
                .0
                .contains("25%")
        );
        // Same milestone again: no line unless the heartbeat elapsed.
        assert!(non_tty_checkpoint(300, Some(1024), Some(0.29), Some(25), start).is_none());
        assert!(
            non_tty_checkpoint(300, Some(1024), Some(0.29), Some(25), NON_TTY_INTERVAL).is_some()
        );
        assert!(
            non_tty_checkpoint(1024, Some(1024), Some(1.0), Some(75), start)
                .unwrap()
                .0
                .contains("100%")
        );
        let unknown = non_tty_checkpoint(300, None, None, None, NON_TTY_INTERVAL)
            .expect("heartbeat for unknown totals")
            .0;
        assert!(unknown.contains("300 B"), "{unknown}");
    }

    #[test]
    fn tty_throttle_allows_completion_but_not_spam() {
        assert!(!tty_should_render(Duration::from_millis(10), Some(0.5)));
        assert!(tty_should_render(THROTTLE, Some(0.5)));
        assert!(tty_should_render(Duration::ZERO, Some(1.0)));
    }
}
