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
use std::time::{Instant, SystemTime};

use serde::Serialize;
use serde_json::Value;

use crate::config::Placement;
use crate::paths::{find_latest_session_file, resolve_session_dir, session_id_from_path};

/// Live streaming measurements for the CURRENT assistant message
/// (03-realtime-token-count §1.4/§2.3): native measures, python computes.
/// All values are raw measurements — no token conversion, no rate.
#[derive(Debug, Default)]
pub struct LiveMeasure {
    /// An assistant message is in flight (`message_start` seen, matching
    /// `message_end` not yet; FR-D: true for the brief window before the
    /// first delta too — "this message has started").
    streaming: bool,
    text_chars: u64,
    thinking_chars: u64,
    toolcall_chars: u64,
    /// Chars accumulated since the last snapshot advance — a SEPARATE
    /// monotonic counter cleared ONLY when the snapshot advances, so it
    /// survives the `message_start` block reset (FR-B: cross-message
    /// delta windows must not be corrupted by message boundaries).
    pending_delta_chars: u64,
    /// Delta-window base: the instant of the last snapshot advance (or of
    /// the message start for the first advance).
    advanced_at: Option<Instant>,
    message_started_at: Option<Instant>,
    /// Frozen elapsed at `message_end` ("keep the last measurements"), so
    /// the payload fingerprint stops changing once the stream settles.
    message_ended_elapsed_ms: Option<u128>,
    /// `message.usage.output`, only when `Some(n) && n > 0` (FR-C: 0 or
    /// missing — aborted/error, usage-less providers — stays `None`).
    output_tokens_exact: Option<u64>,
}

/// Read-side projection of [`LiveMeasure`] at one payload assembly
/// instant (the §1.6 `rpi.live_output` block, snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LiveSnapshot {
    pub streaming: bool,
    pub text_chars: u64,
    pub thinking_chars: u64,
    pub toolcall_chars: u64,
    pub delta_chars: u64,
    pub delta_ms: u128,
    pub elapsed_ms: u128,
    pub output_tokens_exact: Option<u64>,
}

impl LiveMeasure {
    /// FR-D: an assistant `message_start` resets the per-message block
    /// accumulators and clock; the delta window base is untouched.
    pub fn on_message_start(&mut self, role: Option<&str>) {
        if role != Some("assistant") {
            return;
        }
        self.streaming = true;
        self.text_chars = 0;
        self.thinking_chars = 0;
        self.toolcall_chars = 0;
        self.message_started_at = Some(Instant::now());
        self.message_ended_elapsed_ms = None;
        self.output_tokens_exact = None;
    }

    /// FR-A: per-delta bookkeeping — only `assistantMessageEvent.{type,
    /// delta}` is read; `*_start`/`*_end` events carry no delta and add
    /// nothing. Category by event type (no contentIndex dimension —
    /// char sums are block-independent). O(1) per delta.
    pub fn on_message_update(&mut self, assistant_message_event: &Value) {
        let event_type = assistant_message_event.get("type").and_then(Value::as_str);
        let Some(delta) = assistant_message_event.get("delta").and_then(Value::as_str) else {
            return;
        };
        let chars = delta.chars().count() as u64;
        match event_type {
            Some("text_delta") => self.text_chars += chars,
            Some("thinking_delta") => self.thinking_chars += chars,
            Some("toolcall_delta") => self.toolcall_chars += chars,
            _ => return,
        }
        self.pending_delta_chars += chars;
    }

    /// FR-C/FR-D: assistant `message_end` — capture the exact provider
    /// output tokens (n > 0 guard), stop streaming, freeze elapsed.
    pub fn on_message_end(&mut self, message: &Value) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        if let Some(output) = message.pointer("/usage/output").and_then(Value::as_u64) {
            if output > 0 {
                self.output_tokens_exact = Some(output);
            }
        }
        if let Some(started) = self.message_started_at {
            self.message_ended_elapsed_ms = Some(started.elapsed().as_millis());
        }
        self.streaming = false;
    }

    /// Whether an assistant stream is in flight (arms the live ticker).
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Whether any measurement was ever taken (drives block omission:
    /// `liveTokens` unconfigured or nothing streamed → no `live_output`).
    pub fn ever_measured(&self) -> bool {
        self.message_started_at.is_some()
    }

    /// Fingerprint of the measurements WITHOUT advancing the delta
    /// window (the live-ticker dirty check; §2.3 payload.rs "快照指纹").
    /// `elapsed` recomputed at `now` — while streaming it always changes
    /// (a stalled stream still re-runs so the script can zero the rate,
    /// FR-E); once frozen (message_end) it is stable.
    pub fn peek_fingerprint(&self, now: Instant) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{:?}|{:?}",
            self.streaming,
            self.text_chars,
            self.thinking_chars,
            self.toolcall_chars,
            self.pending_delta_chars,
            self.elapsed_ms(now),
            self.output_tokens_exact,
            self.message_started_at
        )
    }

    fn elapsed_ms(&self, now: Instant) -> u128 {
        if self.streaming {
            self.message_started_at
                .map_or(0, |started| now.duration_since(started).as_millis())
        } else {
            self.message_ended_elapsed_ms.unwrap_or(0)
        }
    }

    /// Advance the delta window and project the §1.6 block — called at
    /// the payload-assembly instant (FR-B: the snapshot base IS this
    /// script run's input). First advance bases on the message start.
    pub fn snapshot(&mut self, now: Instant) -> LiveSnapshot {
        let base = self.advanced_at.or(self.message_started_at);
        let delta_ms = base.map_or(0, |base| now.duration_since(base).as_millis());
        let delta_chars = self.pending_delta_chars;
        self.pending_delta_chars = 0;
        self.advanced_at = Some(now);
        LiveSnapshot {
            streaming: self.streaming,
            text_chars: self.text_chars,
            thinking_chars: self.thinking_chars,
            toolcall_chars: self.toolcall_chars,
            delta_chars,
            delta_ms,
            elapsed_ms: self.elapsed_ms(now),
            output_tokens_exact: self.output_tokens_exact,
        }
    }
}

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
    /// `rpi.live_output` measurements (§1.6); `None` = omit the block
    /// (`liveTokens` unconfigured or nothing streamed yet).
    pub live_output: Option<LiveSnapshot>,
    /// CC `hook_event_name`: "Status" for regular runs (TE12); the real
    /// event name for live-tick runs during streaming (FR-F).
    pub hook_event_name: String,
}

/// Mutable engine state, shared between the dispatch thread (event-driven
/// updates, µs-scale) and the refresh loop (snapshot reads) through a
/// `Mutex` — the subagents static-`STATE` precedent.
#[derive(Debug)]
pub struct EngineState {
    totals: Totals,
    last_usage: Option<Totals>,
    session_started_at: Instant,
    /// Wall-clock session start — the mtime floor for the transcript latch
    /// (see [`Self::ensure_transcript`]). `Instant` cannot be compared
    /// against file mtimes, hence the second clock.
    session_started_wall: SystemTime,
    /// Sticky transcript latch: `(cwd at latch time, resolved file)`.
    transcript: Option<(PathBuf, PathBuf)>,
    session_id: Option<String>,
    /// Authoritative session identity from `ctx.sessionFile` (FR-I,
    /// ADR-0022): `(path, id)` — `path: None` for in-memory sessions.
    /// While present it wins over the directory heuristic; cleared on
    /// session lifecycle events and re-fetched every tick.
    authoritative_session: Option<(Option<String>, String)>,
    /// Live streaming measurements (§1.4); absent when `liveTokens` is
    /// not configured (the subscription set stays the TE12 eight).
    pub live: Option<LiveMeasure>,
    /// Which rendering channel currently carries our output (`None` = the
    /// built-in footer is untouched) — needed to restore on config removal
    /// (FR-H).
    pub mounted: Option<Placement>,
}

/// Mtime tolerance below the session-start wall clock: the host creates
/// the session file shortly BEFORE emitting `session_start`, so the new
/// file's mtime can precede the event's arrival by a moment.
const LATCH_MTIME_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Default for EngineState {
    fn default() -> Self {
        Self {
            totals: Totals::default(),
            last_usage: None,
            session_started_at: Instant::now(),
            session_started_wall: SystemTime::now(),
            transcript: None,
            session_id: None,
            authoritative_session: None,
            live: None,
            mounted: None,
        }
    }
}

impl EngineState {
    /// Whether live-token bookkeeping is armed (subscription-time decision,
    /// fixed at install; §1.5 启用时机注记).
    pub fn arm_live_measure(&mut self) {
        self.live.get_or_insert_with(LiveMeasure::default);
    }

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
    /// FR-D: the live measurements reset with the session (an in-flight
    /// stream belongs to the previous session's run).
    pub fn on_session_start(&mut self, reason: Option<&str>) {
        match reason {
            Some("reload") => {
                self.transcript = None;
                self.authoritative_session = None;
                if let Some(live) = self.live.as_mut() {
                    *live = LiveMeasure::default();
                }
            }
            _ => {
                self.totals = Totals::default();
                self.last_usage = None;
                self.session_started_at = Instant::now();
                self.session_started_wall = SystemTime::now();
                self.transcript = None;
                self.authoritative_session = None;
                if let Some(live) = self.live.as_mut() {
                    *live = LiveMeasure::default();
                }
            }
        }
    }

    /// Record the authoritative session identity from a successful
    /// `ctx.sessionFile` call (FR-I). While set, the directory heuristic
    /// is bypassed entirely — same-cwd sibling instances can no longer
    /// win an mtime race (TE-D34 §1 / A8).
    pub fn set_authoritative_session(&mut self, path: Option<String>, id: String) {
        self.authoritative_session = Some((path, id));
    }

    /// Ensure a valid transcript latch: (re)resolve when absent, when the
    /// cwd moved, or when the latched file disappeared (TE-D34 sticky
    /// latch). Never fails — an unresolvable directory (or one whose files
    /// all predate this session — the new-session latch race) just leaves
    /// the latch empty, retried on the next tick; stdin JSON omits
    /// `transcript_path` until then.
    pub fn ensure_transcript(&mut self, cwd: &Path, settings_session_dir: Option<&str>) {
        let needs_reresolve = match &self.transcript {
            None => true,
            Some((latched_cwd, path)) => latched_cwd != cwd || !path.exists(),
        };
        if !needs_reresolve {
            return;
        }
        let dir = resolve_session_dir(cwd, settings_session_dir);
        let since = self
            .session_started_wall
            .checked_sub(LATCH_MTIME_GRACE)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        self.transcript = find_latest_session_file(&dir, since).map(|path| (cwd.to_owned(), path));
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

    /// The authoritative `(path, id)` from `ctx.sessionFile`, when a
    /// recent call succeeded (FR-I).
    pub fn authoritative_session(&self) -> Option<&(Option<String>, String)> {
        self.authoritative_session.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Serializes the two tests that mutate the process-global
    /// `RPI_CODING_AGENT_SESSION_DIR` (parallel set/remove_var races).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn delta(kind: &str, text: &str) -> Value {
        json!({"type": kind, "contentIndex": 0, "delta": text})
    }

    #[test]
    fn live_measure_accumulates_by_category_and_skips_start_end() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        // text/thinking/toolcall deltas land in their own accumulators;
        // *_start/*_end events carry no delta and add nothing (FR-A).
        live.on_message_update(&delta("text_start", ""));
        live.on_message_update(&delta("text_delta", "hello "));
        live.on_message_update(&delta("text_delta", "world"));
        live.on_message_update(&delta("thinking_delta", "pondering…"));
        live.on_message_update(&delta("toolcall_delta", "{\"a\":1}"));
        live.on_message_update(&delta("text_end", "ignored"));
        live.on_message_update(&json!({"type": "done"}));
        let snapshot = live.snapshot(Instant::now());
        assert_eq!(snapshot.text_chars, 11);
        assert_eq!(snapshot.thinking_chars, 10);
        assert_eq!(snapshot.toolcall_chars, 7);
        assert_eq!(snapshot.delta_chars, 11 + 10 + 7);
        assert!(snapshot.streaming);
        // Chars, not bytes: "…" is 3 bytes but 1 char.
        assert_eq!("pondering…".chars().count(), 10);
    }

    #[test]
    fn live_measure_delta_window_survives_message_reset() {
        // FR-B: the delta accumulator is independent of the per-message
        // block reset — a delta window spanning a message boundary is not
        // corrupted by it.
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("text_delta", "aaa"));
        live.on_message_end(&json!({"role": "assistant", "usage": {"output": 42}}));
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("text_delta", "bb"));
        let snapshot = live.snapshot(Instant::now());
        // Block accumulators reset (second message only)…
        assert_eq!(snapshot.text_chars, 2);
        // …but the pending delta window spans both messages.
        assert_eq!(snapshot.delta_chars, 5);
        assert!(snapshot.streaming);
    }

    #[test]
    fn live_measure_snapshot_advances_and_rebases_delta_window() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        // First advance bases the delta window on the message start.
        let first = live.snapshot(Instant::now());
        assert_eq!(first.delta_chars, 0);
        std::thread::sleep(std::time::Duration::from_millis(15));
        live.on_message_update(&delta("text_delta", "0123456789"));
        std::thread::sleep(std::time::Duration::from_millis(15));
        let second = live.snapshot(Instant::now());
        assert_eq!(second.delta_chars, 10);
        assert!(
            second.delta_ms >= 25,
            "monotonic window: {}",
            second.delta_ms
        );
        // The advance cleared the pending window.
        let third = live.snapshot(Instant::now());
        assert_eq!(third.delta_chars, 0);
        assert!(third.delta_ms < second.delta_ms);
    }

    #[test]
    fn live_measure_message_end_freezes_and_guards_exact_tokens() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("text_delta", "abc"));
        // Zero usage (aborted/error/provider without usage) keeps None
        // (FR-C: otherwise "exact first" scripts would render ↓0).
        live.on_message_end(&json!({"role": "assistant", "usage": {"output": 0}}));
        let mut snapshot = live.snapshot(Instant::now());
        assert_eq!(snapshot.output_tokens_exact, None);
        assert!(!snapshot.streaming);
        assert!(snapshot.elapsed_ms < 5, "frozen at message_end, not now");
        // Non-assistant message_end does not stop the stream.
        live.on_message_start(Some("assistant"));
        live.on_message_end(&json!({"role": "user"}));
        snapshot = live.snapshot(Instant::now());
        assert!(snapshot.streaming);
        // n > 0 lands.
        live.on_message_end(&json!({
            "role": "assistant",
            "usage": {"input": 10, "output": 77},
        }));
        snapshot = live.snapshot(Instant::now());
        assert_eq!(snapshot.output_tokens_exact, Some(77));
        assert!(!snapshot.streaming);
    }

    #[test]
    fn live_measure_non_assistant_start_is_ignored_and_fingerprint_moves() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("user"));
        assert!(!live.ever_measured());
        assert!(!live.is_streaming());
        live.on_message_start(Some("assistant"));
        assert!(live.ever_measured() && live.is_streaming());
        let before = live.peek_fingerprint(Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(5));
        let after = live.peek_fingerprint(Instant::now());
        assert_ne!(before, after, "elapsed drives the streaming fingerprint");
        live.on_message_end(&json!({"role": "assistant"}));
        let frozen = live.peek_fingerprint(Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(
            frozen,
            live.peek_fingerprint(Instant::now()),
            "frozen after message_end — poll stays quiet"
        );
    }

    #[test]
    fn session_start_resets_live_measure_and_authoritative_identity() {
        let mut engine = EngineState::default();
        engine.arm_live_measure();
        engine
            .live
            .as_mut()
            .unwrap()
            .on_message_start(Some("assistant"));
        engine.set_authoritative_session(Some("/a/b.jsonl".into()), "sid-1".into());
        engine.on_session_start(Some("new"));
        assert_eq!(engine.authoritative_session(), None);
        assert!(!engine
            .live
            .as_ref()
            .is_some_and(|live| live.ever_measured()));
        // reload keeps totals but resets live + authoritative too.
        engine.set_authoritative_session(None, "sid-2".into());
        engine.on_session_start(Some("reload"));
        assert_eq!(engine.authoritative_session(), None);
    }

    #[test]
    fn authoritative_session_wins_over_heuristic_latch() {
        // FR-I / A8: with ctx.sessionFile answered, the mtime-newest
        // sibling file must NOT win — no heuristic call is made at all.
        let mut engine = EngineState::default();
        engine.set_authoritative_session(
            Some("/sessions/mine.jsonl".into()),
            "018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e".into(),
        );
        assert_eq!(
            engine.authoritative_session(),
            Some(&(
                Some("/sessions/mine.jsonl".to_owned()),
                "018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e".to_owned()
            ))
        );
    }

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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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

    #[test]
    fn new_session_does_not_latch_previous_sessions_file() {
        // TE12 follow-up regression: at a new session's first tick only the
        // previous session's file exists — the latch must stay empty
        // (stdin omits transcript_path, the script shows zero totals)
        // rather than locking onto the stale file for the whole session.
        let dir =
            std::env::temp_dir().join(format!("rpi-statusline-state-race-{}", std::process::id()));
        let session_dir = dir.join("sessions").join("--x--");
        std::fs::create_dir_all(&session_dir).expect("mkdir");
        let stale =
            session_dir.join("2026-08-19T10-00-00-000_11111111-2222-3333-4444-555555555555.jsonl");
        std::fs::write(&stale, b"{}").expect("write");
        let old_time = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .append(true)
            .open(&stale)
            .expect("open")
            .set_modified(old_time)
            .expect("backdate mtime");

        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", &session_dir);
        let mut state = EngineState::default();
        state.ensure_transcript(Path::new("/cwd"), None);
        assert_eq!(
            state.transcript_path(),
            None,
            "stale previous-session file must not be latched"
        );
        assert_eq!(state.session_id(), None);

        // This session's file lands on disk → the next tick latches it.
        let current =
            session_dir.join("2026-08-19T11-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl");
        std::fs::write(&current, b"{}").expect("write");
        state.ensure_transcript(Path::new("/cwd"), None);
        assert_eq!(state.transcript_path(), Some(current.as_path()));
        std::env::remove_var("RPI_CODING_AGENT_SESSION_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }
}
