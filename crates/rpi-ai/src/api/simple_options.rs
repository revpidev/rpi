//! Port of `packages/ai/src/api/simple-options.ts` @ pi 0.82.1 (2efa728).
//!
//! Shared `stream_simple` helpers: max-tokens clamping to context, base
//! option construction, reasoning clamps and thinking budget defaults.

use crate::types::{
    Context, Model, SimpleStreamOptions, StreamOptions, ThinkingBudgets, ThinkingLevel,
};
use crate::utils::estimate::estimate_context_tokens;

pub const CONTEXT_SAFETY_TOKENS: u64 = 4096;
pub const MIN_MAX_TOKENS: u32 = 1;

/// `MIN_ANSWER_TOKENS` (d07889da0): tokens always left for the answer when a
/// thinking budget shares the response ceiling.
pub const MIN_ANSWER_TOKENS: u32 = 1024;

/// `clampMaxTokensToContext`: `contextWindow - estimate - 4096` safety margin,
/// `available` floored at 1. No outer floor — an explicit `maxTokens` of 0
/// stays 0 (upstream `Math.min(maxTokens, Math.max(1, available))`).
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u32) -> u32 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let available = model.context_window as i64
        - estimate_context_tokens(context).tokens as i64
        - CONTEXT_SAFETY_TOKENS as i64;
    (max_tokens as i64).min(available.max(MIN_MAX_TOKENS as i64)) as u32
}

/// `buildBaseOptions`: builds the base `StreamOptions`, clamping maxTokens to
/// the model cap and remaining context. `api_key` argument wins over
/// `options.api_key` when set (upstream `apiKey || options?.apiKey`).
pub fn build_base_options(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    api_key: Option<String>,
) -> StreamOptions {
    let base_max_tokens = options
        .and_then(|o| o.stream.max_tokens)
        .unwrap_or(model.max_tokens);
    let mut stream = options.map(|o| o.stream.clone()).unwrap_or_default();
    stream.max_tokens = Some(clamp_max_tokens_to_context(model, context, base_max_tokens));
    stream.api_key = api_key.or_else(|| options.and_then(|o| o.stream.api_key.clone()));
    // 25a2c8dcf (#7568): `{...model.samplingParams, ...options?.samplingParams}`
    // when either is set — per-request keys override model-level keys.
    let option_sampling_params = stream.sampling_params.take();
    stream.sampling_params = if model.sampling_params.is_none() && option_sampling_params.is_none()
    {
        None
    } else {
        let mut merged = model.sampling_params.clone().unwrap_or_default();
        if let Some(option_params) = option_sampling_params {
            for (key, value) in option_params {
                merged.insert(key, value);
            }
        }
        Some(merged)
    };
    stream
}

/// `clampReasoning`: xhigh/max map down to high on budget paths.
pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => Some(ThinkingLevel::High),
        other => other,
    }
}

/// `adjustMaxTokensForThinking` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingMaxTokens {
    pub max_tokens: u32,
    pub thinking_budget: u32,
}

/// `adjustMaxTokensForThinking`: `baseMaxTokens === undefined` means no
/// explicit caller cap — use the model cap and fit thinking inside it.
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u32>,
    model_max_tokens: u32,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> ThinkingMaxTokens {
    let defaults = ThinkingBudgets {
        minimal: Some(1024),
        low: Some(2048),
        medium: Some(8192),
        high: Some(16384),
    };
    let budgets = ThinkingBudgets {
        minimal: custom_budgets.and_then(|b| b.minimal).or(defaults.minimal),
        low: custom_budgets.and_then(|b| b.low).or(defaults.low),
        medium: custom_budgets.and_then(|b| b.medium).or(defaults.medium),
        high: custom_budgets.and_then(|b| b.high).or(defaults.high),
    };

    let level = clamp_reasoning(Some(reasoning_level)).unwrap_or(reasoning_level);
    let mut thinking_budget = match level {
        ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
        ThinkingLevel::Low => budgets.low.unwrap_or(2048),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            budgets.high.unwrap_or(16384)
        }
    };
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = max_tokens.saturating_sub(MIN_ANSWER_TOKENS);
    }

    ThinkingMaxTokens {
        max_tokens,
        thinking_budget,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::UserContent;

    fn make_model(context_window: u32, max_tokens: u32) -> Model {
        serde_json::from_value(json!({
            "id": "m", "name": "m", "api": "anthropic-messages", "provider": "p",
            "baseUrl": "https://example.com", "reasoning": true, "input": ["text"],
            "cost": {"input": 1.0, "output": 1.0, "cacheRead": 0.1, "cacheWrite": 1.0},
            "contextWindow": context_window, "maxTokens": max_tokens
        }))
        .expect("model")
    }

    fn context_with_text(text: &str) -> Context {
        Context {
            system_prompt: None,
            messages: vec![crate::types::Message::User(crate::types::UserMessage {
                role: crate::types::UserRole::User,
                content: UserContent::Text(text.to_owned()),
                timestamp: 0,
            })],
            tools: None,
        }
    }

    #[test]
    fn test_clamp_max_tokens_to_context() {
        let model = make_model(100_000, 8192);
        let context = context_with_text("");
        assert_eq!(clamp_max_tokens_to_context(&model, &context, 8192), 8192);
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context, 200_000),
            100_000 - 4096
        );
        // Floor is 1 even when the estimate exceeds the window.
        let huge = context_with_text(&"x".repeat(800_000));
        assert_eq!(clamp_max_tokens_to_context(&model, &huge, 8192), 1);
        // contextWindow 0 → clamp to floor only.
        let zero = make_model(0, 8192);
        assert_eq!(clamp_max_tokens_to_context(&zero, &context, 0), 1);
        // Explicit maxTokens=0 stays 0 — upstream has no outer floor.
        assert_eq!(clamp_max_tokens_to_context(&model, &context, 0), 0);
    }
    #[test]
    fn test_clamp_reasoning() {
        assert_eq!(
            clamp_reasoning(Some(ThinkingLevel::Xhigh)),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            clamp_reasoning(Some(ThinkingLevel::Max)),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            clamp_reasoning(Some(ThinkingLevel::Low)),
            Some(ThinkingLevel::Low)
        );
        assert_eq!(clamp_reasoning(None), None);
    }

    #[test]
    fn test_adjust_max_tokens_for_thinking_defaults() {
        // Default budget table: minimal 1024 / low 2048 / medium 8192 / high 16384.
        let result =
            adjust_max_tokens_for_thinking(Some(4096), 128_000, ThinkingLevel::Medium, None);
        assert_eq!(result.thinking_budget, 8192);
        assert_eq!(result.max_tokens, 4096 + 8192);
    }

    #[test]
    fn test_adjust_max_tokens_for_thinking_model_cap() {
        // Undefined base → model cap; budget must leave minOutput 1024.
        let result = adjust_max_tokens_for_thinking(None, 4096, ThinkingLevel::High, None);
        assert_eq!(result.max_tokens, 4096);
        assert_eq!(result.thinking_budget, 4096 - 1024);
    }

    #[test]
    fn test_adjust_max_tokens_for_thinking_xhigh_downgrades() {
        let result =
            adjust_max_tokens_for_thinking(Some(1024), 128_000, ThinkingLevel::Xhigh, None);
        assert_eq!(result.thinking_budget, 16384);
    }

    #[test]
    fn test_adjust_max_tokens_for_thinking_custom_budgets() {
        let budgets = ThinkingBudgets {
            minimal: None,
            low: Some(555),
            medium: None,
            high: None,
        };
        let result =
            adjust_max_tokens_for_thinking(Some(1024), 128_000, ThinkingLevel::Low, Some(&budgets));
        assert_eq!(result.thinking_budget, 555);
    }

    // -- samplingParams merge (25a2c8dcf @ 4181f66, #7568) --------------------

    fn sampling_params(
        pairs: &[(&str, serde_json::Value)],
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        Some(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    fn options_with_sampling(
        sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> SimpleStreamOptions {
        SimpleStreamOptions {
            stream: StreamOptions {
                sampling_params,
                ..StreamOptions::default()
            },
            reasoning: None,
            thinking_budgets: None,
        }
    }

    /// Upstream sampling-options.test.ts: "omits sampling params when neither
    /// options nor model set them".
    #[test]
    fn test_build_base_options_sampling_params_unset() {
        let model = make_model(128_000, 8192);
        let base = build_base_options(&model, &context_with_text("hi"), None, None);
        assert_eq!(base.sampling_params, None);
    }

    /// Upstream: "applies model-level sampling params".
    #[test]
    fn test_build_base_options_sampling_params_model_level() {
        let mut model = make_model(128_000, 8192);
        model.sampling_params =
            sampling_params(&[("temperature", json!(1)), ("top_p", json!(0.95))]);
        let base = build_base_options(&model, &context_with_text("hi"), None, None);
        assert_eq!(
            base.sampling_params,
            sampling_params(&[("temperature", json!(1)), ("top_p", json!(0.95))])
        );
    }

    /// Upstream: "merges stream-option keys over model-level keys".
    #[test]
    fn test_build_base_options_sampling_params_request_over_model() {
        let mut model = make_model(128_000, 8192);
        model.sampling_params = sampling_params(&[("top_p", json!(0.95)), ("min_p", json!(0.05))]);
        let options = options_with_sampling(sampling_params(&[("top_p", json!(0.5))]));
        let base = build_base_options(&model, &context_with_text("hi"), Some(&options), None);
        assert_eq!(
            base.sampling_params,
            sampling_params(&[("top_p", json!(0.5)), ("min_p", json!(0.05))])
        );
    }

    /// Stream-option-only sampling params pass through.
    #[test]
    fn test_build_base_options_sampling_params_request_only() {
        let model = make_model(128_000, 8192);
        let options = options_with_sampling(sampling_params(&[
            ("top_p", json!(0.95)),
            ("top_k", json!(0)),
            ("min_p", json!(0)),
        ]));
        let base = build_base_options(&model, &context_with_text("hi"), Some(&options), None);
        assert_eq!(
            base.sampling_params,
            sampling_params(&[
                ("top_p", json!(0.95)),
                ("top_k", json!(0)),
                ("min_p", json!(0))
            ])
        );
    }
}
