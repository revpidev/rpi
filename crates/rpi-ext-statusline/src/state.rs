//! Accumulated session state: usage/cost totals, session clock, transcript
//! latch, mounted-rendering channel (TE12 FR-K).
//!
//! The totals mirror the host's own footer accounting
//! (`rpi/src/core/usage_totals.rs:12-28` `add_usage_to_totals` — including
//! `cost = usage.cost.total`), fed exclusively by `message_end` events the
//! host forwards for THIS session's assistant messages (subagents children
//! run their own process with their own extension host, so no sidechain
//! filtering is needed — TE12 verification §1).

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::config::{Placement, StatusLineConfig};
use crate::paths::{find_latest_session_file, resolve_session_dir, session_id_from_path};

/// Cumulative usage/cost counters (the footer's `usage_totals` shape).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl Totals {
    fn from_usage(usage: &Value) -> Self {
        let num = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        Totals {
            input: num("input"),
            output: num("output"),
            cache_read: num("cacheRead"),
            cache_write: num("cacheWrite"),
            cost: usage
                .pointer("/cost/total")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        }
    }
}

/// Pure-data snapshot consumed by [`crate::payload::build_stdin_json`]
/// (unit-testable without any FFI).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `ctx.cwd`.
    pub cwd: String,
    /// `ctx.model` raw JSON (`{id, name, contextWindow, ...}`) or `None`.
    pub model: Option<Value>,
    /// `ctx.getContextUsage` raw JSON (`{tokens, contextWindow, percent}`)
    /// or `None`.
    pub context_usage: Option<Value>,
    /// `getThinkingLevel` (off/minimal/low/medium/high/xhigh/max).
    pub thinking_level: Option<String>,
    /// `getSessionName`.
    pub session_name: Option<String>,
    pub totals: Totals,
    pub last_usage: Option<Totals>,
    pub session_elapsed_ms: u128,
    pub transcript_path: Option<String>,
    pub session_id: Option<String>,
}

/// Mutable engine state, shared between the dispatch thread (event-driven
/// updates, µs-scale) and the refresh loop (snapshot reads) through a
/// `Mutex` — the subagents static-`STATE` precedent.
#[derive(Debug)]
pub struct EngineState {
    totals: Totals,
    last_usage: Option<Totals>,
    session_started_at: Instant,
    /// Sticky transcript latch: `(cwd at latch time, resolved file)`.
    transcript: Option<(PathBuf, PathBuf)>,
    session_id: Option<String>,
    /// Which rendering channel currently carries our output (`None` = the
    /// built-in footer is untouched) — needed to restore on config removal
    /// (FR-H).
    pub mounted: Option<Placement>,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            totals: Totals::default(),
            last_usage: None,
            session_started_at: Instant::now(),
            transcript: None,
            session_id: None,
            mounted: None,
        }
    }
}

impl EngineState {
    /// `message_end`: accumulate `message.usage` when present (assistant
    /// messages). Returns whether anything changed (drives the refresh).
    pub fn accumulate_usage(&mut self, message: &Value) -> bool {
        let Some(usage) = message.get("usage") else {
            return false;
        };
        let totals = Totals::from_usage(usage);
        self.totals.input += totals.input;
        self.totals.output += totals.output;
        self.totals.cache_read += totals.cache_read;
        self.totals.cache_write += totals.cache_write;
        self.totals.cost += totals.cost;
        self.last_usage = Some(totals);
        true
    }

    /// `session_start` (FR-K): new/resume/fork/startup reset the
    /// accumulators, session clock and transcript latch; reload only
    /// invalidates the latch (accumulators keep the session's totals).
    pub fn on_session_start(&mut self, reason: Option<&str>) {
        match reason {
            Some("reload") => self.transcript = None,
            _ => {
                self.totals = Totals::default();
                self.last_usage = None;
                self.session_started_at = Instant::now();
                self.transcript = None;
            }
        }
    }

    /// Ensure a valid transcript latch: (re)resolve when absent, when the
    /// cwd moved, or when the latched file disappeared (TE-D34 sticky
    /// latch). Never fails — an unresolvable directory just leaves the
    /// latch empty (stdin JSON omits `transcript_path`).
    pub fn ensure_transcript(&mut self, cwd: &Path, settings_session_dir: Option<&str>) {
        let needs_reresolve = match &self.transcript {
            None => true,
            Some((latched_cwd, path)) => latched_cwd != cwd || !path.exists(),
        };
        if !needs_reresolve {
            return;
        }
        let dir = resolve_session_dir(cwd, settings_session_dir);
        self.transcript = find_latest_session_file(&dir).map(|path| (cwd.to_owned(), path));
        self.session_id = self
            .transcript
            .as_ref()
            .and_then(|(_, path)| session_id_from_path(path));
    }

    /// Read-side projection (refresh tick input).
    pub fn totals(&self) -> Totals {
        self.totals
    }

    pub fn last_usage(&self) -> Option<Totals> {
        self.last_usage
    }

    pub fn session_elapsed_ms(&self) -> u128 {
        self.session_started_at.elapsed().as_millis()
    }

    pub fn transcript_path(&self) -> Option<&Path> {
        self.transcript.as_ref().map(|(_, path)| path.as_path())
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Dirty key (fleet.rs:381 precedent): when this string is unchanged the
/// refresh tick skips spawning the script. Includes the elapsed-seconds
/// coarse clock so a duration display advances on subsequent events even
/// with identical usage.
pub fn snapshot_dirty_key(snapshot: &Snapshot, config: &StatusLineConfig) -> String {
    #[derive(Serialize)]
    struct Key<'a> {
        command: &'a str,
        padding: usize,
        placement: &'a str,
        cwd: &'a str,
        model_id: Option<&'a str>,
        context_tokens: Option<&'a Value>,
        context_percent: Option<&'a Value>,
        effort: Option<&'a str>,
        session_name: Option<&'a str>,
        totals: Totals,
        last_usage: Option<Totals>,
        elapsed_secs: u128,
    }
    let key = Key {
        command: &config.command,
        padding: config.padding,
        placement: match config.placement {
            Placement::Replace => "replace",
            Placement::Widget => "widget",
            Placement::Status => "status",
        },
        cwd: &snapshot.cwd,
        model_id: snapshot
            .model
            .as_ref()
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str),
        context_tokens: snapshot
            .context_usage
            .as_ref()
            .and_then(|c| c.get("tokens")),
        context_percent: snapshot
            .context_usage
            .as_ref()
            .and_then(|c| c.get("percent")),
        effort: snapshot.thinking_level.as_deref(),
        session_name: snapshot.session_name.as_deref(),
        totals: snapshot.totals,
        last_usage: snapshot.last_usage,
        elapsed_secs: snapshot.session_elapsed_ms / 1000,
    };
    serde_json::to_string(&key).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Value {
        json!({
            "input": input, "output": output,
            "cacheRead": cache_read, "cacheWrite": cache_write,
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": cost}
        })
    }

    #[test]
    fn accumulate_usage_sums_and_tracks_last() {
        let mut state = EngineState::default();
        let message = json!({"role": "assistant", "usage": usage(100, 10, 1000, 200, 0.05)});
        assert!(state.accumulate_usage(&message));
        let message = json!({"role": "assistant", "usage": usage(50, 5, 500, 100, 0.02)});
        assert!(state.accumulate_usage(&message));
        assert_eq!(state.totals().input, 150);
        assert_eq!(state.totals().cache_write, 300);
        assert!((state.totals().cost - 0.07).abs() < 1e-9);
        assert_eq!(state.last_usage().map(|t| t.input), Some(50));
        // No usage (e.g. a user message) → no change.
        assert!(!state.accumulate_usage(&json!({"role": "user"})));
    }

    #[test]
    fn session_start_reset_matrix() {
        let mut state = EngineState::default();
        state.accumulate_usage(&json!({"usage": usage(10, 1, 0, 0, 1.0)}));
        state.on_session_start(Some("new"));
        assert_eq!(state.totals(), Totals::default());
        assert_eq!(state.last_usage(), None);
        state.accumulate_usage(&json!({"usage": usage(10, 1, 0, 0, 1.0)}));
        // reload keeps totals.
        state.on_session_start(Some("reload"));
        assert_eq!(state.totals().input, 10);
        state.on_session_start(None);
        assert_eq!(state.totals(), Totals::default());
    }

    #[test]
    fn transcript_latch_is_sticky_and_re_latches_on_cwd_change() {
        let dir = std::env::temp_dir().join(format!("rpi-statusline-state-{}", std::process::id()));
        let session_dir = dir.join("sessions").join("--x--");
        std::fs::create_dir_all(&session_dir).expect("mkdir");
        let file =
            session_dir.join("2026-08-19T10-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl");
        std::fs::write(&file, b"{}").expect("write");

        let cwd = Path::new("/definitely/not/here");
        // Point the resolution at the fixture via the env override.
        std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", &session_dir);
        let mut state = EngineState::default();
        state.ensure_transcript(cwd, None);
        assert_eq!(state.transcript_path(), Some(file.as_path()));
        assert_eq!(
            state.session_id(),
            Some("018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e")
        );
        // Sticky: resolving again with no change keeps the latch (hard to
        // observe directly; changing cwd re-latches to the same dir).
        state.ensure_transcript(Path::new("/elsewhere"), None);
        assert_eq!(state.transcript_path(), Some(file.as_path()));
        // File disappearing invalidates the latch.
        std::fs::remove_file(&file).expect("remove");
        state.ensure_transcript(cwd, None);
        assert_eq!(state.transcript_path(), None);
        std::env::remove_var("RPI_CODING_AGENT_SESSION_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }
}
