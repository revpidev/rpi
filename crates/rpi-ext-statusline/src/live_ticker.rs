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
    pub deltas: [String; TIERS],
    pub session_generation: u64,
    pub message_generation: u64,
    pub policy: LiveCalculation,
    pub requirements: TokenRequirements,
    pub tokenizer: Option<Arc<Tokenizer>>,
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
    /// Byte counts of each tier buffer already fed.
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
    /// Whether the exact counter is wanted for the current message (policy
    /// `auto` + a local number needed).
    active: bool,
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
    /// are policy independent). Returns whether a counter was discarded.
    fn drop_exact(&mut self) -> bool {
        self.exact.take().is_some()
    }

    /// Build the frame's live view for the CURRENT message.
    ///
    /// * `status` — the cheap live status (no text);
    /// * `deltas` — the bytes that arrived since the last tick per tier;
    /// * `full_text` — a one-time shared full-text snapshot, requested only
    ///   for a rebuild (policy switch, or a family tokenizer that just
    ///   became known) and for the full-recount fallback;
    /// * `policy` — the policy read from the settings for this tick.
    ///
    /// `full_text` is a closure so the caller copies the message only when
    /// this function asks for it.
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
                    .then(|| self.last_breakdown(policy, requirements, tokenizer.as_ref()))
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
        // Policy switch mid-stream: discard the old policy's counter state
        // and rebuild ONCE from the shared full text (CountReset). A change
        // noticed before the first delta of a message has nothing to rebuild.
        let switched = self.policy.is_some_and(|previous| previous != policy);
        if switched && !self.fed {
            self.policy = Some(policy);
        } else if switched {
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
                    // otherwise rebuild the local counter from the buckets'
                    // text (one pass, one BPE run over the message so far).
                    (LiveCalculation::Estimate, LiveCalculation::Auto) => {
                        let provider_exact = status.output_tokens.or(status.output_tokens_exact);
                        if provider_exact.is_some_and(|total| total > 0)
                            && !requirements.needs_tiers
                        {
                            self.drop_exact();
                        } else {
                            let text = full_text();
                            self.rebuild_exact(&text, policy, requirements, tokenizer.clone());
                        }
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
            self.policy = Some(policy);
        } else {
            self.policy = Some(policy);
        }
        // Feed the new bytes.
        let local_needed = requirements.needs_total || requirements.needs_tiers;
        if local_needed {
            let buckets = self.buckets.get_or_insert_with(Buckets::default);
            for (tier, delta) in deltas.iter().enumerate() {
                buckets.push(tier, delta);
            }
        }
        let provider_streaming = status.output_tokens.or(status.output_tokens_exact);
        let provider_suffices = policy == LiveCalculation::Auto
            && !requirements.needs_tiers
            && provider_streaming.is_some_and(|total| total > 0);
        let want_exact = policy == LiveCalculation::Auto
            && local_needed
            && !provider_suffices
            && tokenizer.is_some();
        if want_exact {
            if self.exact.is_none() && tokenizer.is_some() {
                // First local number of this message after the provider stopped
                // satisfying it, or the family tokenizer just became known:
                // one rebuild pass over the message so far.
                let already = self.processed;
                let text = full_text();
                self.rebuild_exact(&text, policy, requirements, tokenizer.clone());
                self.processed = [text[0].len(), text[1].len(), text[2].len()];
                let _ = already;
            }
            if let Some(exact) = self.exact.as_mut()
                && exact.active
            {
                for (tier, delta) in deltas.iter().enumerate() {
                    exact.counter.push(tier, delta);
                }
                if exact.counter.fallback().is_some() && !exact.reported_fallback {
                    exact.reported_fallback = true;
                    self.stats.count_fallbacks += 1;
                }
            }
        }
        for (tier, delta) in deltas.iter().enumerate() {
            self.processed[tier] += delta.len();
            if !delta.is_empty() {
                self.fed = true;
            }
        }
        let local = self.local_breakdown(policy, requirements, tokenizer.as_ref(), &mut full_text);
        let _ = provider_streaming;
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

    /// The settled view of the message that just ended: the caller passes
    /// the authoritative settlement breakdown (provider exact is overlaid
    /// by `reported_output`), or `None` to keep the last provisional.
    fn last_breakdown(
        &mut self,
        _policy: LiveCalculation,
        requirements: TokenRequirements,
        _tokenizer: Option<&Arc<Tokenizer>>,
    ) -> Option<TokenBreakdown> {
        if !requirements.needs_total && !requirements.needs_tiers {
            return None;
        }
        if let Some(exact) = self.exact.as_mut()
            && exact.active
        {
            return exact.counter.provisional();
        }
        self.buckets
            .as_ref()
            .map(Buckets::breakdown)
            .or(Some(TokenBreakdown::default()))
    }

    fn rebuild_exact(
        &mut self,
        text: &[String; 3],
        policy: LiveCalculation,
        requirements: TokenRequirements,
        tokenizer: Option<Arc<Tokenizer>>,
    ) {
        self.exact = None;
        let Some(tokenizer) = tokenizer else {
            return;
        };
        if policy != LiveCalculation::Auto
            || !(requirements.needs_total || requirements.needs_tiers)
        {
            return;
        }
        let mut counter = IncrementalCounter::new(Arc::clone(&tokenizer));
        for (tier, text) in text.iter().enumerate() {
            counter.push(tier, text);
        }
        self.exact = Some(ExactState {
            tokenizer,
            counter,
            active: true,
            reported_fallback: false,
        });
    }

    /// The local breakdown for a streaming frame under the policy.
    fn local_breakdown(
        &mut self,
        policy: LiveCalculation,
        requirements: TokenRequirements,
        tokenizer: Option<&Arc<Tokenizer>>,
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
            Some(exact) if exact.active => match exact.counter.provisional() {
                Some(provisional) => provisional,
                // CountFallback: full recount at refresh cadence (the
                // message left the incremental path), never a wrong number.
                None => {
                    let text = full_text();
                    exact.tokenizer.count_tiers(&text[0], &text[1], &text[2])
                }
            },
            _ => match tokenizer {
                // No local exact counter (provider total suffices so far):
                // an empty local split; `with_provider_total` places the
                // provider total.
                Some(_) => TokenBreakdown::default(),
                None => self
                    .buckets
                    .as_ref()
                    .map(Buckets::breakdown)
                    .unwrap_or_default(),
            },
        }
    }

    /// Finalize the exact counter for the message that just ended and
    /// return its per-tier counts, or `None` when no exact counter ran.
    /// `official` is the official full pass for the comparison.
    pub fn finalize_exact(&mut self, official: Option<u64>) -> Option<u64> {
        let exact = self.exact.as_mut()?;
        if !exact.active {
            return None;
        }
        match exact.counter.finalize() {
            Some(finalized) => {
                if let Some(official) = official
                    && official != finalized.total
                {
                    self.stats.finalize_mismatches += 1;
                    tracing::warn!(
                        incremental = finalized.total,
                        official,
                        "rpi-statusline: incremental count disagreed with the official tokenizer; \
                         the official value is used and the incremental path is disabled for \
                         this message"
                    );
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

    pub fn fallback(&self) -> Option<CountFallback> {
        self.exact.as_ref()?.counter.fallback()
    }

    pub fn stats(&self) -> TickerStats {
        self.stats
    }

    /// Bytes of each tier buffer already consumed (the caller asks the
    /// engine for the text past these offsets).
    pub fn processed(&self) -> [usize; TIERS] {
        self.processed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::ALL_FIELDS;
    use crate::items::parse_items;
    use serde_json::{Value, json};

    fn requirements(items: Value) -> TokenRequirements {
        let config = parse_items(items.as_object().expect("object"));
        TokenRequirements::for_items(&config)
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

    #[test]
    fn heuristic_buckets_feed_from_deltas_only() {
        let mut ticker = LiveTicker::new();
        let status = LiveStatus {
            streaming: true,
            ..LiveStatus::default()
        };
        let items = json!({"type": "items", "items": ["live-output-tokens"]});
        let config = parse_items(items.as_object().expect("object"));
        let req = TokenRequirements::for_items(&config);
        let view = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: ["你好".into(), "".into(), "".into()],
                session_generation: 1,
                message_generation: 1,
                policy: LiveCalculation::Estimate,
                requirements: req,
                tokenizer: None,
                settled: None,
            },
            || [String::new(), String::new(), String::new()],
        );
        assert_eq!(view.tokens.expect("tokens").total, 2);
        let view = ticker.live_view(
            LiveFrameInput {
                status: &status,
                deltas: [" hello".into(), "".into(), "".into()],
                session_generation: 1,
                message_generation: 1,
                policy: LiveCalculation::Estimate,
                requirements: req,
                tokenizer: None,
                settled: None,
            },
            || [String::new(), String::new(), String::new()],
        );
        // ceil((2/1.2) + (6/3.5)) = ceil(3.37) = 4, not 2 + 2.
        assert_eq!(view.tokens.expect("tokens").total, 4);
        assert_eq!(ticker.stats().count_resets, 0);
    }
}
