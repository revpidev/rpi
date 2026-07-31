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

    const MIN_OUTPUT_TOKENS: u32 = 1024;
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
        thinking_budget = max_tokens.saturating_sub(MIN_OUTPUT_TOKENS);
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
}
