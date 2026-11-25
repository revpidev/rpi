//! Opt-in frame measurements for the V19-00 replay harness.
//!
//! The probe is inert unless `RPI_V19_BENCHMARK_LOG` names an output JSONL
//! file. Pure compute ends before terminal I/O; `terminal_io_ns` is reported
//! separately so a blocked terminal cannot masquerade as layout/paint cost.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::tui_base::RenderScheduleStats;

const LOG_ENV: &str = "RPI_V19_BENCHMARK_LOG";

#[derive(Default)]
struct Recorder {
    file: Option<File>,
    sequence: u64,
    last_started: Option<Instant>,
}

fn recorder() -> &'static Mutex<Recorder> {
    static RECORDER: OnceLock<Mutex<Recorder>> = OnceLock::new();
    RECORDER.get_or_init(|| Mutex::new(Recorder::default()))
}

pub fn deterministic_frame_index() -> Option<usize> {
    log_path()?;
    let recorder = recorder().lock().unwrap_or_else(|error| error.into_inner());
    Some(recorder.sequence as usize)
}

fn log_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os(LOG_ENV).map(PathBuf::from))
        .as_ref()
}

#[derive(Debug)]
struct FrameTiming {
    mode: &'static str,
    started: Instant,
    layout: Option<Duration>,
    paint: Option<Duration>,
    write_compute: Option<Duration>,
    terminal_io: Duration,
    output_bytes: usize,
    output_hash: Option<u64>,
    /// Render schedule state when the frame started (V19-06 backpressure
    /// report): tier and latest-wins counters.
    backpressure: Option<RenderScheduleStats>,
}

impl FrameTiming {
    fn new(mode: &'static str, started: Instant) -> Self {
        Self {
            mode,
            started,
            layout: None,
            paint: None,
            write_compute: None,
            terminal_io: Duration::ZERO,
            output_bytes: 0,
            output_hash: None,
            backpressure: None,
        }
    }
}

/// Records the render schedule state of the frame being laid out (V19-06
/// FR-D R5): adaptive tier and latest-wins counters travel with each frame
/// so the throttled replay report can attribute frames to tiers.
pub fn note_backpressure(stats: RenderScheduleStats) {
    with_frame(|frame, _| frame.backpressure = Some(stats));
}

thread_local! {
    static ACTIVE_FRAME: RefCell<Option<FrameTiming>> = const { RefCell::new(None) };
}

/// Marks one renderer frame. Nested probes are ignored rather than replacing
/// the outer frame's timestamps.
pub struct FrameProbe {
    active: bool,
}

impl FrameProbe {
    pub fn new(mode: &'static str) -> Self {
        if log_path().is_none() {
            return Self { active: false };
        }
        let active = ACTIVE_FRAME.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                false
            } else {
                *slot = Some(FrameTiming::new(mode, Instant::now()));
                true
            }
        });
        Self { active }
    }
}

impl Drop for FrameProbe {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let timing = ACTIVE_FRAME.with(|slot| slot.borrow_mut().take());
        if let Some(timing) = timing {
            record(timing);
        }
    }
}

fn with_frame(update: impl FnOnce(&mut FrameTiming, Instant)) {
    ACTIVE_FRAME.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(frame) = slot.as_mut() {
            let now = Instant::now();
            update(frame, now);
        }
    });
}

/// End of component layout/render. If a renderer performs a second layout
/// pass, the mark includes both passes and any intervening search work.
pub fn mark_layout() {
    with_frame(|frame, now| frame.layout = Some(now.saturating_duration_since(frame.started)));
}

/// End of screen paint/compositing, before output-buffer construction.
pub fn mark_paint() {
    with_frame(|frame, now| {
        let start = frame
            .layout
            .map_or(frame.started, |end| frame.started + end);
        frame.paint = Some(now.saturating_duration_since(start));
    });
}

/// Write an output buffer while separating buffer construction from terminal
/// I/O. The first write owns the output hash; later cursor writes only add I/O.
pub fn terminal_write(data: &str, write: impl FnOnce()) {
    let active = ACTIVE_FRAME.with(|slot| slot.borrow().is_some());
    if !active {
        write();
        return;
    }
    with_frame(|frame, now| {
        if frame.write_compute.is_none() {
            let start = match (frame.layout, frame.paint) {
                (Some(layout), Some(paint)) => frame.started + layout + paint,
                (Some(layout), None) => frame.started + layout,
                _ => frame.started,
            };
            frame.write_compute = Some(now.saturating_duration_since(start));
            let mut hasher = DefaultHasher::new();
            data.hash(&mut hasher);
            frame.output_hash = Some(hasher.finish());
            frame.output_bytes = data.len();
        }
    });
    let started = Instant::now();
    write();
    let elapsed = started.elapsed();
    with_frame(|frame, _| frame.terminal_io += elapsed);
}

fn record(frame: FrameTiming) {
    let layout = frame.layout.unwrap_or_default();
    let paint = frame.paint.unwrap_or_default();
    let write_compute = frame.write_compute.unwrap_or_default();
    let pure_compute = layout + paint + write_compute;
    let mut recorder = recorder().lock().unwrap_or_else(|error| error.into_inner());
    if recorder.file.is_none() {
        let Some(path) = log_path() else {
            return;
        };
        recorder.file = File::create(path).ok();
    }
    recorder.sequence += 1;
    let sequence = recorder.sequence;
    let interval = recorder
        .last_started
        .map(|last| frame.started.saturating_duration_since(last));
    recorder.last_started = Some(frame.started);
    let output_hash = frame.output_hash.unwrap_or_else(|| {
        let mut hasher = DefaultHasher::new();
        "".hash(&mut hasher);
        hasher.finish()
    });
    let event = json!({
        "type": "frame",
        "sequence": sequence,
        "mode": frame.mode,
        "layout_ns": layout.as_nanos(),
        "paint_ns": paint.as_nanos(),
        "write_compute_ns": write_compute.as_nanos(),
        "pure_compute_ns": pure_compute.as_nanos(),
        "terminal_io_ns": frame.terminal_io.as_nanos(),
        "frame_interval_ns": interval.map(|value| value.as_nanos()),
        "output_bytes": frame.output_bytes,
        "output_hash": format!("{output_hash:016x}"),
        "backpressureTier": frame.backpressure.map(|stats| stats.tier_fps),
        "requestedGenerations": frame.backpressure.map(|stats| stats.requested_generation),
        "renderedGenerations": frame.backpressure.map(|stats| stats.rendered_generation),
        "supersededRequests": frame.backpressure.map(|stats| stats.superseded_requests),
        "latencyPromotions": frame.backpressure.map(|stats| stats.latency_promotions),
        "tierChanges": frame.backpressure.map(|stats| stats.tier_changes),
        "lastFrameCostNs": frame
            .backpressure
            .map(|stats| stats.last_frame_cost.as_nanos()),
    });
    let Some(file) = recorder.file.as_mut() else {
        return;
    };
    // A failed/unwritable log produces no usable baseline: the runner checks
    // for a non-empty complete JSONL sequence and fails closed.
    let _ = writeln!(file, "{event}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_or_unconfigured_probes_are_noops() {
        let probe = FrameProbe::new("regular");
        mark_layout();
        mark_paint();
        terminal_write("not-logged", || {});
        drop(probe);
    }
}
