//! The refresh loop's streaming counter for items mode (V19-07 FR-B/FR-G).
//!
//! Owned by the refresh loop, never by the dispatch thread: the dispatch
//! path only appends deltas to the live measurement; the live tick feeds the
//! bytes that arrived since the previous tick ([`LiveMeasure::unprocessed_text`])
//! into the exact incremental counter ([`IncrementalCounter`]) and the cheap
//! heuristic buckets, so the items path never deep-copies the message per
//! refresh (command mode still materializes `rpi.live_output` in full).
//!
//! Two policies share the buckets; `auto` additionally runs the exact
//! counter when the model family has a bundled tokenizer and the frame needs
//! a local number (`TokenRequirements`). A policy switch mid-stream discards
//! the old policy's counter state and rebuilds the current message's counter
//! ONCE from a shared full-text snapshot (`CountReset`), keeping the
//! generations and the pending/settled overlay untouched (R5.1.4).

use std::sync::Arc;

use crate::config::LiveCalculation;
use crate::fields::{FieldId, LiveView};
use crate::incremental::{CharBuckets, CountFallback, IncrementalCounter, TIERS};
use crate::items::ItemsConfig;
use crate::state::LiveStatus;
use crate::tokens::{self, TokenBreakdown, TokenSource, Tokenizer};

/// Which local numbers a frame needs (R5.3.1 / 02 §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenRequirements {
    /// A local total is needed (`live-output-tokens`, `live-output-rate`,
    /// `live-total-tokens`): a provider running total may satisfy it without
    /// any local BPE when the tier split is not needed.
    pub needs_total: bool,
    /// A tier row is selected (`live-text/thinking/toolcall-tokens`): the
    /// local counter must run even when the provider reports a total.
    pub needs_tiers: bool,
}

impl TokenRequirements {
    pub fn for_items(items: &ItemsConfig) -> Self {
        let has = |id: FieldId| items.has_field(id);
        Self {
            needs_total: has(FieldId::LiveOutputTokens)
                || has(FieldId::LiveOutputRate)
                || has(FieldId::LiveTotalTokens),
            needs_tiers: has(FieldId::LiveTextTokens)
                || has(FieldId::LiveThinkingTokens)
                || has(FieldId::LiveToolcallTokens),
        }
    }
}

/// Everything one streaming frame needs from the engine, read under the
/// engine lock in one pass (no full text: that is copied only through the
/// `full_text` closure when the ticker asks for a rebuild).
pub(crate) struct LiveFrameInput<'a> {
    pub status: &'a LiveStatus,
    /// The bytes that arrived since the last tick — computed by the caller
    /// from the base [`LiveTicker::processed_for`] returns for this frame's
    /// message generation (a new message starts from zero).
    pub deltas: [String; TIERS],
    pub session_generation: u64,
    pub message_generation: u64,
    pub policy: LiveCalculation,
    pub requirements: TokenRequirements,
    /// Lazy family-tokenizer loader: called ONLY when the frame needs a
    /// local exact count (`auto` without a sufficient provider total), so
    /// `estimate` and provider-satisfied `auto` frames never load or run
    /// the tokenizer (R5.3.2).
    pub tokenizer: &'a dyn Fn() -> Option<Arc<Tokenizer>>,
    pub settled: Option<TokenBreakdown>,
}

/// Counters exposed for diagnostics/benchmarks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickerStats {
    /// Policy switches that rebuilt the current message's counter.
    pub count_resets: u64,
    /// Messages that left the incremental path (`CountFallback`).
    pub count_fallbacks: u64,
    /// `finalize()` results that disagreed with the official full pass.
    pub finalize_mismatches: u64,
    /// `finalize()` results that were compared with the official pass.
    pub finalize_comparisons: u64,
}

#[derive(Debug, Default)]
struct Buckets {
    han: [u64; TIERS],
    other: [u64; TIERS],
}

impl Buckets {
    fn push(&mut self, tier: usize, text: &str) {
        let mut bucket = CharBuckets::default();
        bucket.push_str(text);
        self.han[tier] += bucket.han;
        self.other[tier] += bucket.other;
    }

    fn breakdown(&self) -> TokenBreakdown {
        TokenBreakdown::from_tiers(
            tokens::heuristic_from_buckets(self.han[0], self.other[0]),
            tokens::heuristic_from_buckets(self.han[1], self.other[1]),
            tokens::heuristic_from_buckets(self.han[2], self.other[2]),
            TokenSource::Estimated,
        )
    }
}

/// The loop-owned per-message state.
#[derive(Default)]
pub struct LiveTicker {
    session_generation: u64,
    message_generation: u64,
    policy: Option<LiveCalculation>,
    /// Byte counts of each tier buffer already fed for the CURRENT message.
    processed: [usize; TIERS],
    /// Whether any delta of the current message was fed (a policy change
    /// before the first delta is not a mid-stream switch).
    fed: bool,
    buckets: Option<Buckets>,
    exact: Option<ExactState>,
    pub stats: TickerStats,
}

struct ExactState {
    tokenizer: Arc<Tokenizer>,
    counter: IncrementalCounter,
    /// The CountFallback of this message was already counted.
    reported_fallback: bool,
}

impl std::fmt::Debug for LiveTicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveTicker")
            .field("session_generation", &self.session_generation)
            .field("message_generation", &self.message_generation)
            .field("policy", &self.policy)
            .field("processed", &self.processed)
            .field("fed", &self.fed)
            .field("exact", &self.exact.as_ref().map(|_| "exact"))
            .field("stats", &self.stats)
            .finish()
    }
}

impl LiveTicker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the exact counter only (the heuristic buckets stay valid — they
    /// are policy independent).
    fn drop_exact(&mut self) {
        self.exact = None;
    }

    /// The byte base for [`LiveMeasure::unprocessed_text`] when the frame
    /// describes `(session_generation, message_generation)`: the bytes
    /// already fed when it is the message of the session this ticker
    /// counted, zero otherwise (the deltas of a new message — or of the first
    /// message after `/new`, whose message counter restarts — start from
    /// their first byte).
    pub fn processed_for(
        &self,
        session_generation: u64,
        message_generation: u64,
    ) -> [usize; TIERS] {
        if session_generation == self.session_generation
            && message_generation == self.message_generation
        {
            self.processed
        } else {
            [0; TIERS]
        }
    }

    /// The message generation this ticker's counters belong to.
    pub fn message_generation(&self) -> u64 {
        self.message_generation
    }

    /// Build the frame's live view for the CURRENT message. `full_text` is a
    /// closure so the caller copies the message only when this function asks
    /// for it (a policy switch, a tokenizer that just became needed, or the
    /// full-recount fallback).
    pub(crate) fn live_view(
        &mut self,
        input: LiveFrameInput<'_>,
        mut full_text: impl FnMut() -> [String; 3],
    ) -> LiveView {
        let LiveFrameInput {
            status,
            deltas,
            session_generation,
            message_generation,
            policy,
            requirements,
            tokenizer,
            settled,
        } = input;
        // New session: every per-message state restarts (and the policy
        // baseline with it, so the first frame is never a CountReset).
        if session_generation != self.session_generation {
            self.session_generation = session_generation;
            self.message_generation = 0;
            self.processed = [0; TIERS];
            self.fed = false;
            self.buckets = None;
            self.exact = None;
            self.policy = None;
        }
        // A finished message keeps its settled view: the engine's
        // authoritative settlement, else the last provisional of the message
        // this ticker counted; no deltas arrive for it.
        if !status.streaming {
            let tokens = settled.or_else(|| {
                (self.message_generation == message_generation)
                    .then(|| self.last_breakdown(requirements))
                    .flatten()
            });
            return LiveView {
                streaming: false,
                output_tokens: status.output_tokens_exact.or(status.output_tokens),
                output_tokens_exact: status.output_tokens_exact,
                decode_ms: status.decode_ms,
                tokens,
                rate_source_switched: false,
                policy,
                settled: true,
            };
        }
        // New message (generation moved on): the per-message counters
        // restart; the policy baseline stays (a change is handled below).
        if message_generation != self.message_generation {
            self.message_generation = message_generation;
            self.processed = [0; TIERS];
            self.fed = false;
            self.buckets = None;
            self.exact = None;
        }
        let local_needed = requirements.needs_total || requirements.needs_tiers;
        let provider_streaming = status.output_tokens.or(status.output_tokens_exact);
        let provider_suffices = policy == LiveCalculation::Auto
            && !requirements.needs_tiers
            && provider_streaming.is_some_and(|total| total > 0);
        // Policy switch mid-stream: discard the old policy's counter state
        // and rebuild ONCE from the shared full text (CountReset). A change
        // noticed before the first delta of a message has nothing to rebuild.
        let switched = self.policy.is_some_and(|previous| previous != policy);
        if switched && self.fed {
            if let Some(previous) = self.policy {
                match (previous, policy) {
                    // auto → estimate: no BPE ever again this message; the
                    // Han/other buckets are rebuilt from the full text.
                    (LiveCalculation::Auto, LiveCalculation::Estimate) => {
                        self.drop_exact();
                        self.buckets = None;
                        let text = full_text();
                        let mut buckets = Buckets::default();
                        for (tier, text) in text.iter().enumerate() {
                            buckets.push(tier, text);
                        }
                        self.buckets = Some(buckets);
                    }
                    // estimate → auto: adopt the provider's streaming exact
                    // total directly when there is one and no tier is needed;
                    // otherwise rebuild the local counter from the full text
                    // (one pass, one BPE run over the message so far).
                    (LiveCalculation::Estimate, LiveCalculation::Auto) if !provider_suffices => {
                        let text = full_text();
                        self.rebuild_exact(&text, tokenizer());
                    }
                    _ => {}
                }
            }
            self.stats.count_resets += 1;
            tracing::info!(
                previous = ?self.policy,
                policy = policy.as_str(),
                "rpi-statusline: CountReset (calculation policy switched mid-stream)"
            );
        }
        self.policy = Some(policy);
        // Feed the new bytes to the policy-independent buckets.
        if local_needed {
            let buckets = self.buckets.get_or_insert_with(Buckets::default);
            for (tier, delta) in deltas.iter().enumerate() {
                buckets.push(tier, delta);
            }
        }
        // The exact counter runs only under `auto`, only when a local number
        // is needed and the provider total does not suffice — the tokenizer
        // is loaded exactly then (and never for `estimate`). A counter that
        // is no longer needed (the provider started reporting mid-message,
        // or the tier rows were deselected) is DROPPED rather than starved:
        // a starved counter would disagree with the official pass at
        // message_end; without a counter no comparison is owed and the
        // official pass settles the message if the provider reports nothing.
        let need_exact = policy == LiveCalculation::Auto && local_needed && !provider_suffices;
        if !need_exact {
            self.drop_exact();
        } else if self.exact.is_none() {
            if let Some(loaded) = tokenizer() {
                // First local number of this message, or the family tokenizer
                // just became needed again: one rebuild pass over the message
                // so far (the deltas of this tick are already in it).
                let text = full_text();
                self.rebuild_exact(&text, Some(loaded));
                self.processed = [text[0].len(), text[1].len(), text[2].len()];
            }
        } else if let Some(exact) = self.exact.as_mut() {
            for (tier, delta) in deltas.iter().enumerate() {
                exact.counter.push(tier, delta);
                // The base advances with every byte fed to the counter: the
                // next tick's deltas start where this one ended (a stale base
                // re-fed everything since the rebuild on every tick — V19-07
                // review round 3).
                self.processed[tier] += delta.len();
            }
            if exact.counter.fallback().is_some() && !exact.reported_fallback {
                exact.reported_fallback = true;
                self.stats.count_fallbacks += 1;
            }
        }
        if self.exact.is_none() {
            for (index, delta) in deltas.iter().enumerate() {
                self.processed[index] += delta.len();
            }
        }
        for delta in deltas.iter() {
            if !delta.is_empty() {
                self.fed = true;
            }
        }
        let local = self.local_breakdown(policy, requirements, provider_suffices, &mut full_text);
        LiveView {
            streaming: true,
            output_tokens: status.output_tokens,
            output_tokens_exact: status.output_tokens_exact,
            decode_ms: status.decode_ms,
            tokens: Some(local),
            rate_source_switched: false,
            policy,
            settled: false,
        }
    }

    /// The settled view of the message that just ended: the last provisional
    /// of the message this ticker counted (exact counter, else buckets).
    fn last_breakdown(&mut self, requirements: TokenRequirements) -> Option<TokenBreakdown> {
        if !requirements.needs_total && !requirements.needs_tiers {
            return None;
        }
        if let Some(exact) = self.exact.as_mut() {
            return exact.counter.provisional();
        }
        self.buckets
            .as_ref()
            .map(Buckets::breakdown)
            .or(Some(TokenBreakdown::default()))
    }

    /// Rebuild the exact counter from the full text of the current message.
    fn rebuild_exact(&mut self, text: &[String; 3], tokenizer: Option<Arc<Tokenizer>>) {
        self.exact = None;
        let Some(tokenizer) = tokenizer else {
            return;
        };
        let mut counter = IncrementalCounter::new(tokenizer.clone());
        for (tier, text) in text.iter().enumerate() {
            counter.push(tier, text);
        }
        self.exact = Some(ExactState {
            tokenizer,
            counter,
            reported_fallback: false,
        });
    }

    /// The local breakdown for a streaming frame under the policy.
    fn local_breakdown(
        &mut self,
        policy: LiveCalculation,
        requirements: TokenRequirements,
        provider_suffices: bool,
        full_text: &mut impl FnMut() -> [String; 3],
    ) -> TokenBreakdown {
        let local_needed = requirements.needs_total || requirements.needs_tiers;
        if !local_needed {
            return TokenBreakdown::default();
        }
        if policy == LiveCalculation::Estimate {
            return self
                .buckets
                .as_ref()
                .map(Buckets::breakdown)
                .unwrap_or_default();
        }
        match self.exact.as_mut() {
            Some(exact) => match exact.counter.provisional() {
                Some(provisional) => provisional,
                // CountFallback: full recount at refresh cadence (the
                // message left the incremental path), never a wrong number.
                None => {
                    let text = full_text();
                    exact.tokenizer.count_tiers(&text[0], &text[1], &text[2])
                }
            },
            // No local exact counter: the provider total suffices (an empty
            // local split; `with_provider_total` places the total), or no
            // bundled tokenizer exists for the model (the heuristic).
            None if provider_suffices => TokenBreakdown::default(),
            None => self
                .buckets
                .as_ref()
                .map(Buckets::breakdown)
                .unwrap_or_default(),
        }
    }

    /// Finalize the exact counter of `generation`'s message and return its
    /// per-tier total, or `None` when no exact counter ran for it. `official`
    /// is the official full pass for the comparison (counted once per
    /// comparison); a counter of another generation is never compared.
    pub fn finalize_exact(&mut self, official: Option<u64>, generation: u64) -> Option<u64> {
        if generation != self.message_generation {
            return None;
        }
        let exact = self.exact.as_mut()?;
        match exact.counter.finalize() {
            Some(finalized) => {
                if let Some(official) = official {
                    self.stats.finalize_comparisons += 1;
                    if official != finalized.total {
                        self.stats.finalize_mismatches += 1;
                        tracing::warn!(
                            incremental = finalized.total,
                            official,
                            "rpi-statusline: incremental count disagreed with the official \
                             tokenizer; the official value is used and the incremental path is \
                             disabled for this message"
                        );
                    }
                }
                Some(finalized.total)
            }
            None => {
                if !exact.reported_fallback {
                    exact.reported_fallback = true;
                    self.stats.count_fallbacks += 1;
                }
                None
            }
        }
    }

    /// Whether an exact counter ran for `generation`'s message (the
    /// official comparison of a provider-exact message only runs then).
    pub fn has_exact_for(&self, generation: u64) -> bool {
        generation == self.message_generation && self.exact.is_some()
    }

    pub fn fallback(&self) -> Option<CountFallback> {
        self.exact.as_ref()?.counter.fallback()
    }

    pub fn stats(&self) -> TickerStats {
        self.stats
    }

    /// Bytes of each tier buffer already consumed for the current message.
    pub fn processed(&self) -> [usize; TIERS] {
        self.processed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::ALL_FIELDS;
    use crate::items::parse_items;
    use crate::tokens::tokenizer_for_model;
    use serde_json::{Value, json};

    fn requirements(items: Value) -> TokenRequirements {
        let config = parse_items(items.as_object().expect("object"));
        TokenRequirements::for_items(&config)
    }

    /// Counting-benchmark leg through the ticker protocol (the way the
    /// items live tick feeds the counter): one provisional per tick, BPE piece
    /// evaluations counted by the tokenizer. `--ignored --release --nocapture`.
    #[test]
    #[ignore = "counting benchmark leg; run explicitly with --ignored --nocapture"]
    fn v19_counting_bench_ticker_protocol_leg() {
        let text: String = "The assistant explains: 这是一个较长的回答 with **Markdown**, `code`, \
\"json\": {\"a\": [1, 2, 3]}\n\n- list item 1\n- item 2\n"
            .repeat(60);
        for model in ["glm-5.3", "kimi-k3.1-pro", "deepseek-chat"] {
            let tokenizer = tokenizer_for_model(Some(model)).expect("bundled family");
            let loader = move || tokenizer_for_model(Some(model));
            let config = parse_items(
                json!({"type": "items", "items": ["live-output-tokens"]})
                    .as_object()
                    .expect("object"),
            );
            let req = TokenRequirements::for_items(&config);
            let mut ticker = LiveTicker::new();
            let mut fed = String::new();
            let mut rest = text.as_str();
            tokens::reset_bpe_calls();
            let started = std::time::Instant::now();
            let mut ticks = 0u64;
            while !rest.is_empty() {
                let mut end = rest.len().min(24);
                while !rest.is_char_boundary(end) {
                    end += 1;
                }
                for _ in 0..4 {
                    fed.push_str(&rest[..end.min(rest.len())]);
                    let base = ticker.processed_for(1, 1);
                    let deltas = [fed[base[0]..].to_owned(), String::new(), String::new()];
                    let status = LiveStatus {
                        streaming: true,
                        ..LiveStatus::default()
                    };
                    let full = [fed.clone(), String::new(), String::new()];
                    let _ = ticker.live_view(
                        LiveFrameInput {
                            status: &status,
                            deltas,
                            session_generation: 1,
                            message_generation: 1,
                            policy: LiveCalculation::Auto,
                            requirements: req,
                            tokenizer: &loader,
                            settled: None,
                        },
                        || full.clone(),
                    );
                    ticks += 1;
                    rest = &rest[end.min(rest.len())..];
                    if rest.is_empty() {
                        break;
                    }
                    end = rest.len().min(24);
                    while !rest.is_char_boundary(end) {
                        end += 1;
                    }
                }
            }
            let elapsed = started.elapsed();
            let official = tokenizer.count(&text);
            let finalized = ticker.finalize_exact(Some(official), 1);
            let calls = tokens::bpe_calls();
            println!(
                "{model:<16} ticker protocol: {ticks} ticks, BPE piece evals {calls}, CPU {elapsed:?}, finalize {finalized:?} vs official {official}, mismatches {}",
                ticker.stats().finalize_mismatches
            );
            assert_eq!(finalized, Some(official));
            assert_eq!(ticker.stats().finalize_mismatches, 0);
        }
    }

    #[test]
    fn token_requirements_follow_the_field_selection() {
        let defaults = requirements(json!({"type": "items"}));
        assert!(defaults.needs_total);
        assert!(!defaults.needs_tiers);
        let tiers = requirements(json!({
            "type": "items",
            "items": ["live-text-tokens", "live-thinking-tokens"]
        }));
        assert!(tiers.needs_tiers);
        assert!(!tiers.needs_total);
        let streaming_only = requirements(json!({"type": "items", "items": ["streaming"]}));
        assert!(!streaming_only.needs_total && !streaming_only.needs_tiers);
        let rate = requirements(json!({"type": "items", "items": ["live-output-rate"]}));
        assert!(rate.needs_total);
        for id in ["live-output-tokens", "live-total-tokens"] {
            assert!(
                requirements(json!({"type": "items", "items": [id]})).needs_total,
                "{id}"
            );
        }
        for id in [
            "live-text-tokens",
            "live-thinking-tokens",
            "live-toolcall-tokens",
        ] {
            assert!(
                requirements(json!({"type": "items", "items": [id]})).needs_tiers,
                "{id}"
            );
        }
        // Every live field is classified.
        for field in ALL_FIELDS.into_iter().filter(|field| field.is_live()) {
            let value = requirements(json!({"type": "items", "items": [field.id()]}));
            assert!(
                value.needs_total || value.needs_tiers || field == FieldId::Streaming,
                "{}",
                field.id()
            );
        }
    }

    fn frame<'a>(
        status: &'a LiveStatus,
        deltas: [String; TIERS],
        session_generation: u64,
        message_generation: u64,
        policy: LiveCalculation,
        requirements: TokenRequirements,
    ) -> LiveFrameInput<'a> {
        LiveFrameInput {
            status,
            deltas,
            session_generation,
            message_generation,
            policy,
            requirements,
            tokenizer: &|| None,
            settled: None,
        }
    }

    /// Buckets are fed from the deltas alone: each tier's `ceil` is taken
    /// once at display, never per delta.
    #[test]
    fn heuristic_buckets_feed_from_deltas_only() {
        let mut ticker = LiveTicker::new();
        let status = LiveStatus {
            streaming: true,
            ..LiveStatus::default()
        };
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let view = ticker.live_view(
            frame(
                &status,
                ["你好".into(), "".into(), "".into()],
                1,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(view.tokens.expect("tokens").total, 2);
        let view = ticker.live_view(
            frame(
                &status,
                [" hello".into(), "".into(), "".into()],
                1,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        // ceil((2/1.2) + (6/3.5)) = ceil(3.37) = 4, not 2 + 2.
        assert_eq!(view.tokens.expect("tokens").total, 4);
        assert_eq!(ticker.stats().count_resets, 0);
    }

    /// V19-07 review P1-1: the delta base is per message generation. The
    /// caller computes the deltas of a NEW message from zero
    /// ([`LiveTicker::processed_for`]), so its first frame counts the whole
    /// new text and the second message's estimate equals its full-text
    /// heuristic — never the previous message's offsets applied to it.
    #[test]
    fn delta_base_follows_the_message_generation() {
        let mut ticker = LiveTicker::new();
        let status = LiveStatus {
            streaming: true,
            ..LiveStatus::default()
        };
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let first = "hello world, hello"; // 18 chars → ceil(18/3.5) = 6
        let second = "a much longer second message with 你好 in it"; // 44 chars: 2 Han + 42 other
        let expected_second = tokens::heuristic_tokens(second);
        // Message 1: two ticks.
        ticker.live_view(
            frame(
                &status,
                [first[..9].into(), "".into(), "".into()],
                1,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(ticker.processed_for(1, 1), [9, 0, 0]);
        let view = ticker.live_view(
            frame(
                &status,
                [first[9..].into(), "".into(), "".into()],
                1,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(
            view.tokens.expect("tokens").total,
            tokens::heuristic_tokens(first)
        );
        // Message 2 starts: the base for its generation is zero, so the
        // caller hands the whole new text so far.
        assert_eq!(
            ticker.processed_for(1, 2),
            [0, 0, 0],
            "a new message starts from byte 0"
        );
        ticker.live_view(
            frame(
                &status,
                [second[..20].into(), "".into(), "".into()],
                1,
                2,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(ticker.processed_for(1, 2), [20, 0, 0]);
        let view = ticker.live_view(
            frame(
                &status,
                [second[20..].into(), "".into(), "".into()],
                1,
                2,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(
            view.tokens.expect("tokens").total,
            expected_second,
            "the second message's estimate is its own full-text heuristic"
        );
        // The wrong base (message 1's offsets) would have lost the head of
        // the message.
        assert_ne!(
            tokens::heuristic_tokens(&second[18..]),
            expected_second,
            "the fixture distinguishes the two bases"
        );
        assert_eq!(ticker.message_generation(), 2);
    }

    /// `estimate` frames and provider-satisfied `auto` frames never ask for
    /// the tokenizer (R5.3.2 / FR-B R7): the loader closure counts its calls.
    #[test]
    fn tokenizer_loads_only_when_a_local_exact_count_is_needed() {
        let status = LiveStatus {
            streaming: true,
            output_tokens: Some(500),
            ..LiveStatus::default()
        };
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let loads = std::sync::atomic::AtomicU32::new(0);
        let loader = || {
            loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokenizer_for_model(Some("glm-5.3"))
        };
        let mut ticker = LiveTicker::new();
        for policy in [LiveCalculation::Estimate, LiveCalculation::Auto] {
            let view = ticker.live_view(
                LiveFrameInput {
                    status: &status,
                    deltas: ["hello".into(), "".into(), "".into()],
                    session_generation: 1,
                    message_generation: 1,
                    policy,
                    requirements: req,
                    tokenizer: &loader,
                    settled: None,
                },
                || [String::new(), String::new(), String::new()],
            );
            // The local split is the heuristic under estimate (ignored by
            // `reported_output` while streaming) and empty under a
            // provider-satisfied auto frame; the provider total is placed
            // either way without any tokenizer load.
            match policy {
                LiveCalculation::Estimate => {
                    assert_eq!(view.tokens.expect("tokens").source, TokenSource::Estimated);
                    assert_eq!(view.reported_output().expect("output").total, 2);
                }
                LiveCalculation::Auto => {
                    assert_eq!(view.tokens.expect("tokens").total, 0);
                    assert_eq!(view.reported_output().expect("output").total, 500);
                }
            }
        }
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "estimate never loads; a provider-satisfied auto frame does not either"
        );
        // Without a provider total, auto loads exactly once and keeps it.
        let status = LiveStatus {
            streaming: true,
            output_tokens: None,
            ..LiveStatus::default()
        };
        let _ = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: ["hello world".into(), "".into(), "".into()],
                session_generation: 1,
                message_generation: 2,
                policy: LiveCalculation::Auto,
                requirements: req,
                tokenizer: &loader,
                settled: None,
            },
            || [String::new(), String::new(), String::new()],
        );
        let _ = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: ["!".into(), "".into(), "".into()],
                session_generation: 1,
                message_generation: 2,
                policy: LiveCalculation::Auto,
                requirements: req,
                tokenizer: &loader,
                settled: None,
            },
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(ticker.stats().finalize_mismatches, 0);
    }

    /// V19-07 review round 2: the delta base is keyed by the SESSION
    /// generation too — `/new` restarts the message counter, so the first
    /// message of the new session would otherwise reuse the previous
    /// session's offsets for the same message number.
    #[test]
    fn delta_base_follows_the_session_generation() {
        let mut ticker = LiveTicker::new();
        let status = LiveStatus {
            streaming: true,
            ..LiveStatus::default()
        };
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let previous = "the only message of the previous session";
        ticker.live_view(
            frame(
                &status,
                [previous.into(), "".into(), "".into()],
                7,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(ticker.processed_for(7, 1), [previous.len(), 0, 0]);
        // The new session's first message is generation (8, 1): base zero.
        assert_eq!(ticker.processed_for(8, 1), [0, 0, 0]);
        let fresh = "brand new session, first message, much longer";
        let view = ticker.live_view(
            frame(
                &status,
                [fresh.into(), "".into(), "".into()],
                8,
                1,
                LiveCalculation::Estimate,
                req,
            ),
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(
            view.tokens.expect("tokens").total,
            tokens::heuristic_tokens(fresh)
        );
        assert_eq!(ticker.processed_for(8, 1), [fresh.len(), 0, 0]);
        assert_eq!(
            ticker.processed_for(7, 1),
            [0, 0, 0],
            "the old session is gone"
        );
    }

    /// V19-07 review round 3 (P1): with the exact counter present, three
    /// ticks under the caller's base/deltas protocol of `items_live_view`
    /// (each tick feeds `text[base..]`) — every frame equals the official
    /// count of the text so far, the base tracks the text length, and no
    /// finalize disagreement is reported. The counter used to leave the
    /// base at the rebuild length, re-feeding everything since then on every
    /// tick (frames 4 → 16 → 49 for an official 15).
    #[test]
    fn three_ticks_with_the_exact_counter_feed_each_byte_once() {
        let tokenizer = tokenizer_for_model(Some("glm-5.3")).expect("glm");
        let loader = || Some(Arc::clone(&tokenizer));
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let mut ticker = LiveTicker::new();
        let mut text = String::new();
        for piece in [
            "The quick brown",
            " fox jumps over",
            " the lazy dog. 你好世界",
        ] {
            text.push_str(piece);
            let base = ticker.processed_for(1, 1);
            let deltas = [text[base[0]..].to_owned(), String::new(), String::new()];
            let status = LiveStatus {
                streaming: true,
                ..LiveStatus::default()
            };
            let full = [text.clone(), String::new(), String::new()];
            let view = ticker.live_view(
                LiveFrameInput {
                    status: &status,
                    deltas,
                    session_generation: 1,
                    message_generation: 1,
                    policy: LiveCalculation::Auto,
                    requirements: req,
                    tokenizer: &loader,
                    settled: None,
                },
                || full.clone(),
            );
            let official = tokenizer.count(&text);
            assert_eq!(
                view.tokens.expect("tokens").total,
                official,
                "after {piece:?}"
            );
            assert_eq!(ticker.processed_for(1, 1), [text.len(), 0, 0]);
        }
        // An idle tick (no new bytes) changes nothing.
        let status = LiveStatus {
            streaming: true,
            ..LiveStatus::default()
        };
        let full = [text.clone(), String::new(), String::new()];
        let view = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: [String::new(), String::new(), String::new()],
                session_generation: 1,
                message_generation: 1,
                policy: LiveCalculation::Auto,
                requirements: req,
                tokenizer: &loader,
                settled: None,
            },
            || full.clone(),
        );
        let official = tokenizer.count(&text);
        assert_eq!(view.tokens.expect("tokens").total, official);
        assert_eq!(ticker.finalize_exact(Some(official), 1), Some(official));
        assert_eq!(ticker.stats().finalize_mismatches, 0);
    }

    /// V19-07 review round 2: when the provider starts reporting a running
    /// total after the exact counter already ran, the counter is dropped
    /// (no BPE while the provider suffices, no starved counter) — no
    /// finalize comparison is owed for that message, and the byte base keeps
    /// advancing so a later local need rebuilds from the full text.
    #[test]
    fn provider_total_mid_message_drops_the_exact_counter() {
        let tokenizer = tokenizer_for_model(Some("glm-5.3")).expect("glm");
        let loader = || Some(Arc::clone(&tokenizer));
        let config = parse_items(
            json!({"type": "items", "items": ["live-output-tokens"]})
                .as_object()
                .expect("object"),
        );
        let req = TokenRequirements::for_items(&config);
        let mut ticker = LiveTicker::new();
        let mut text = String::new();
        let tick =
            |ticker: &mut LiveTicker, text: &mut String, delta: &str, provider: Option<u64>| {
                text.push_str(delta);
                let base = ticker.processed_for(1, 1);
                let deltas = [text[base[0]..].to_owned(), String::new(), String::new()];
                let status = LiveStatus {
                    streaming: true,
                    output_tokens: provider,
                    ..LiveStatus::default()
                };
                let full = [text.clone(), String::new(), String::new()];
                ticker.live_view(
                    LiveFrameInput {
                        status: &status,
                        deltas,
                        session_generation: 1,
                        message_generation: 1,
                        policy: LiveCalculation::Auto,
                        requirements: req,
                        tokenizer: &loader,
                        settled: None,
                    },
                    || full.clone(),
                );
            };
        tick(&mut ticker, &mut text, "hello world", None);
        assert!(ticker.has_exact_for(1));
        // The provider starts reporting: the counter is dropped, the base
        // keeps advancing, and nothing is compared at message_end.
        tick(&mut ticker, &mut text, " again and again", Some(500));
        assert!(!ticker.has_exact_for(1));
        assert_eq!(ticker.processed_for(1, 1), [text.len(), 0, 0]);
        assert_eq!(ticker.finalize_exact(Some(tokenizer.count(&text)), 1), None);
        assert_eq!(ticker.stats().finalize_comparisons, 0);
        assert_eq!(ticker.stats().finalize_mismatches, 0);
        // A later local need (tiers selected again) rebuilds from the full
        // text and finalizes equal to the official pass.
        text.push_str(" tail");
        let status = LiveStatus {
            streaming: true,
            output_tokens: Some(501),
            ..LiveStatus::default()
        };
        let full = [text.clone(), String::new(), String::new()];
        let view = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: [" tail".into(), "".into(), "".into()],
                session_generation: 1,
                message_generation: 1,
                policy: LiveCalculation::Auto,
                requirements: TokenRequirements {
                    needs_total: true,
                    needs_tiers: true,
                },
                tokenizer: &loader,
                settled: None,
            },
            || full.clone(),
        );
        assert_eq!(
            view.tokens.expect("tokens").source,
            TokenSource::ExactTokenizer
        );
        let official = tokenizer.count(&text);
        assert_eq!(ticker.finalize_exact(Some(official), 1), Some(official));
        assert_eq!(ticker.stats().finalize_mismatches, 0);
    }
}
