//! Accumulated session state: usage/cost totals, session clock, transcript
//! latch, mounted-rendering channel (TE12 FR-K).
//!
//! The totals mirror the host's own footer accounting
//! (`rpi/src/core/usage_totals.rs:12-28` `add_usage_to_totals` — including
//! `cost = usage.cost.total`), fed exclusively by `message_end` events the
//! host forwards for THIS session's assistant messages (subagents children
//! run their own process with their own extension host, so no sidechain
//! filtering is needed — TE12 verification §1).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde::Serialize;
use serde_json::Value;

use crate::config::{LiveCalculation, Placement};
use crate::fields::{FieldId, FieldValue, LiveView};
use crate::git::GitCache;
use crate::live_total::{AwaitingLocal, LiveTotalState, LiveTotalView, Settlement};
use crate::paths::{find_latest_session_file, resolve_session_dir, session_id_from_path};
use crate::session_stats::SessionStatsCache;
use crate::tokens::TokenSource;

/// Live streaming measurements for the CURRENT assistant message
/// (03-realtime-token-count §1.4/§2.3): native measures, python computes.
/// All values are raw measurements — no token conversion, no rate.
/// Since #45 the raw material rides along too: the cumulative raw text
/// per category and the provider-reported streaming output tokens —
/// still measurements; any (language-aware) token estimation stays in
/// the script.
#[derive(Debug, Default)]
pub struct LiveMeasure {
    /// An assistant message is in flight (`message_start` seen, matching
    /// `message_end` not yet; FR-D: true for the brief window before the
    /// first delta too — "this message has started").
    streaming: bool,
    text_chars: u64,
    thinking_chars: u64,
    toolcall_chars: u64,
    /// Cumulative raw text of the current message per category (#45 B):
    /// the concatenation of every `*_delta` since `message_start`, reset
    /// per message, frozen after `message_end`. Deltas build exactly the
    /// partial's block bodies, so `text.chars().count() == text_chars`
    /// always holds (the invariant is asserted in tests). Known benign
    /// gaps vs the authoritative `message_end` content: no signatures,
    /// no block structure (multi-block concatenation), redacted thinking
    /// absent — none affect the script-side token estimation this exists
    /// for.
    text: String,
    thinking: String,
    toolcall: String,
    /// Cumulative provider-reported output tokens of the current message
    /// (#45 A), read from the `message_update` cumulative partial's
    /// `usage.output` — OpenAI-compatible chunk usage and Anthropic
    /// `message_delta` usage both land there. `None` while the provider
    /// reports nothing (0/absent); a reported value is never overwritten
    /// by later silence.
    streaming_output_tokens: Option<u64>,
    /// Wall clock of the FIRST delta of the current message (#45
    /// companion): the TTFT-exclusion anchor scripts anchor the speed
    /// clock on, jitter-free vs observing the first non-zero char count
    /// at refresh cadence. `Instant` cannot serve — scripts compare
    /// against their own wall clock.
    decode_started_at: Option<SystemTime>,
    /// Monotonic twin of [`Self::decode_started_at`] (#50): the duration
    /// anchor `decode_ms` is measured from. Set at the same instant as
    /// the wall-clock anchor; `Instant` because `decode_ms` is
    /// host-computed (never compared against a script clock) and must
    /// stay monotonic across NTP/wall-clock jumps — and because
    /// `snapshot` samples it with the SAME `now` as `elapsed_ms`, making
    /// `TTFT = elapsed_ms − decode_ms` exact by construction.
    decode_started: Option<Instant>,
    /// Frozen decode duration at `message_end` (#50): first delta → end,
    /// measured on the same monotonic clock as the streaming value, so
    /// idle ticks recompute the identical rate from the unchanged frozen
    /// payload (scripts stay pure functions — no cached denominator).
    message_ended_decode_ms: Option<u128>,
    /// Monotonic per-LiveMeasure message counter (#45): serialized as
    /// `message_id`. Deliberately NOT reset by `on_message_start` — its
    /// job is to CHANGE there, so stateless scripts detect the message
    /// switch and reset their own smoothing state.
    message_seq: u64,
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
    /// Numerator source of the last PUBLISHED streaming rate frame (rpi#78 B):
    /// a switch between frames means the ratio's numerator changed meaning, so
    /// that frame must not publish a rate.
    last_rate_source: Option<TokenSource>,
    /// How the finished message was settled at `message_end` (V19-07): the
    /// authoritative breakdown the frozen frame shows once the refresh loop
    /// has run the official full tokenizer (or kept the final heuristic).
    /// `None` while the settlement is still pending.
    settled_tokens: Option<crate::tokens::TokenBreakdown>,
    /// `stopReason` of the finished message (`aborted`/`error` drop the final
    /// heuristic, R5.2.7) and whether an `errorMessage` was present.
    stopped_normally: bool,
}

/// Cheap live status for the items live tick (V19-07 FR-G R1): timing,
/// provider usage and generations WITHOUT the full text. The refresh loop
/// feeds its incremental counter from [`LiveMeasure::unprocessed_text`] (only
/// the bytes that arrived since the last tick) and never deep-copies the
/// message per refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveStatus {
    pub streaming: bool,
    pub output_tokens: Option<u64>,
    pub output_tokens_exact: Option<u64>,
    pub decode_ms: u128,
    pub message_seq: u64,
    /// Byte length of each cumulative buffer (text, thinking, toolcall).
    pub text_lens: [usize; 3],
}

/// Read-side projection of [`LiveMeasure`] at one payload assembly
/// instant (the §1.6 `rpi.live_output` block, snake_case). Carries the
/// #45 raw material (cumulative text, streaming usage, decode anchor,
/// message id) alongside the original counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LiveSnapshot {
    pub streaming: bool,
    /// `message_id`: plugin-local monotonic id, changes per message.
    pub message_id: String,
    /// Provider-reported cumulative output tokens while streaming
    /// (#45 A); `None` = provider not reporting (field omitted).
    pub output_tokens: Option<u64>,
    /// Cumulative raw text of the current message (#45 B).
    pub text: String,
    pub thinking: String,
    pub toolcall: String,
    /// Wall-clock epoch ms of the first delta (#45 companion); `None`
    /// until the first delta (field omitted).
    pub decode_started_at_ms: Option<u64>,
    /// Decode duration ms: first delta → now while `streaming`, FROZEN
    /// at `message_end` (#50). Always present, `0` until the first
    /// delta — same always-present-with-0 discipline as `delta_ms` /
    /// `elapsed_ms` (durations, not #45 Optional raw material), so
    /// scripts index it unconditionally and the `decode_s < 0.5`-style
    /// guard absorbs the pre-first-delta window.
    pub decode_ms: u128,
    pub text_chars: u64,
    pub thinking_chars: u64,
    pub toolcall_chars: u64,
    pub delta_chars: u64,
    pub delta_ms: u128,
    pub elapsed_ms: u128,
    pub output_tokens_exact: Option<u64>,
    /// Tiered token count (provider exact or bundled tokenizer/heuristic).
    /// Filled by the caller after the engine lock is released; `None` until
    /// the first snapshot with text data.
    pub tokens: Option<crate::tokens::TokenBreakdown>,
}

/// A cheap clone of the live measurement used to count tokens **outside**
/// the engine lock (the dispatch thread must never wait on a tokenizer).
#[derive(Debug, Clone, Default)]
pub struct LivePeek {
    pub streaming: bool,
    pub output_tokens: Option<u64>,
    pub output_tokens_exact: Option<u64>,
    pub decode_ms: u128,
    pub text: String,
    pub thinking: String,
    pub toolcall: String,
}

impl LivePeek {
    /// Project the peek plus a token breakdown into the fields' `LiveView`.
    /// Full-text accounting (command mode, editor preview) — the items live
    /// tick feeds its incremental counter instead (V19-07 FR-G R1).
    pub fn into_view(self, model_id: Option<&str>, policy: LiveCalculation) -> LiveView {
        let tokens = match policy {
            LiveCalculation::Auto => {
                crate::tokens::count_breakdown(model_id, &self.text, &self.thinking, &self.toolcall)
            }
            LiveCalculation::Estimate => {
                crate::tokens::heuristic_breakdown(&self.text, &self.thinking, &self.toolcall)
            }
        };
        LiveView {
            streaming: self.streaming,
            output_tokens: self.output_tokens,
            output_tokens_exact: self.output_tokens_exact,
            decode_ms: self.decode_ms,
            tokens: Some(tokens),
            rate_source_switched: false,
            policy,
            settled: false,
        }
    }
}

impl LiveMeasure {
    /// FR-D: an assistant `message_start` resets the per-message block
    /// accumulators and clock; the delta window base is untouched. #45:
    /// the raw-text buffers, streaming usage and decode anchor reset with
    /// the block; `message_seq` moves instead of resetting (see field).
    pub fn on_message_start(&mut self, role: Option<&str>) {
        if role != Some("assistant") {
            return;
        }
        self.begin_stream();
    }

    /// Begin a message block without a `message_start` event. This covers
    /// two real cases: the user enabled `liveTokens` while a response was
    /// already streaming (the subscription was added mid-flight), and a
    /// dropped `message_start` after a session host rebind. The first delta
    /// observed becomes the block/clock anchor, so live tokens and rate start
    /// updating instead of waiting for the next message.
    fn begin_stream(&mut self) {
        self.streaming = true;
        self.text_chars = 0;
        self.thinking_chars = 0;
        self.toolcall_chars = 0;
        self.text.clear();
        self.thinking.clear();
        self.toolcall.clear();
        self.streaming_output_tokens = None;
        self.decode_started_at = None;
        self.decode_started = None;
        self.message_seq += 1;
        self.message_started_at = Some(Instant::now());
        self.message_ended_elapsed_ms = None;
        self.message_ended_decode_ms = None;
        self.output_tokens_exact = None;
        self.last_rate_source = None;
        self.settled_tokens = None;
        self.stopped_normally = true;
    }

    /// FR-A: per-delta bookkeeping — only `assistantMessageEvent.{type,
    /// delta}` is counted; `*_start`/`*_end` events carry no delta and add
    /// nothing. Category by event type (no contentIndex dimension —
    /// char sums are block-independent). O(1) amortized per delta.
    ///
    /// #45 takes the WHOLE event payload: besides the delta counters it
    /// retains the raw delta text (buffer append) and reads the cumulative
    /// partial's `usage.output` (`{message, assistantMessageEvent}` — the
    /// host forwards the partial verbatim, provider running totals
    /// included; zero/absent = not reporting, never overwrite a reported
    /// value).
    pub fn on_message_update(&mut self, payload: &Value) {
        let event_type = payload
            .pointer("/assistantMessageEvent/type")
            .and_then(Value::as_str);
        let Some(delta) = payload
            .pointer("/assistantMessageEvent/delta")
            .and_then(Value::as_str)
        else {
            return;
        };
        // A delta can arrive without a live message_start (mid-stream enable
        // or a dropped start event): treat it as the block/clock anchor so
        // the estimated tokens/rate update immediately.
        if !self.streaming {
            self.begin_stream();
        }
        let chars = delta.chars().count() as u64;
        match event_type {
            Some("text_delta") => {
                self.text_chars += chars;
                self.text.push_str(delta);
            }
            Some("thinking_delta") => {
                self.thinking_chars += chars;
                self.thinking.push_str(delta);
            }
            Some("toolcall_delta") => {
                self.toolcall_chars += chars;
                self.toolcall.push_str(delta);
            }
            _ => return,
        }
        if self.decode_started_at.is_none() {
            self.decode_started_at = Some(SystemTime::now());
            self.decode_started = Some(Instant::now());
        }
        self.pending_delta_chars += chars;
        // Two payload shapes are accepted (rpi#78 A1/1.2): the legacy
        // cumulative `{message:{usage}}` and the delta-only `{usage}` (the
        // cumulative message is dropped there, which is what makes the host
        // path O(delta)).
        let usage = payload
            .pointer("/message/usage/output")
            .or_else(|| payload.pointer("/usage/output"))
            .and_then(Value::as_u64);
        if let Some(output) = usage
            && output > 0
        {
            self.streaming_output_tokens = Some(output);
        }
    }

    /// Whether a finished message completed normally: `stopReason` is neither
    /// `aborted` nor `error` and no `errorMessage` is set (R5.2.7 — only a
    /// normal completion may keep its final heuristic as `settled_estimated`).
    fn completed_normally(message: &Value) -> bool {
        let stop = message.get("stopReason").and_then(Value::as_str);
        let errored = message
            .get("errorMessage")
            .is_some_and(|value| !value.is_null());
        !matches!(stop, Some("aborted") | Some("error")) && !errored
    }

    /// FR-C/FR-D: assistant `message_end` — capture the exact provider
    /// output tokens (n > 0 guard), stop streaming, freeze elapsed.
    pub fn on_message_end(&mut self, message: &Value) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        self.stopped_normally = Self::completed_normally(message);
        if let Some(output) = message.pointer("/usage/output").and_then(Value::as_u64)
            && output > 0
        {
            self.output_tokens_exact = Some(output);
        }
        if let Some(started) = self.message_started_at {
            self.message_ended_elapsed_ms = Some(started.elapsed().as_millis());
        }
        // #50: freeze the decode duration on the same monotonic clock the
        // streaming snapshots used (Instant twin anchor) — the frozen
        // `elapsed_ms − decode_ms` pair stays an exact TTFT.
        if let Some(decode) = self.decode_started {
            self.message_ended_decode_ms = Some(decode.elapsed().as_millis());
        }
        self.streaming = false;
    }

    /// Whether an assistant stream is in flight (arms the live ticker).
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Project the current live values. Token accounting is *not* done here:
    /// the caller clones this peek and counts outside the engine lock so a
    /// large tokenizer run never blocks the streaming dispatch thread.
    pub fn peek(&self, now: Instant) -> LivePeek {
        LivePeek {
            streaming: self.streaming,
            output_tokens: self.streaming_output_tokens,
            output_tokens_exact: self.output_tokens_exact,
            decode_ms: self.decode_ms(now),
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            toolcall: self.toolcall.clone(),
        }
    }

    /// The settled breakdown of the finished message (V19-07), if the refresh
    /// loop has settled it yet.
    pub fn settled_tokens(&self) -> Option<crate::tokens::TokenBreakdown> {
        self.settled_tokens
    }

    pub fn set_settled_tokens(&mut self, tokens: Option<crate::tokens::TokenBreakdown>) {
        self.settled_tokens = tokens;
    }

    /// Whether the finished message completed normally (aborted/errored
    /// messages drop their final heuristic).
    pub fn stopped_normally(&self) -> bool {
        self.stopped_normally
    }

    /// Cheap status for the items live tick: no text is copied.
    pub fn status(&self, now: Instant) -> LiveStatus {
        LiveStatus {
            streaming: self.streaming,
            output_tokens: self.streaming_output_tokens,
            output_tokens_exact: self.output_tokens_exact,
            decode_ms: self.decode_ms(now),
            message_seq: self.message_seq,
            text_lens: [self.text.len(), self.thinking.len(), self.toolcall.len()],
        }
    }

    /// The bytes of each cumulative buffer past `processed` (the delta-only
    /// feed of the incremental counter): copies only what is new.
    pub fn unprocessed_text(&self, processed: [usize; 3]) -> [String; 3] {
        let tail = |text: &str, from: usize| -> String {
            let from = from.min(text.len());
            // Snap to a char boundary (the counter tracks byte offsets that
            // always land on one; a reset message restarts from 0).
            let mut from = from;
            while from > 0 && !text.is_char_boundary(from) {
                from -= 1;
            }
            text[from..].to_owned()
        };
        [
            tail(&self.text, processed[0]),
            tail(&self.thinking, processed[1]),
            tail(&self.toolcall, processed[2]),
        ]
    }

    /// One-time copy of the full text (hot-switch rebuild, `message_end`
    /// settlement, command-mode payload) — never on the per-refresh path of
    /// the items live tick.
    pub fn full_text(&self) -> [String; 3] {
        [
            self.text.clone(),
            self.thinking.clone(),
            self.toolcall.clone(),
        ]
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
            "{}|{}|{}|{}|{}|{}|{:?}|{:?}|{:?}|{}",
            self.streaming,
            self.text_chars,
            self.thinking_chars,
            self.toolcall_chars,
            self.pending_delta_chars,
            self.elapsed_ms(now),
            self.output_tokens_exact,
            self.message_started_at,
            self.streaming_output_tokens,
            self.message_seq
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

    /// #50: decode duration at the snapshot instant — live while
    /// streaming (same `now` as [`Self::elapsed_ms`], so the difference
    /// is the exact TTFT), frozen after `message_end`.
    fn decode_ms(&self, now: Instant) -> u128 {
        if self.streaming {
            self.decode_started
                .map_or(0, |started| now.duration_since(started).as_millis())
        } else {
            self.message_ended_decode_ms.unwrap_or(0)
        }
    }

    /// Advance the delta window and project the §1.6 block — called at
    /// the payload-assembly instant (FR-B: the snapshot base IS this
    /// script run's input). First advance bases on the message start.
    /// The #45 raw material is CLONED, not taken: cumulative text,
    /// streaming usage and the decode anchor belong to the message
    /// lifetime, not the delta window (only `pending_delta_chars` is
    /// window-scoped and cleared here).
    pub fn snapshot(&mut self, now: Instant) -> LiveSnapshot {
        let base = self.advanced_at.or(self.message_started_at);
        let delta_ms = base.map_or(0, |base| now.duration_since(base).as_millis());
        let delta_chars = self.pending_delta_chars;
        self.pending_delta_chars = 0;
        self.advanced_at = Some(now);
        LiveSnapshot {
            streaming: self.streaming,
            message_id: self.message_seq.to_string(),
            output_tokens: self.streaming_output_tokens,
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            toolcall: self.toolcall.clone(),
            decode_started_at_ms: self.decode_started_at.map(Self::epoch_millis),
            decode_ms: self.decode_ms(now),
            text_chars: self.text_chars,
            thinking_chars: self.thinking_chars,
            toolcall_chars: self.toolcall_chars,
            delta_chars,
            delta_ms,
            elapsed_ms: self.elapsed_ms(now),
            output_tokens_exact: self.output_tokens_exact,
            tokens: None,
        }
    }

    /// Wall-clock epoch ms for `decode_started_at_ms` (#45): scripts
    /// compare against their own clock. Pre-epoch clocks clamp to 0
    /// (never occurs in practice).
    fn epoch_millis(time: SystemTime) -> u64 {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
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
    /// Absolute `ctx.sessionStats` cache (items mode; TE52 R20 §4.2).
    pub session_stats: SessionStatsCache,
    /// Items `live-total-tokens` overlay (V19-07 / ADR-0049 decision 8):
    /// pending provider-exact totals with their persistence watermarks, the
    /// local exact/estimated settled sums and the current-message flag.
    pub live_total: LiveTotalState,
    /// Whether the items selection currently renders `live-total-tokens`
    /// (reconciled from the field selection on every poll — the `message_end`
    /// callback reads THIS cache and never the raw settings file, R5.2.8).
    /// `false` in command mode: command keeps its full `rpi.live_output`
    /// payload and never creates pending entries or probes.
    live_total_selected: bool,
    /// Recent terminal-probe durations (nanoseconds, most recent last) —
    /// the runtime leg of the R5.2.8 budget: a host whose counts-only probe
    /// is slow (an old host answering the FULL snapshot, or a regression)
    /// must not pay O(entries) per terminal, so the probe is replaced by the
    /// cached `assistant_messages + pending` lower bound (R5.2.8: never a
    /// fall back to the full stats).
    probe_durations_ns: VecDeque<u64>,
    /// How many terminals took the lower-bound path instead of a probe.
    probe_fallbacks: u64,
    /// Local Git probe cache for project-name/git-branch (TE52 R20 §4.4).
    pub git: GitCache,
    /// Items-mode last rendered value of every live row. Between two
    /// assistant messages the fresh measurement carries no data yet, and the
    /// rows keep these values instead of flickering to the dash placeholders
    /// ([`Self::record_live_values`] / [`Self::live_held_values`]).
    last_live_values: Vec<(FieldId, FieldValue)>,
    /// The host proved it emits the delta-only `message_update_delta`
    /// (rpi#78 A1/1.2). Once set, reconciliation drops the cumulative
    /// `message_update` subscription, so the host stops materializing the whole
    /// accumulated message per delta. Latched for the process: a host that
    /// emitted a delta event keeps doing so.
    delta_supported: bool,
}

/// A terminal counts-only probe slower than this (p95 of the recent probes)
/// sends the following terminals to the lower-bound path: the probe budget of
/// R5.2.8 is a few hundred nanoseconds on the current host, so a
/// millisecond-scale probe means the host answered with the full snapshot.
pub const PROBE_SLOW_NS: u64 = 2_000_000;
/// Recent probes kept for the p95 estimate.
const PROBE_SAMPLES: usize = 16;

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
            session_stats: SessionStatsCache::default(),
            live_total: LiveTotalState::default(),
            live_total_selected: false,
            probe_durations_ns: VecDeque::new(),
            probe_fallbacks: 0,
            git: GitCache::default(),
            last_live_values: Vec::new(),
            delta_supported: false,
        }
    }
}

impl EngineState {
    /// Arm live-token bookkeeping (reconciled from the current config on
    /// every poll; no restart needed).
    pub fn arm_live_measure(&mut self) {
        self.live.get_or_insert_with(LiveMeasure::default);
    }

    /// Items mode: dynamic live membership. Disabling clears the measurement
    /// immediately (R20 §4.4) so a later re-enable cannot show stale tokens —
    /// the held between-messages view is dropped for the same reason.
    pub fn set_live_measure(&mut self, enabled: bool) {
        if enabled {
            self.live.get_or_insert_with(LiveMeasure::default);
        } else {
            self.live = None;
            self.last_live_values.clear();
        }
    }

    /// Items mode: the values the previous frame rendered for the live rows,
    /// attached to every frame so [`crate::fields::value`] can hold a row
    /// that has no fresh reading yet.
    pub fn live_held_values(&self) -> Vec<(FieldId, FieldValue)> {
        self.last_live_values.clone()
    }

    /// Items mode: remember this frame's fresh live readings. Only fields
    /// with an actual measurement are stored, so a row keeps the last value
    /// it really had: the gap between two assistant messages (the measurement
    /// resets at `message_start` and has no delta yet), the rate window
    /// before 0.5 s, or a tier the current message has not produced. A
    /// SETTLED frame (message_end) replaces the whole cache first: its frozen
    /// measurements are authoritative, so a row the finished message could
    /// not produce must not keep an older message's number.
    ///
    /// `streaming` is deliberately not stored: during a real gap it must not
    /// keep claiming a stream is running (its dash placeholder is the
    /// truthful state, and the fresh value covers the in-flight window).
    pub fn record_live_values(&mut self, data: &crate::fields::FieldData) {
        if data.live.is_some_and(|live| !live.streaming) {
            self.last_live_values.clear();
        }
        for id in crate::fields::ALL_FIELDS
            .iter()
            .copied()
            .filter(|id| id.is_live() && *id != FieldId::Streaming)
        {
            let Some(value) = crate::fields::fresh_value(id, data) else {
                continue;
            };
            match self
                .last_live_values
                .iter_mut()
                .find(|(held, _): &&mut (FieldId, FieldValue)| *held == id)
            {
                Some(entry) => entry.1 = value,
                None => self.last_live_values.push((id, value)),
            }
        }
    }

    /// Project the current live measurement (items fields; `None` when the
    /// measurement is disarmed). This is a cheap clone; token counting is
    /// done by the caller via [`LiveMeasure::peek`].
    /// Record the numerator source of a frame about to be published and report
    /// whether the streaming frame's source just changed (rpi#78 B).
    ///
    /// A settled frame never counts as a switch: numerator and denominator are
    /// both frozen there, so the value is the message's own average (the
    /// authoritative `usage.output` correction is exactly what should show).
    pub fn note_rate_source(
        &mut self,
        streaming: bool,
        source: Option<crate::tokens::TokenSource>,
    ) -> bool {
        let Some(live) = self.live.as_mut() else {
            return false;
        };
        if !streaming {
            live.last_rate_source = None;
            return false;
        }
        let switched = live
            .last_rate_source
            .is_some_and(|last| Some(last) != source);
        live.last_rate_source = source;
        switched
    }

    /// Whether the host emits the delta-only `message_update_delta`
    /// (rpi#78 A1/1.2). Latched on the first delta event.
    pub fn delta_supported(&self) -> bool {
        self.delta_supported
    }

    /// Record that the host emitted a delta-only `message_update` payload.
    pub fn note_delta_supported(&mut self) {
        self.delta_supported = true;
    }

    pub fn live_peek(&self, now: Instant) -> Option<LivePeek> {
        self.live.as_ref().map(|live| live.peek(now))
    }

    /// The items `live-total-tokens` overlay projection for one frame.
    pub fn live_total_view(&self) -> LiveTotalView {
        self.live_total.view()
    }

    /// Items mode: record whether `live-total-tokens` is selected (updated by
    /// the reconciliation on every poll; read by the `message_end` callback).
    pub fn set_live_total_selected(&mut self, selected: bool) {
        self.live_total_selected = selected;
    }

    pub fn live_total_selected(&self) -> bool {
        self.live_total_selected
    }

    /// `message_start`: a new assistant message generation (the baseline,
    /// earlier pending entries and the local settled sums are untouched).
    pub fn on_message_start(&mut self, role: Option<&str>) {
        if role == Some("assistant") {
            self.live_total.begin_message();
        }
        if let Some(live) = self.live.as_mut() {
            live.on_message_start(role);
        }
    }

    /// `message_update`: per-delta bookkeeping; a stream that starts without
    /// a `message_start` (mid-stream enable, dropped start) opens the message
    /// generation on its first delta.
    pub fn on_message_update(&mut self, payload: &Value) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let was_streaming = live.is_streaming();
        live.on_message_update(payload);
        if !was_streaming && live.is_streaming() {
            self.live_total.begin_message();
        }
    }

    /// `message_end`: freeze the live measurement and settle the finished
    /// assistant message exactly once for the live session total (V19-07
    /// R5.2.7). Returns what the dispatch path must do next: run the one
    /// terminal counts-only probe (provider exact), hand the message to the
    /// refresh loop (local settlement), or nothing. Aborted/usage-less
    /// messages never inflate the committed total: their final heuristic is
    /// dropped unless they completed normally.
    pub fn on_message_end(&mut self, message: &Value) -> Settlement {
        if let Some(live) = self.live.as_mut() {
            live.on_message_end(message);
        }
        // Only assistant output feeds the live session total; other message
        // kinds may carry usage-shaped fields but are not model output.
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Settlement::None;
        }
        // Command mode / field not selected: no pending entries, no probe,
        // no local settlement — the overlay belongs to items only.
        if !self.live_total_selected {
            return Settlement::None;
        }
        let [text, thinking, toolcall] = match self.live.as_ref() {
            Some(live) => live.full_text(),
            None => [String::new(), String::new(), String::new()],
        };
        let awaiting = AwaitingLocal {
            text,
            thinking,
            toolcall,
            normal_completion: self
                .live
                .as_ref()
                .is_none_or(|live| live.stopped_normally()),
            message_generation: self.live_total.message_generation(),
        };
        let provider_output = message
            .pointer("/usage/output")
            .and_then(Value::as_u64)
            .filter(|output| *output > 0);
        self.live_total.on_message_end(
            provider_output,
            awaiting,
            self.session_stats.current_request_generation(),
        )
    }

    /// Whether the terminal probe should run: it is skipped once the recent
    /// probes' p95 exceeds [`PROBE_SLOW_NS`] (with at least half the samples
    /// in). The caller then records the lower-bound watermark through
    /// [`Self::record_lower_bound_watermark`].
    pub fn terminal_probe_wanted(&mut self) -> bool {
        let mut samples: Vec<u64> = self.probe_durations_ns.iter().copied().collect();
        samples.sort_unstable();
        if samples.len() < PROBE_SAMPLES / 2 {
            return true;
        }
        let p95 = samples[samples.len() * 95 / 100];
        p95 <= PROBE_SLOW_NS
    }

    /// Record the duration of a terminal probe (nanoseconds).
    pub fn note_terminal_probe_duration(&mut self, elapsed_ns: u64) {
        self.probe_durations_ns.push_back(elapsed_ns);
        while self.probe_durations_ns.len() > PROBE_SAMPLES {
            self.probe_durations_ns.pop_front();
        }
    }

    /// The lower-bound path of the budget: the watermark is the cached
    /// snapshot's `assistant_messages` plus every queued pending entry
    /// (each is one message the host is about to persist). It is cleared
    /// later than a probed watermark at worst — never earlier than the
    /// persistence, never through the full `sessionStats`.
    pub fn record_lower_bound_watermark(&mut self) {
        self.probe_fallbacks += 1;
        let base = self
            .session_stats
            .snapshot()
            .map_or(0, |stats| stats.assistant_messages);
        let pending = self.live_total.pending_len() as u64;
        self.live_total
            .record_probe(Some(base.saturating_add(pending.max(1) - 1)));
    }

    /// How many terminals took the lower-bound path (diagnostics).
    pub fn probe_fallbacks(&self) -> u64 {
        self.probe_fallbacks
    }

    /// Record the ONE terminal counts-only probe for the pending entry that
    /// [`Self::on_message_end`] just queued (`pre_persist_assistant_messages`
    /// or `None` when the probe failed → unknown watermark, field hidden).
    pub fn record_terminal_probe(&mut self, pre_persist_assistant: Option<u64>) {
        self.live_total.record_probe(pre_persist_assistant);
    }

    /// Take the finished message awaiting its local settlement (refresh loop;
    /// the official tokenizer run happens outside the engine lock).
    pub fn take_awaiting_local(&mut self) -> Option<AwaitingLocal> {
        self.live_total.take_awaiting_local()
    }

    /// Apply a local settlement and remember the settled breakdown for the
    /// frozen frame.
    pub fn settle_local(
        &mut self,
        settlement: crate::live_total::LocalSettlement,
        tokens: Option<crate::tokens::TokenBreakdown>,
    ) {
        self.live_total.settle_local(settlement);
        if let Some(live) = self.live.as_mut() {
            live.set_settled_tokens(tokens);
        }
    }

    /// Command mode: the official local breakdown of the FINISHED message,
    /// computed once at the first settled run and cached on the frozen
    /// measurement (V19-07: both policies publish the official full pass once
    /// the message settled; idle re-runs do not repeat the BPE).
    pub fn cached_settled_local(
        &mut self,
        model_id: Option<&str>,
    ) -> crate::tokens::TokenBreakdown {
        let Some(live) = self.live.as_mut() else {
            return crate::tokens::TokenBreakdown::default();
        };
        if let Some(tokens) = live.settled_tokens() {
            return tokens;
        }
        let [text, thinking, toolcall] = live.full_text();
        let tokens = crate::tokens::count_breakdown(model_id, &text, &thinking, &toolcall);
        live.set_settled_tokens(Some(tokens));
        tokens
    }

    /// Apply an applied snapshot's watermark to the pending entries.
    pub fn on_snapshot_applied(
        &mut self,
        assistant_messages: u64,
        applied: &crate::session_stats::AppliedRequest,
    ) {
        self.live_total
            .on_snapshot_applied(assistant_messages, applied);
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
        // The absolute session snapshot belongs to the previous session in
        // every case (reload included: identity is re-checked on the next
        // reply; the snapshot is never re-added).
        self.session_stats.begin_session(reason);
        // Every overlay generation belongs to the previous session (reload
        // included): pending watermarks, local settled sums and the current
        // message all start over.
        self.live_total.begin_session();
        // The held live footer belongs to the previous session's messages.
        self.last_live_values.clear();
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

    /// The dispatch payload shape: `{message, assistantMessageEvent}`
    /// (the host forwards the cumulative partial verbatim, #45 reads its
    /// usage); tests default to no usage reporting.
    fn delta(kind: &str, text: &str) -> Value {
        json!({"assistantMessageEvent": {"type": kind, "contentIndex": 0, "delta": text}})
    }

    /// A `message_update` payload whose cumulative partial carries
    /// provider running usage (#45 A).
    fn delta_with_usage(kind: &str, text: &str, output: u64) -> Value {
        json!({
            "message": {"role": "assistant", "usage": {"output": output}},
            "assistantMessageEvent": {"type": kind, "contentIndex": 0, "delta": text},
        })
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

    /// #45 B: the raw-text buffers are the cumulative per-category delta
    /// concatenation, byte-for-byte what the script needs for its own
    /// (language-aware) token estimation; `*_chars` stays in lockstep
    /// (the invariant), `message_id` moves per message, and the snapshot
    /// CLONES rather than drains (cumulative ≠ delta window).
    #[test]
    fn live_measure_retains_raw_text_and_moves_message_id_per_message() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("thinking_delta", "中文思考"));
        live.on_message_update(&delta("text_delta", "hello "));
        live.on_message_update(&delta("text_delta", "世界"));
        live.on_message_update(&delta("toolcall_delta", "{\"cmd\":true}"));
        let first = live.snapshot(Instant::now());
        assert_eq!(first.thinking, "中文思考");
        assert_eq!(first.text, "hello 世界");
        assert_eq!(first.toolcall, "{\"cmd\":true}");
        assert_eq!(first.message_id, "1");
        // Invariant: counters and buffers agree (chars, not bytes).
        assert_eq!(first.text.chars().count() as u64, first.text_chars);
        assert_eq!(first.thinking.chars().count() as u64, first.thinking_chars);
        assert_eq!(first.toolcall.chars().count() as u64, first.toolcall_chars);
        // The snapshot clones: a second advance sees the same cumulative
        // text (only the delta window drained).
        let second = live.snapshot(Instant::now());
        assert_eq!(second.text, first.text);
        assert_eq!(second.delta_chars, 0);
        // message_end freezes, the next message resets buffers + moves id.
        live.on_message_end(&json!({"role": "assistant"}));
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("text_delta", "next"));
        let third = live.snapshot(Instant::now());
        assert_eq!(third.text, "next");
        assert_eq!(third.thinking, "");
        assert_eq!(third.message_id, "2");
        // A non-assistant start neither resets nor moves the id.
        live.on_message_start(Some("user"));
        let fourth = live.snapshot(Instant::now());
        assert_eq!(fourth.message_id, "2");
        assert_eq!(fourth.text, "next");
    }

    /// #45 A: the cumulative partial's `usage.output` is the provider's
    /// running total — latched when > 0, never overwritten by later
    /// silence (0/absent), reset per message, frozen after message_end
    /// (where `output_tokens_exact` takes over).
    #[test]
    fn live_measure_latches_streaming_usage_with_zero_guard() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        // Not reporting yet → omitted.
        live.on_message_update(&delta("text_delta", "a"));
        assert_eq!(live.snapshot(Instant::now()).output_tokens, None);
        // Running totals land cumulatively.
        live.on_message_update(&delta_with_usage("text_delta", "b", 7));
        assert_eq!(live.snapshot(Instant::now()).output_tokens, Some(7));
        live.on_message_update(&delta_with_usage("text_delta", "c", 19));
        assert_eq!(live.snapshot(Instant::now()).output_tokens, Some(19));
        // Later silence (0 / absent usage) keeps the last report.
        live.on_message_update(&delta_with_usage("text_delta", "d", 0));
        live.on_message_update(&delta("text_delta", "e"));
        assert_eq!(live.snapshot(Instant::now()).output_tokens, Some(19));
        // message_end: exact takes over; streaming value frozen alongside.
        live.on_message_end(&json!({"role": "assistant", "usage": {"output": 23}}));
        let frozen = live.snapshot(Instant::now());
        assert_eq!(frozen.output_tokens, Some(19));
        assert_eq!(frozen.output_tokens_exact, Some(23));
        // Per-message reset.
        live.on_message_start(Some("assistant"));
        live.on_message_update(&delta("text_delta", "x"));
        assert_eq!(live.snapshot(Instant::now()).output_tokens, None);
    }

    #[test]
    fn live_measure_starts_from_the_first_observed_delta_mid_stream() {
        let mut live = LiveMeasure::default();
        assert!(!live.is_streaming());
        // No message_start was observed (liveTokens enabled mid-flight).
        live.on_message_update(&delta_with_usage("text_delta", "hello", 3));
        assert!(live.is_streaming(), "the first delta anchors the block");
        let snapshot = live.snapshot(Instant::now());
        assert_eq!(snapshot.text, "hello");
        assert_eq!(snapshot.output_tokens, Some(3));
        assert_eq!(snapshot.message_id, "1");
        // The block clock starts at that first delta, so decode_ms is small.
        assert!(snapshot.decode_ms < 5);
    }

    /// #45 companion: the decode anchor is the wall clock of the FIRST
    /// delta (any category), cleared per message; set before the first
    /// delta it is absent (scripts fall back to `elapsed_ms`).
    #[test]
    fn live_measure_decode_anchor_on_first_delta_only() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        // The pre-first-delta window carries no anchor yet.
        assert_eq!(live.snapshot(Instant::now()).decode_started_at_ms, None);
        let before = SystemTime::now();
        live.on_message_update(&delta("text_delta", "a"));
        let after = SystemTime::now();
        let anchor = live
            .snapshot(Instant::now())
            .decode_started_at_ms
            .expect("first delta sets the anchor");
        let clamp = |time: SystemTime| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0)
        };
        assert!(anchor >= clamp(before) && anchor <= clamp(after));
        // Non-delta events never arm it; later deltas never move it.
        live.on_message_update(&delta("text_start", ""));
        live.on_message_update(&delta("thinking_delta", "t"));
        assert_eq!(
            live.snapshot(Instant::now()).decode_started_at_ms,
            Some(anchor)
        );
        // Per-message reset.
        live.on_message_end(&json!({"role": "assistant"}));
        live.on_message_start(Some("assistant"));
        assert_eq!(live.snapshot(Instant::now()).decode_started_at_ms, None);
    }

    /// #50: `decode_ms` — 0 until the first delta (always present, never
    /// omitted), live-updating while streaming on the monotonic twin
    /// anchor, sampled at the SAME `now` as `elapsed_ms` (so
    /// `elapsed_ms − decode_ms` is the exact TTFT window, never
    /// negative), frozen at `message_end` (idle snapshots recompute the
    /// identical value — pure-function scripts), reset per message.
    #[test]
    fn live_measure_decode_ms_lives_then_freezes() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        // Pre-first-delta window: 0, present.
        assert_eq!(live.snapshot(Instant::now()).decode_ms, 0);
        // TTFT window: message start → first delta.
        std::thread::sleep(std::time::Duration::from_millis(10));
        live.on_message_update(&delta("text_delta", "a"));
        let armed = live.snapshot(Instant::now());
        assert!(
            armed.decode_ms < 5,
            "armed at the first delta: {}",
            armed.decode_ms
        );
        // Decode window grows; the same-instant difference is the TTFT.
        std::thread::sleep(std::time::Duration::from_millis(40));
        let live_snap = live.snapshot(Instant::now());
        assert!(
            live_snap.decode_ms >= 40,
            "live-updating while streaming: {}",
            live_snap.decode_ms
        );
        let ttft = live_snap.elapsed_ms - live_snap.decode_ms;
        assert!(
            (8..40).contains(&ttft),
            "TTFT = elapsed_ms − decode_ms is the start→first-delta window: {ttft}"
        );
        // message_end freezes; idle ticks recompute identical values.
        live.on_message_end(&json!({"role": "assistant", "usage": {"output": 9}}));
        let frozen = live.snapshot(Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(15));
        let idle = live.snapshot(Instant::now());
        assert_eq!(
            frozen.decode_ms, idle.decode_ms,
            "frozen at message_end — pure-function idle ticks"
        );
        assert_eq!(frozen.elapsed_ms, idle.elapsed_ms);
        assert!(
            idle.elapsed_ms >= idle.decode_ms,
            "frozen pair stays a consistent TTFT (never negative)"
        );
        // Per-message reset.
        live.on_message_start(Some("assistant"));
        assert_eq!(live.snapshot(Instant::now()).decode_ms, 0);
    }

    /// #50 edge: a message that ends without any delta (aborted before
    /// the first token) keeps `decode_ms` at 0 — there is no decode
    /// window to measure, matching `output_tokens_exact: None`.
    #[test]
    fn live_measure_decode_ms_zero_when_message_ends_before_first_delta() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        std::thread::sleep(std::time::Duration::from_millis(10));
        live.on_message_end(&json!({"role": "assistant", "usage": {"output": 0}}));
        let snapshot = live.snapshot(Instant::now());
        assert_eq!(snapshot.decode_ms, 0);
        assert_eq!(snapshot.output_tokens_exact, None);
        assert!(snapshot.elapsed_ms >= 10);
    }

    /// #45: streaming usage and the message counter join the fingerprint
    /// — running totals can advance without char movement (reasoning
    /// tokens), and a frozen stream must still self-heal a dropped
    /// `message_end` when the NEXT message starts.
    #[test]
    fn fingerprint_moves_with_streaming_usage_and_message_seq() {
        let mut live = LiveMeasure::default();
        live.on_message_start(Some("assistant"));
        live.on_message_end(&json!({"role": "assistant"}));
        let frozen = live.peek_fingerprint(Instant::now());
        // Same measurements, later instant: frozen fingerprint stable.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(live.peek_fingerprint(Instant::now()), frozen);
        // Streaming usage alone moves it.
        live.streaming_output_tokens = Some(4);
        let with_usage = live.peek_fingerprint(Instant::now());
        assert_ne!(with_usage, frozen);
        live.streaming_output_tokens = Some(5);
        assert_ne!(live.peek_fingerprint(Instant::now()), with_usage);
        // Message seq alone moves it (frozen-state message switch).
        live.message_seq += 1;
        assert_ne!(live.peek_fingerprint(Instant::now()), with_usage);
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
        assert!(
            !engine
                .live
                .as_ref()
                .is_some_and(|live| live.ever_measured())
        );
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

    /// V19-07 FR-C R6: command mode (field not selected) never creates a
    /// pending entry; items mode queues the provider-exact total and asks
    /// for the one terminal probe; usage-less messages go to local
    /// settlement; a new session drops the overlay.
    #[test]
    fn message_end_settles_the_live_total_overlay() {
        let mut state = EngineState::default();
        state.arm_live_measure();
        // Command mode: nothing is queued, nothing to probe.
        assert_eq!(
            state.on_message_end(&json!({"role": "assistant", "usage": {"output": 42}})),
            Settlement::None
        );
        assert_eq!(state.live_total_view(), LiveTotalView::default());
        // Items mode with `live-total-tokens` selected.
        state.set_live_total_selected(true);
        state.on_message_start(Some("assistant"));
        assert!(state.live_total_view().current_open);
        assert_eq!(
            state.on_message_end(&json!({"role": "assistant", "usage": {"output": 42}})),
            Settlement::ProviderExact
        );
        let view = state.live_total_view();
        assert_eq!(view.pending_output, 42);
        assert!(view.hidden, "unknown watermark until the probe answers");
        assert!(
            !view.current_open,
            "the finished message is not current any more"
        );
        state.record_terminal_probe(Some(3));
        let view = state.live_total_view();
        assert!(!view.hidden);
        assert_eq!(
            state.live_total.pending()[0].persist_after_assistant,
            Some(4)
        );
        // A failed probe keeps the watermark unknown.
        state.on_message_start(Some("assistant"));
        state.on_message_end(&json!({"role": "assistant", "usage": {"output": 5}}));
        state.record_terminal_probe(None);
        assert!(state.live_total_view().hidden);
        // Usage-less normal message: local settlement pending; the official
        // tokenizer result lands without `~`.
        state.on_message_start(Some("assistant"));
        state.on_message_update(&delta("text_delta", "hello world"));
        assert_eq!(
            state.on_message_end(&json!({"role": "assistant", "usage": {"output": 0}})),
            Settlement::AwaitingLocal
        );
        let awaiting = state.take_awaiting_local().expect("awaiting");
        assert_eq!(awaiting.text, "hello world");
        assert!(awaiting.normal_completion);
        assert!(!state.live_total_view().current_open);
        state.settle_local(crate::live_total::LocalSettlement::Exact(2), None);
        assert_eq!(state.live_total_view().settled_local_exact, 2);
        // Aborted message: the final heuristic is dropped.
        state.on_message_start(Some("assistant"));
        state.on_message_update(&delta("text_delta", "partial"));
        state.on_message_end(&json!({"role": "assistant", "stopReason": "aborted"}));
        let awaiting = state.take_awaiting_local().expect("awaiting");
        assert!(!awaiting.normal_completion);
        state.settle_local(crate::live_total::LocalSettlement::Dropped, None);
        assert_eq!(state.live_total_view().settled_estimated, 0);
        // Non-assistant ends settle nothing.
        assert_eq!(
            state.on_message_end(&json!({"role": "user"})),
            Settlement::None
        );
        // A new session drops the previous session's overlay.
        state.on_session_start(Some("new"));
        assert_eq!(state.live_total_view(), LiveTotalView::default());
    }

    /// R5.2.8 runtime budget: a slow terminal probe (a host answering the
    /// full snapshot) switches the following terminals to the cached
    /// `assistant_messages + pending` lower bound — never the full stats —
    /// and counts the fallbacks; fast probes keep probing.
    #[test]
    fn slow_probes_fall_back_to_the_cached_lower_bound_watermark() {
        let mut state = EngineState::default();
        state.arm_live_measure();
        state.set_live_total_selected(true);
        assert!(state.terminal_probe_wanted(), "no samples yet: probe");
        for _ in 0..8 {
            state.note_terminal_probe_duration(500);
        }
        assert!(
            state.terminal_probe_wanted(),
            "sub-microsecond probes stay on the probe path"
        );
        // Baseline snapshot at 3 assistant messages.
        let token = state
            .session_stats
            .begin_request(crate::session_stats::StatsTrigger::TurnEnd);
        let mut stats = crate::session_stats::SessionStats {
            session_id: "s1".into(),
            session_file: None,
            started_at: "2026-10-27T10:00:00Z".into(),
            entry_count: 3,
            user_messages: 1,
            assistant_messages: 3,
            tokens: Default::default(),
            cost: 0.0,
        };
        assert!(state.session_stats.apply(token, stats.clone()).is_some());
        for _ in 0..8 {
            state.note_terminal_probe_duration(PROBE_SLOW_NS + 1);
        }
        assert!(
            !state.terminal_probe_wanted(),
            "p95 above the budget: lower bound"
        );
        state.on_message_start(Some("assistant"));
        state.on_message_end(&json!({"role": "assistant", "usage": {"output": 10}}));
        state.record_lower_bound_watermark();
        assert_eq!(state.probe_fallbacks(), 1);
        // Watermark = cached assistant_messages + pending (the snapshot
        // that counts this message).
        assert_eq!(
            state.live_total.pending()[0].persist_after_assistant,
            Some(4)
        );
        assert!(!state.live_total_view().hidden);
        // A second pending entry moves the bound up by one.
        stats.assistant_messages = 3;
        state.on_message_start(Some("assistant"));
        state.on_message_end(&json!({"role": "assistant", "usage": {"output": 11}}));
        state.record_lower_bound_watermark();
        assert_eq!(
            state.live_total.pending()[1].persist_after_assistant,
            Some(5)
        );
        assert_eq!(state.probe_fallbacks(), 2);
    }

    /// A stream that starts without `message_start` opens the message
    /// generation on its first delta (mid-stream enable / dropped start).
    #[test]
    fn first_delta_opens_the_message_generation_without_a_start() {
        let mut state = EngineState::default();
        state.arm_live_measure();
        state.on_message_update(&delta("text_delta", "x"));
        assert!(state.live_total_view().current_open);
        assert_eq!(state.live_total.message_generation(), 1);
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
        rpi_test_env::set_var("RPI_CODING_AGENT_SESSION_DIR", &session_dir);
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
        rpi_test_env::remove_var("RPI_CODING_AGENT_SESSION_DIR");
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
        rpi_test_env::set_var("RPI_CODING_AGENT_SESSION_DIR", &session_dir);
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
        rpi_test_env::remove_var("RPI_CODING_AGENT_SESSION_DIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `record_live_values` / `live_held_values`: every live row keeps the
    /// last value it actually measured, so the gap between two assistant
    /// messages (measurement reset, no delta yet) and the rate window do not
    /// flip the footer to the dash placeholders. `streaming` is not held.
    #[test]
    fn items_live_rows_keep_the_last_measured_value() {
        use crate::fields::{FieldData, LiveView};
        use crate::tokens::{TokenBreakdown, TokenSource};

        let frame = |live: LiveView| FieldData {
            live: Some(live),
            ..FieldData::default()
        };
        let mut state = EngineState::default();
        state.set_live_measure(true);

        // First message settles with exact numbers.
        state.record_live_values(&frame(LiveView {
            streaming: false,
            output_tokens: None,
            output_tokens_exact: Some(500),
            decode_ms: 4_000,
            tokens: Some(TokenBreakdown::from_tiers(
                480,
                0,
                0,
                TokenSource::Estimated,
            )),
            rate_source_switched: false,
            policy: LiveCalculation::Auto,
            settled: true,
        }));
        let held = state.live_held_values();
        let held_output = held
            .iter()
            .find(|(id, _)| *id == FieldId::LiveOutputTokens)
            .map(|(_, value)| value.text.clone());
        assert_eq!(held_output.as_deref(), Some("500"));
        assert!(
            held.iter().all(|(id, _)| *id != FieldId::Streaming),
            "`streaming` is never held"
        );

        // Next `message_start`, before its first delta: the measurement is
        // empty, but the frame still renders the held values.
        let gap = frame(LiveView {
            streaming: true,
            ..LiveView::default()
        });
        let mut gap_with_held = gap.clone();
        gap_with_held.live_held = state.live_held_values();
        assert!(
            crate::fields::value(FieldId::LiveOutputTokens, &gap_with_held)
                .is_some_and(|value| value.text == "500"),
            "the gap keeps the previous total"
        );
        assert!(
            crate::fields::value(FieldId::LiveOutputRate, &gap_with_held)
                .is_some_and(|value| value.text.contains("tok/s")),
            "the gap keeps the previous rate"
        );

        // The new message's first delta refreshes the total (the recorded
        // value moves); the not-yet-stable rate keeps its previous text.
        state.record_live_values(&frame(LiveView {
            streaming: true,
            output_tokens: Some(7),
            output_tokens_exact: None,
            decode_ms: 300,
            tokens: Some(TokenBreakdown::from_tiers(7, 0, 0, TokenSource::Estimated)),
            rate_source_switched: false,
            policy: LiveCalculation::Auto,
            settled: false,
        }));
        let refreshed = state.live_held_values();
        assert!(
            refreshed
                .iter()
                .any(|(id, value)| *id == FieldId::LiveOutputTokens && value.text == "7"),
            "fresh data replaces the held total: {refreshed:?}"
        );

        // Disabling the measurement drops the cache; a re-enable starts from
        // the dash placeholders.
        state.set_live_measure(false);
        assert!(state.live_held_values().is_empty());
        state.set_live_measure(true);
        let mut after = frame(LiveView::default());
        after.live_held = state.live_held_values();
        assert!(
            crate::fields::value(FieldId::LiveOutputTokens, &after).is_some_and(|v| v.text == "-")
        );

        // A new session drops the held rows as well.
        state.record_live_values(&frame(LiveView {
            streaming: false,
            output_tokens: None,
            output_tokens_exact: Some(9),
            decode_ms: 1_000,
            tokens: Some(TokenBreakdown::from_tiers(9, 0, 0, TokenSource::Estimated)),
            rate_source_switched: false,
            policy: LiveCalculation::Auto,
            settled: true,
        }));
        assert!(!state.live_held_values().is_empty());
        state.on_session_start(Some("new"));
        assert!(state.live_held_values().is_empty());
    }

    /// rpi#78 B: the rate's numerator source is tracked per streaming frame; a
    /// switch is reported once, and both a new message and a settled frame
    /// clear the tracking (the settled frame is authoritative).
    #[test]
    fn rate_source_switch_is_detected_once_per_message() {
        use crate::fields::{FieldData, LiveView};
        use crate::tokens::TokenSource;

        let mut state = EngineState::default();
        state.set_live_measure(true);
        let _ = FieldData::default();
        // No live measurement armed → no switch is ever reported.
        assert!(!state.note_rate_source(true, Some(TokenSource::Estimated)));

        state.live = Some(LiveMeasure::default());
        assert!(
            !state.note_rate_source(true, Some(TokenSource::Estimated)),
            "the first frame has no previous source"
        );
        assert!(
            !state.note_rate_source(true, Some(TokenSource::Estimated)),
            "the same source is not a switch"
        );
        assert!(
            state.note_rate_source(true, Some(TokenSource::ExactProvider)),
            "estimate → provider is a switch"
        );
        assert!(
            !state.note_rate_source(false, Some(TokenSource::ExactProvider)),
            "a settled frame never counts as a switch"
        );
        assert!(
            !state.note_rate_source(true, Some(TokenSource::Estimated)),
            "the settle cleared the tracking, so the next message starts fresh"
        );
        // `begin_stream` (message_start) also clears it.
        if let Some(live) = state.live.as_mut() {
            let _ = LiveView::default();
            live.begin_stream();
        }
        assert!(!state.note_rate_source(true, Some(TokenSource::ExactProvider)));
    }

    /// A settled frame replaces the hold cache: a row the finished message
    /// could not produce must not keep an older message's number into the
    /// next gap.
    #[test]
    fn settled_frame_resets_the_hold_cache() {
        use crate::fields::{FieldData, LiveView};
        use crate::tokens::{TokenBreakdown, TokenSource};

        let frame = |live: LiveView| FieldData {
            live: Some(live),
            ..FieldData::default()
        };
        let mut state = EngineState::default();
        state.set_live_measure(true);

        // A long message settles with a rate.
        state.record_live_values(&frame(LiveView {
            streaming: false,
            output_tokens: None,
            output_tokens_exact: Some(500),
            decode_ms: 4_000,
            tokens: Some(TokenBreakdown::from_tiers(
                500,
                0,
                0,
                TokenSource::Estimated,
            )),
            rate_source_switched: false,
            policy: LiveCalculation::Auto,
            settled: true,
        }));
        assert!(
            state
                .live_held_values()
                .iter()
                .any(|(id, _)| *id == FieldId::LiveOutputRate)
        );

        // A short message settles without a usable rate window: the previous
        // rate must not survive the settle.
        state.record_live_values(&frame(LiveView {
            streaming: false,
            output_tokens: None,
            output_tokens_exact: Some(1),
            decode_ms: 0,
            tokens: Some(TokenBreakdown::from_tiers(1, 0, 0, TokenSource::Estimated)),
            rate_source_switched: false,
            policy: LiveCalculation::Auto,
            settled: true,
        }));
        let held = state.live_held_values();
        assert!(
            held.iter()
                .any(|(id, value)| *id == FieldId::LiveOutputTokens && value.text == "1"),
            "the settled total is recorded: {held:?}"
        );
        assert!(
            held.iter().all(|(id, _)| *id != FieldId::LiveOutputRate),
            "the previous rate is not held across the settle: {held:?}"
        );
    }
}
