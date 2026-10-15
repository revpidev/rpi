//! Prompt-cache miss detection (`core/cache-stats.ts` @ abe508e1b).
//!
//! Port of upstream `packages/coding-agent/src/core/cache-stats.ts` (174
//! lines): the `prev` request state machine, the detection noise floor and
//! the session-level waste totals consumed by the transcript notices and the
//! `/session` "Cache Re-billed" line.
//!
//! Intentional differences (Rust carrier, not behavior):
//! - The upstream `Map<AssistantMessage, CacheMiss>` (keyed by JS object
//!   reference) is keyed by the assistant `MessageEntry.id` instead, because
//!   the Rust render path clones messages out of the entries and loses the
//!   reference identity (requirements R6.1.1, design 02 §4.1).
//! - `detect_cache_miss` also accepts entries that already contain `message`
//!   (rpi persists in the session listener before the UI drains
//!   `message_end`); the message is skipped during the scan so it cannot
//!   become its own previous request. Upstream entries never contain it.
//! - `ModelPriceSource` is a small trait (upstream duck-types
//!   `ModelRuntime`); [`crate::core::model_runtime::ModelRuntime`] implements
//!   it.
//! - Unparsable usage-entry timestamps fall back to `0` (upstream
//!   `Date.parse` NaN would poison the idle label; rpi is never expected to
//!   write such entries).

use std::collections::HashMap;

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::{SessionEntry, parse_iso8601_ms};
use rpi_ai::types::AssistantMessage;

use crate::core::session_manager::StoredEntry;

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A counted cache miss on a single assistant message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous turn's prompt but not read
    /// from cache.
    pub missed_tokens: u64,
    /// Extra dollars paid vs. a full cache hit; 0 when pricing is unknown.
    pub missed_cost: f64,
    /// Milliseconds since the previous request (which last refreshed the
    /// cache).
    pub idle_ms: i64,
    /// True when the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative cache waste across a session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: u64,
}

/// Minimal pricing lookup (upstream `ModelPriceSource`). Cost is
/// dollars per million tokens.
pub trait ModelPriceSource {
    fn cache_read_price_per_million(&self, provider: &str, model: &str) -> Option<f64>;
}

impl ModelPriceSource for crate::core::model_runtime::ModelRuntime {
    fn cache_read_price_per_million(&self, provider: &str, model: &str) -> Option<f64> {
        self.get_model(provider, model)
            .map(|model| model.cost.rates.cache_read)
    }
}

/// The last request seen by the scan; everything in its prompt should be
/// cached.
#[derive(Debug, Clone)]
struct PreviousRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: i64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity. Distinguishes a total miss on a cache-read-only provider
    /// (OpenAI-style, writes unreported) from a provider that never reports
    /// caching at all.
    reported_cache: bool,
}

/// Scan result: the trailing request state, the cumulative totals and the
/// misses keyed by the paying `MessageEntry.id`.
struct Scan {
    prev: Option<PreviousRequest>,
    totals: CacheWasteTotals,
    misses: HashMap<String, CacheMiss>,
}

fn prompt_tokens(usage: &rpi_ai::types::Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

/// Compute the cache miss for one assistant message relative to the previous
/// request. Returns `None` when nothing is counted: first turn, after a
/// reset, no cache activity ever reported (provider without cache support),
/// or miss below the noise floor.
fn detect_miss(
    prev: Option<&PreviousRequest>,
    message: &AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let usage = &message.usage;
    let prompt = prompt_tokens(usage);
    // A zero-cache turn only counts when cache activity was reported before:
    // on cache-read-only providers that is a total miss, while on providers
    // that never report caching it means nothing.
    if prompt == 0
        || (usage.cache_read + usage.cache_write == 0
            && !prev.is_some_and(|prev| prev.reported_cache))
    {
        return None;
    }
    let prev = prev?;

    let missed_tokens = prev
        .prompt_tokens
        .min(prompt)
        .saturating_sub(usage.cache_read);
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the actual paid rate
    // (input/cacheWrite, incl. write premium) instead of the cache-read
    // rate. Missed tokens can only land in the input or cacheWrite buckets,
    // so the paid rate comes straight from this message's own cost breakdown.
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        models
            .cache_read_price_per_million(&message.provider, &message.model)
            .unwrap_or(0.0)
            / 1_000_000.0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: (message.timestamp - prev.timestamp).max(0),
        model_changed: format!("{}/{}", message.provider, message.model) != prev.model_key,
    })
}

fn as_previous_request(
    message: &AssistantMessage,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let usage = &message.usage;
    let prompt = prompt_tokens(usage);
    if prompt == 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens: prompt,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

fn scan(
    entries: &[StoredEntry],
    models: &dyn ModelPriceSource,
    skip: Option<&AssistantMessage>,
) -> Scan {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();
    let mut misses: HashMap<String, CacheMiss> = HashMap::new();

    for stored in entries {
        let Some(entry) = stored.known() else {
            continue;
        };
        match entry {
            SessionEntry::Compaction(_) | SessionEntry::BranchSummary(_) => {
                // The context legitimately changed; the next turn's prompt is
                // new content, not re-billed content. Model switches are NOT
                // exempt: they re-bill the full prompt and should be counted.
                prev = None;
            }
            SessionEntry::Usage(usage_entry) if usage_entry.kind == "cache_warm" => {
                let prompt = prompt_tokens(&usage_entry.usage);
                if prompt > 0 {
                    prev = Some(PreviousRequest {
                        prompt_tokens: prompt,
                        model_key: format!("{}/{}", usage_entry.provider, usage_entry.model),
                        timestamp: parse_iso8601_ms(&usage_entry.timestamp).unwrap_or(0),
                        reported_cache: true,
                    });
                }
            }
            SessionEntry::Message(message_entry) => {
                let AgentMessage::Assistant(assistant) = &message_entry.message else {
                    continue;
                };
                if skip.is_some_and(|skip| assistant == skip) {
                    // rpi order: the just-completed message may already be
                    // persisted; it must not act as its own `prev`.
                    continue;
                }
                if let Some(miss) = detect_miss(prev.as_ref(), assistant, models) {
                    totals.missed_tokens += miss.missed_tokens;
                    totals.missed_cost += miss.missed_cost;
                    totals.miss_count += 1;
                    misses.insert(message_entry.id.clone(), miss);
                }
                prev = as_previous_request(
                    assistant,
                    prev.as_ref().is_some_and(|prev| prev.reported_cache),
                )
                .or(prev);
            }
            _ => {}
        }
    }

    Scan {
        prev,
        totals,
        misses,
    }
}

/// Cumulative cache waste across a session: prompt tokens that should have
/// been cache reads (they were in the previous turn's prompt) but were
/// re-billed.
pub fn compute_cache_waste(
    entries: &[StoredEntry],
    models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, models, None).totals
}

/// All counted cache misses across a session, keyed by the assistant
/// [`rpi_agent::session::MessageEntry`] id that paid for them. Used to
/// re-derive transcript notices when rebuilding the chat from entries
/// (resume, post-compaction rebuild).
pub fn collect_cache_misses(
    entries: &[StoredEntry],
    models: &dyn ModelPriceSource,
) -> HashMap<String, CacheMiss> {
    scan(entries, models, None).misses
}

/// Detect a cache miss on a just-completed assistant message.
///
/// Upstream requires `entries` to not yet contain `message` (`message_end`
/// fires before persistence); rpi also accepts the persisted order and skips
/// the message itself in that case.
pub fn detect_cache_miss(
    entries: &[StoredEntry],
    message: &AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let prev = scan(entries, models, Some(message)).prev;
    detect_miss(prev.as_ref(), message, models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::session::{MessageEntry, UsageEntry};
    use rpi_ai::types::{ApiKind, AssistantRole, StopReason, Usage, UsageCost};
    use serde_json::json;

    /// Upstream test fixture: every lookup answers $0.3/M.
    struct TestPriceSource;

    impl ModelPriceSource for TestPriceSource {
        fn cache_read_price_per_million(&self, _provider: &str, _model: &str) -> Option<f64> {
            Some(0.3)
        }
    }

    fn zero_cost() -> UsageCost {
        UsageCost::default()
    }

    fn usage(input: u64, cache_read: u64, cache_write: u64, cost: UsageCost) -> Usage {
        Usage {
            input,
            output: 10,
            cache_read,
            cache_write,
            total_tokens: 0,
            cost,
            ..Default::default()
        }
    }

    fn assistant(options: AssistantOptions<'_>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            api: ApiKind("anthropic-messages".to_owned()),
            provider: "test".to_owned(),
            model: options.model.unwrap_or("test-model").to_owned(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: usage(
                options.input,
                options.cache_read,
                options.cache_write,
                options.cost,
            ),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: options.timestamp,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
            duration_ms: None,
        }
    }

    /// Options mirroring the upstream `assistant(options)` helper.
    struct AssistantOptions<'a> {
        input: u64,
        cache_read: u64,
        cache_write: u64,
        cost: UsageCost,
        model: Option<&'a str>,
        timestamp: i64,
    }

    impl Default for AssistantOptions<'_> {
        fn default() -> Self {
            AssistantOptions {
                input: 0,
                cache_read: 0,
                cache_write: 0,
                cost: zero_cost(),
                model: None,
                timestamp: 0,
            }
        }
    }

    fn entry(id: &str, message: AssistantMessage) -> StoredEntry {
        StoredEntry::Known {
            typed: Box::new(SessionEntry::Message(MessageEntry {
                id: id.to_owned(),
                parent_id: None,
                timestamp: String::new(),
                message: AgentMessage::Assistant(message),
            })),
            raw: json!({"type": "message", "id": id}),
        }
    }

    fn iso_ms(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .expect("representable timestamp")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn usage_entry(id: &str, kind: &str, timestamp_ms: i64) -> StoredEntry {
        StoredEntry::Known {
            typed: Box::new(SessionEntry::Usage(UsageEntry {
                id: id.to_owned(),
                parent_id: None,
                timestamp: iso_ms(timestamp_ms),
                kind: kind.to_owned(),
                provider: "test".to_owned(),
                model: "test-model".to_owned(),
                usage: usage(0, 100_000, 0, zero_cost()),
                note: None,
            })),
            raw: json!({"type": "usage", "id": id}),
        }
    }

    fn reset_entry(id: &str) -> StoredEntry {
        StoredEntry::Known {
            typed: Box::new(SessionEntry::Compaction(
                rpi_agent::session::CompactionEntry {
                    id: id.to_owned(),
                    parent_id: None,
                    timestamp: String::new(),
                    summary: String::new(),
                    first_kept_entry_id: None,
                    tokens_before: 0,
                    retained_tail: None,
                    details: None,
                    usage: None,
                    from_hook: None,
                    system_message: None,
                },
            )),
            raw: json!({"type": "compaction", "id": id}),
        }
    }

    /// Upstream fixture: turn 1 writes 100k at $3.75/M.
    fn turn1() -> AssistantMessage {
        assistant(AssistantOptions {
            cache_write: 100_000,
            cost: UsageCost {
                cache_write: 0.375,
                ..zero_cost()
            },
            ..Default::default()
        })
    }

    /// Upstream fixture: turn 2 reads everything back at $0.30/M.
    fn turn2() -> AssistantMessage {
        assistant(AssistantOptions {
            cache_read: 100_000,
            cache_write: 5_000,
            cost: UsageCost {
                cache_read: 0.03,
                cache_write: 0.019,
                ..zero_cost()
            },
            timestamp: 60_000,
            ..Default::default()
        })
    }

    /// Upstream fixture: turn 3 re-bills the previous 105k prompt.
    fn turn3() -> AssistantMessage {
        assistant(AssistantOptions {
            cache_write: 110_000,
            cost: UsageCost {
                cache_write: 0.4125,
                ..zero_cost()
            },
            timestamp: 120_000,
            ..Default::default()
        })
    }

    // --- upstream `computeCacheWaste` cases ---------------------------------

    #[test]
    fn compute_cache_waste_accumulates_tokens_and_cost_across_turns() {
        // Turn 3: full miss, previous 105k prompt re-billed at $3.75/M write.
        let totals = compute_cache_waste(
            &[
                entry("t1", turn1()),
                entry("t2", turn2()),
                entry("t3", turn3()),
            ],
            &TestPriceSource,
        );
        assert_eq!(totals.missed_tokens, 105_000);
        // 105k at ($3.75 - $0.30)/M
        assert!(
            (totals.missed_cost - 0.36225).abs() < 1e-5,
            "{}",
            totals.missed_cost
        );
    }

    #[test]
    fn compute_cache_waste_counts_nothing_for_healthy_sessions() {
        let totals = compute_cache_waste(
            &[entry("t1", turn1()), entry("t2", turn2())],
            &TestPriceSource,
        );
        assert_eq!(totals.missed_tokens, 0);
        assert_eq!(totals.missed_cost, 0.0);
        assert_eq!(totals.miss_count, 0);
    }

    #[test]
    fn compute_cache_waste_skips_the_turn_after_a_compaction_reset() {
        let after_reset = assistant(AssistantOptions {
            cache_write: 20_000,
            cost: UsageCost {
                cache_write: 0.075,
                ..zero_cost()
            },
            ..Default::default()
        });
        let totals = compute_cache_waste(
            &[
                entry("t1", turn1()),
                reset_entry("c"),
                entry("t3", after_reset),
            ],
            &TestPriceSource,
        );
        assert_eq!(totals.missed_tokens, 0);
    }

    #[test]
    fn compute_cache_waste_counts_misses_caused_by_model_switches() {
        let other_model = assistant(AssistantOptions {
            cache_write: 100_000,
            cost: UsageCost {
                cache_write: 0.375,
                ..zero_cost()
            },
            model: Some("other-model"),
            ..Default::default()
        });
        let totals = compute_cache_waste(
            &[entry("t1", turn1()), entry("t2", other_model)],
            &TestPriceSource,
        );
        assert_eq!(totals.missed_tokens, 100_000);
        assert_eq!(totals.miss_count, 1);
    }

    #[test]
    fn compute_cache_waste_skips_providers_that_report_no_cache_activity() {
        let a = assistant(AssistantOptions {
            input: 100_000,
            ..Default::default()
        });
        let b = assistant(AssistantOptions {
            input: 110_000,
            ..Default::default()
        });
        let totals = compute_cache_waste(&[entry("a", a), entry("b", b)], &TestPriceSource);
        assert_eq!(totals.missed_tokens, 0);
    }

    // --- upstream `collectCacheMisses` case ---------------------------------

    #[test]
    fn collect_cache_misses_keys_by_entry_id_with_the_counted_miss() {
        let miss_turn = turn3();
        let misses = collect_cache_misses(
            &[
                entry("t1", turn1()),
                entry("t2", turn2()),
                entry("t3", miss_turn),
            ],
            &TestPriceSource,
        );
        assert_eq!(misses.len(), 1);
        let miss = misses.get("t3").expect("miss keyed by entry id");
        assert_eq!(miss.missed_tokens, 105_000);
    }

    // --- upstream `detectCacheMiss` cases -----------------------------------

    #[test]
    fn detect_cache_miss_detects_a_just_completed_message_with_idle_time() {
        let miss_message = assistant(AssistantOptions {
            cache_write: 110_000,
            cost: UsageCost {
                cache_write: 0.4125,
                ..zero_cost()
            },
            timestamp: 600_000,
            ..Default::default()
        });
        let miss = detect_cache_miss(
            &[entry("t1", turn1()), entry("t2", turn2())],
            &miss_message,
            &TestPriceSource,
        )
        .expect("miss detected");
        assert_eq!(miss.missed_tokens, 105_000);
        assert!((miss.missed_cost - 0.36225).abs() < 1e-5);
        // 600s - 60s since the previous request.
        assert_eq!(miss.idle_ms, 540_000);
        assert!(!miss.model_changed);
    }

    #[test]
    fn detect_cache_miss_flags_model_switches() {
        let other_model = assistant(AssistantOptions {
            cache_write: 110_000,
            cost: UsageCost {
                cache_write: 0.4125,
                ..zero_cost()
            },
            model: Some("other-model"),
            timestamp: 120_000,
            ..Default::default()
        });
        let miss = detect_cache_miss(
            &[entry("t1", turn1()), entry("t2", turn2())],
            &other_model,
            &TestPriceSource,
        )
        .expect("miss detected");
        assert_eq!(miss.missed_tokens, 105_000);
        assert!(miss.model_changed);
    }

    #[test]
    fn detect_cache_miss_uses_only_cache_warm_usage_entries_as_refreshes() {
        let miss_message = assistant(AssistantOptions {
            cache_write: 110_000,
            cost: UsageCost {
                cache_write: 0.4125,
                ..zero_cost()
            },
            timestamp: 600_000,
            ..Default::default()
        });

        let after_cache_warm = detect_cache_miss(
            &[
                entry("t1", turn1()),
                usage_entry("usage-cache-warm", "cache_warm", 500_000),
            ],
            &miss_message,
            &TestPriceSource,
        )
        .expect("miss detected");
        let after_other_usage = detect_cache_miss(
            &[
                entry("t1", turn1()),
                usage_entry("usage-custom", "custom_operation", 500_000),
            ],
            &miss_message,
            &TestPriceSource,
        )
        .expect("miss detected");

        assert_eq!(after_cache_warm.idle_ms, 100_000);
        assert_eq!(after_other_usage.idle_ms, 600_000);
    }

    #[test]
    fn detect_cache_miss_returns_none_for_healthy_turns() {
        let healthy = assistant(AssistantOptions {
            cache_read: 105_000,
            cache_write: 2_000,
            cost: UsageCost {
                cache_read: 0.0315,
                cache_write: 0.0075,
                ..zero_cost()
            },
            timestamp: 120_000,
            ..Default::default()
        });
        assert!(
            detect_cache_miss(
                &[entry("t1", turn1()), entry("t2", turn2())],
                &healthy,
                &TestPriceSource
            )
            .is_none()
        );
    }

    #[test]
    fn detect_cache_miss_returns_none_for_the_first_turn_of_a_session() {
        assert!(detect_cache_miss(&[], &turn1(), &TestPriceSource).is_none());
    }

    // --- rpi persistence-order robustness (FR-A R6) -------------------------

    /// The rpi session listener persists before the UI drains `message_end`,
    /// so the just-completed message may already be in `entries`. Skipping it
    /// must produce exactly the upstream order's result.
    #[test]
    fn detect_cache_miss_ignores_the_message_itself_in_entries() {
        let miss_message = assistant(AssistantOptions {
            cache_write: 110_000,
            cost: UsageCost {
                cache_write: 0.4125,
                ..zero_cost()
            },
            timestamp: 600_000,
            ..Default::default()
        });
        let upstream_order = detect_cache_miss(
            &[entry("t1", turn1()), entry("t2", turn2())],
            &miss_message,
            &TestPriceSource,
        );
        let rpi_order = detect_cache_miss(
            &[
                entry("t1", turn1()),
                entry("t2", turn2()),
                entry("t3", miss_message.clone()),
            ],
            &miss_message,
            &TestPriceSource,
        );
        assert_eq!(upstream_order, rpi_order);
        assert_eq!(rpi_order.expect("miss detected").missed_tokens, 105_000);
    }

    /// A zero-cache first turn is never counted (and never becomes a `prev`).
    #[test]
    fn detect_cache_miss_skips_zero_cache_first_turns() {
        let first = assistant(AssistantOptions {
            input: 50_000,
            timestamp: 0,
            ..Default::default()
        });
        let miss_message = assistant(AssistantOptions {
            input: 60_000,
            timestamp: 60_000,
            ..Default::default()
        });
        assert!(
            detect_cache_miss(&[entry("t1", first)], &miss_message, &TestPriceSource).is_none()
        );
    }
}
