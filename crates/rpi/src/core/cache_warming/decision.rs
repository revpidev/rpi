//! 决策器 — cost-aware pure functions of the cache warmer
//! (cache-warmer.ts:16-107, 362-383, 387-437 @ c596d09d9, #9668).
//!
//! All functions are pure (time/state passed in) so the eligibility matrix
//! and economics are table-testable without a runtime.

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::SessionEntry;
use rpi_agent::session::UsageEntry;
use rpi_ai::api::anthropic_messages::resolve_cache_retention;
use rpi_ai::types::{ApiKind, CacheRetention, Model, ProviderEnv, ThinkingLevel, Usage, UsageCost};
use rpi_ai::utils::cost::calculate_cost;
use rpi_ext_host::types::CacheWarmingAction;

use super::{CacheWarmingStatus, Phase};
use crate::core::cache_warming::WarmingState;

/// Streaming warming never continues past this long after the real request
/// that started it (cache-warmer.ts:16).
pub const MAX_WARMING_AGE_MS: u64 = 60 * 60_000;
/// Idle warming uses a shorter horizon because continuation estimates become
/// less reliable with age (cache-warmer.ts:18).
pub const MAX_IDLE_WARMING_AGE_MS: u64 = 30 * 60_000;
/// A refresh is sent only when it is expected to save at least this many
/// dollars (cache-warmer.ts:20).
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// Chance that a real request arrives before the cache entry expires while
/// the agent sits idle. Measured from upstream usage; per-session estimates
/// were not better than this constant (cache-warmer.ts:26).
pub const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

/// Refresh at 90% of the TTL while preserving at least ten seconds of
/// margin (cache-warmer.ts:29-32). `None` when the TTL is too short to
/// schedule anything.
pub fn get_cache_warming_delay_ms(ttl_ms: u64) -> Option<u64> {
    if ttl_ms <= 10_000 {
        return None;
    }
    Some((ttl_ms.saturating_mul(9) / 10).min(ttl_ms - 10_000).max(1))
}

/// Lifetime of the prompt cache entry a request writes, from the model's
/// `promptCache` tier for the retention the request used (cache-warmer.ts:
/// 33-43). `None` when the model has no lifetime for that tier or caching
/// is off. Retention resolution reuses the adapter-side
/// [`resolve_cache_retention`] (`RPI_CACHE_RETENTION`, ADR-0001 rename).
pub fn get_prompt_cache_ttl_ms(
    model: &Model,
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> Option<u64> {
    let retention = resolve_cache_retention(cache_retention, env);
    if retention == CacheRetention::None {
        return None;
    }
    let prompt_cache = model.prompt_cache?;
    let seconds = match retention {
        CacheRetention::Long => prompt_cache.long,
        _ => prompt_cache.short,
    }?;
    Some(u64::from(seconds) * 1000)
}

/// Whether replaying the request with a one-token output cap leaves its
/// cache entry untouched (cache-warmer.ts:55-58). Anthropic's budget-based
/// thinking (Claude models without adaptive thinking) derives
/// `budget_tokens` from `max_tokens`; the replay would get a different
/// budget, which Anthropic keys the message cache on, and the model could
/// still think for thousands of tokens.
pub fn is_replayable(model: &Model, reasoning: Option<&ThinkingLevel>) -> bool {
    let reasoning_requested = reasoning.is_some();
    if !reasoning_requested || model.api.as_str() != ApiKind::ANTHROPIC_MESSAGES {
        return true;
    }
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.force_adaptive_thinking)
        .unwrap_or(false)
}

/// Prompt size of the most recent real request on the branch, as reported
/// by the provider (cache-warmer.ts:61-70): the last assistant message's
/// `input + cacheRead + cacheWrite`.
pub fn last_prompt_tokens(branch: &[SessionEntry]) -> u64 {
    branch
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::Message(message_entry) => match &message_entry.message {
                AgentMessage::Assistant(assistant) => Some(
                    assistant.usage.input
                        + assistant.usage.cache_read
                        + assistant.usage.cache_write,
                ),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(0)
}

/// `price` (cache-warmer.ts:72-86): cost of a partial usage record.
fn price(model: &Model, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost::default(),
        ..Usage::default()
    };
    calculate_cost(model, &mut usage).total
}

/// Inputs and outcome of one warm-or-stop decision, as shown by `/session`
/// (cache-warmer.ts:91-107).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheWarmingDecision {
    /// `"streaming"` while the agent run that sent the request is active.
    pub phase: Phase,
    /// Price of this refresh: a cache read of the prompt plus one output token.
    pub warm_cost: f64,
    /// Extra price of the next real request if the cache entry is lost.
    pub miss_cost: f64,
    /// Estimated chance that a real request arrives before the entry expires.
    pub continuation_probability: f64,
    /// `continuationProbability * missCost - warmCost`.
    pub expected_savings: f64,
    /// False when the prompt size or the model's prices are unknown.
    pub economics_available: bool,
    /// Pi's decision: `Warm` when `expected_savings` is at least $0.05.
    pub action: CacheWarmingAction,
}

/// `evaluate` (cache-warmer.ts:362-379): the cost model. A refresh is a
/// cache read of the whole prompt plus one output token; the avoided miss
/// is the cache-write (or uncached input) price of the prompt minus its
/// cache-read price. Idle continuation uses the fixed 15% estimate;
/// streaming runs always continue (probability 1).
pub fn evaluate_decision(model: &Model, prompt_tokens: u64, phase: Phase) -> CacheWarmingDecision {
    let cache_hit_cost = price(model, 0, 0, prompt_tokens, 0);
    let cache_miss_cost = if model.cost.rates.cache_write > 0.0 {
        price(model, 0, 0, 0, prompt_tokens)
    } else {
        price(model, prompt_tokens, 0, 0, 0)
    };
    let warm_cost = price(model, 0, 1, prompt_tokens, 0);
    let miss_cost = (cache_miss_cost - cache_hit_cost).max(0.0);
    let continuation_probability = match phase {
        Phase::Idle => IDLE_CONTINUATION_PROBABILITY,
        Phase::Streaming => 1.0,
    };
    let economics_available = prompt_tokens > 0 && (cache_hit_cost > 0.0 || cache_miss_cost > 0.0);
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    CacheWarmingDecision {
        phase,
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available,
        action: if expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
            CacheWarmingAction::Warm
        } else {
            CacheWarmingAction::Stop
        },
    }
}

fn format_dollars(value: f64) -> String {
    if value < 0.0 {
        format!("-${:.3}", value.abs())
    } else {
        format!("${value:.3}")
    }
}

fn format_cache_warming_economics(decision: &CacheWarmingDecision) -> String {
    if !decision.economics_available {
        return "cache economics unavailable".to_owned();
    }
    let probability = (decision.continuation_probability * 100.0).round() as u64;
    let probability_text = match decision.phase {
        Phase::Streaming => {
            format!("{probability}% continuation probability while agent is running")
        }
        Phase::Idle => format!("{probability}% continuation probability"),
    };
    let comparison = if decision.action == CacheWarmingAction::Warm {
        ">="
    } else {
        "<"
    };
    format!(
        "{probability_text}, expected savings {} {comparison} ${:.3}",
        format_dollars(decision.expected_savings),
        CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS
    )
}

fn format_cache_warming_decision_time(next_warm_at: Option<u64>, now: u64) -> String {
    let Some(next_warm_at) = next_warm_at.filter(|at| *at > now) else {
        return "Decision now".to_owned();
    };
    let mut remaining_seconds = (next_warm_at - now).div_ceil(1000);
    let hours = remaining_seconds / 3600;
    remaining_seconds %= 3600;
    let minutes = remaining_seconds / 60;
    let seconds = remaining_seconds % 60;
    let mut parts: Vec<String> = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    format!("Decision in {}", parts.join(" "))
}

/// One-line status for `/session` (cache-warmer.ts:417-431).
pub fn format_cache_warming_status(status: &CacheWarmingStatus, now: u64) -> String {
    let Some(decision) = &status.decision else {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    };
    // A decision is attached once pi (or an extension) acted on it;
    // "inactive" without one never got that far.
    if status.state == WarmingState::Inactive
        && !decision.economics_available
        && !status.extension_override
    {
        return format!(
            "Inactive ({})",
            status.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let details = if status.extension_override {
        format!(
            "extension override, {}",
            format_cache_warming_economics(decision)
        )
    } else {
        format!(
            "{} -> {}",
            format_cache_warming_economics(decision),
            match decision.action {
                CacheWarmingAction::Warm => "warm",
                CacheWarmingAction::Stop => "stop",
            }
        )
    };
    if status.state == WarmingState::Inactive {
        return format!("Stopped ({details})");
    }
    if status.state == WarmingState::Refreshing {
        return format!("Warming cache ({details})");
    }
    format!(
        "{} ({details})",
        format_cache_warming_decision_time(status.next_warm_at, now)
    )
}

/// One-line transcript text for persisted cache-warming usage
/// (cache-warmer.ts:433-437).
pub fn format_cache_warming_usage(entry: &UsageEntry) -> String {
    let note = entry
        .note
        .as_deref()
        .map(|note| format!(" ({note})"))
        .unwrap_or_default();
    // `toFixed(6)` then strip trailing zeros beyond three decimals
    // (cache-warmer.ts:435 regex `/(\.\d{3}\d*?)0+$/`).
    let formatted = format!("{:.6}", entry.usage.cost.total);
    let cost = match formatted.split_once('.') {
        Some((int_part, decimals)) => {
            let trimmed = decimals.trim_end_matches('0');
            let kept = if trimmed.len() >= 3 {
                trimmed.to_owned()
            } else {
                decimals[..3].to_owned()
            };
            format!("${int_part}.{kept}")
        }
        None => format!("${formatted}"),
    };
    format!("Cache warmed{note}: {cost}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_model(prompt_cache: Option<(u32, u32)>) -> Model {
        let prompt_cache = prompt_cache.map(|(short, long)| rpi_ai::types::ModelPromptCache {
            short: Some(short),
            long: Some(long),
        });
        Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            api: ApiKind(ApiKind::ANTHROPIC_MESSAGES.to_owned()),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            prompt_cache,
            context_window: 200_000,
            max_tokens: 16_384,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn derives_eligibility_and_timing_from_retention_and_provider_behavior() {
        let adaptive = anthropic_model(Some((300, 3600)));
        let mut openai = anthropic_model(Some((300, 86_400)));
        openai.provider = "openai".into();
        openai.api = ApiKind(ApiKind::OPENAI_RESPONSES.to_owned());
        let unknown = anthropic_model(None);

        // TTL resolution (upstream cache-warmer.test.ts:128-135).
        assert_eq!(
            get_prompt_cache_ttl_ms(&adaptive, None, None),
            Some(300_000)
        );
        assert_eq!(
            get_prompt_cache_ttl_ms(&adaptive, Some(CacheRetention::Long), None),
            Some(3_600_000)
        );
        assert_eq!(
            get_prompt_cache_ttl_ms(&adaptive, Some(CacheRetention::None), None),
            None
        );
        let long_env: ProviderEnv = [("RPI_CACHE_RETENTION".to_owned(), "long".to_owned())].into();
        assert_eq!(
            get_prompt_cache_ttl_ms(&adaptive, None, Some(&long_env)),
            Some(3_600_000)
        );
        assert_eq!(
            get_prompt_cache_ttl_ms(&openai, Some(CacheRetention::Long), None),
            Some(86_400_000)
        );
        assert_eq!(get_prompt_cache_ttl_ms(&unknown, None, None), None);

        // Delay (90% of TTL with a 10s margin; cache-warmer.test.ts:136-138).
        assert_eq!(get_cache_warming_delay_ms(300_000), Some(270_000));
        assert_eq!(get_cache_warming_delay_ms(60_000), Some(50_000));
        assert_eq!(get_cache_warming_delay_ms(10_000), None);

        // Replayability (budget-thinking Claude skipped while reasoning on).
        let mut budget = adaptive.clone();
        budget.id = "claude-sonnet-4-5".into();
        assert!(!is_replayable(&budget, Some(&ThinkingLevel::Medium)));
        assert!(is_replayable(&budget, None));
        let mut adaptive_thinking = adaptive.clone();
        adaptive_thinking.compat = Some(rpi_ai::types::ModelCompat {
            force_adaptive_thinking: Some(true),
            ..Default::default()
        });
        assert!(is_replayable(
            &adaptive_thinking,
            Some(&ThinkingLevel::Medium)
        ));
        assert!(is_replayable(&openai, Some(&ThinkingLevel::Medium)));
    }

    #[test]
    fn evaluates_streaming_and_idle_economics() {
        // claude-opus-4-6 catalog rates (upstream cache-warmer.test.ts uses
        // the builtin model; numbers below mirror its assertions at
        // promptTokens = 100_000: warmCost ≈ 0.050025, missCost ≈ 0.575).
        let mut model = anthropic_model(Some((300, 3600)));
        model.cost.rates.input = 5.0;
        model.cost.rates.output = 25.0;
        model.cost.rates.cache_read = 0.50;
        model.cost.rates.cache_write = 6.25;

        let streaming = evaluate_decision(&model, 100_000, Phase::Streaming);
        assert_eq!(streaming.continuation_probability, 1.0);
        assert_eq!(streaming.action, CacheWarmingAction::Warm);
        assert!((streaming.warm_cost - 0.050025).abs() < 1e-9);
        assert!((streaming.miss_cost - 0.575).abs() < 1e-9);
        assert!((streaming.expected_savings - (0.575 - 0.050025)).abs() < 1e-9);
        assert!(streaming.economics_available);

        let idle = evaluate_decision(&model, 100_000, Phase::Idle);
        assert!((idle.continuation_probability - 0.15).abs() < 1e-9);
        // 0.15 * 0.575 - 0.050025 = 0.036225 < $0.05 → stop.
        assert_eq!(idle.action, CacheWarmingAction::Stop);

        // Zero prompt tokens → economics unavailable.
        let unavailable = evaluate_decision(&model, 0, Phase::Streaming);
        assert!(!unavailable.economics_available);
        assert_eq!(unavailable.action, CacheWarmingAction::Stop);

        // Zero-cost model → economics unavailable.
        let free = anthropic_model(Some((300, 3600)));
        let free_decision = evaluate_decision(&free, 100_000, Phase::Streaming);
        assert!(!free_decision.economics_available);
    }

    #[test]
    fn formats_status_and_usage_entries() {
        let decision = CacheWarmingDecision {
            phase: Phase::Idle,
            warm_cost: 0.013,
            miss_cost: 0.621,
            continuation_probability: 0.6,
            expected_savings: 0.36,
            economics_available: true,
            action: CacheWarmingAction::Warm,
        };
        let status = CacheWarmingStatus {
            state: WarmingState::Scheduled,
            reason: None,
            next_warm_at: Some(222_000),
            decision: Some(decision),
            extension_override: false,
        };
        assert_eq!(
            format_cache_warming_status(&status, 0),
            "Decision in 3m 42s (60% continuation probability, expected savings $0.360 >= $0.050 -> warm)"
        );

        let mut usage = Usage {
            cache_read: 98_024,
            ..Usage::default()
        };
        usage.cost.total = 0.02949725;
        let entry = UsageEntry {
            id: "u1".into(),
            parent_id: None,
            timestamp: String::new(),
            kind: "cache_warm".into(),
            provider: "anthropic".into(),
            model: "claude-opus-4-6".into(),
            usage,
            note: Some("extension override".into()),
        };
        assert_eq!(
            format_cache_warming_usage(&entry),
            "Cache warmed (extension override): $0.029497"
        );

        // Inactive without a decision never got that far (reason only).
        assert_eq!(
            format_cache_warming_status(
                &CacheWarmingStatus::inactive("waiting for first request"),
                0
            ),
            "Inactive (waiting for first request)"
        );
    }
}
