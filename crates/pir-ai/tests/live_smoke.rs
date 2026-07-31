//! Live smoke tests against real provider endpoints (task T03 自测清单).
//!
//! Gated (coding-standards §12.6): every test returns immediately unless
//! `PIR_LIVE_TEST=1` is set **and** the provider's API key env var is present.
//! Model ids can be overridden via `PIR_LIVE_<PROVIDER>_MODEL`; base URLs via
//! `PIR_LIVE_<PROVIDER>_BASE_URL`.

use futures::StreamExt;
use pir_ai::api::anthropic_messages::AnthropicMessages;
use pir_ai::api::openai_completions::OpenAiCompletions;
use pir_ai::api::openai_responses::OpenAiResponses;
use pir_ai::models::ProviderStreams;
use pir_ai::types::{Context, DoneReason, Model, StreamEvent, StreamOptions};
use serde_json::json;

struct LiveTarget {
    key_env: &'static str,
    model_env: &'static str,
    base_url_env: &'static str,
    default_model: &'static str,
    default_base_url: &'static str,
    api: &'static str,
    provider: &'static str,
}

const ANTHROPIC: LiveTarget = LiveTarget {
    key_env: "ANTHROPIC_API_KEY",
    model_env: "PIR_LIVE_ANTHROPIC_MODEL",
    base_url_env: "PIR_LIVE_ANTHROPIC_BASE_URL",
    default_model: "claude-haiku-4-5",
    default_base_url: "https://api.anthropic.com",
    api: "anthropic-messages",
    provider: "anthropic",
};

const OPENAI_COMPLETIONS: LiveTarget = LiveTarget {
    key_env: "OPENAI_API_KEY",
    model_env: "PIR_LIVE_OPENAI_MODEL",
    base_url_env: "PIR_LIVE_OPENAI_BASE_URL",
    default_model: "gpt-4o-mini",
    default_base_url: "https://api.openai.com",
    api: "openai-completions",
    provider: "openai",
};

const OPENAI_RESPONSES: LiveTarget = LiveTarget {
    key_env: "OPENAI_API_KEY",
    model_env: "PIR_LIVE_OPENAI_RESPONSES_MODEL",
    base_url_env: "PIR_LIVE_OPENAI_RESPONSES_BASE_URL",
    default_model: "gpt-4o-mini",
    default_base_url: "https://api.openai.com",
    api: "openai-responses",
    provider: "openai",
};

/// Returns `Some((model, options))` when the gate is open, `None` to skip.
fn gate(target: &LiveTarget) -> Option<(Model, StreamOptions)> {
    if std::env::var("PIR_LIVE_TEST").ok().as_deref() != Some("1") {
        return None;
    }
    let api_key = std::env::var(target.key_env).ok()?;
    let model_id =
        std::env::var(target.model_env).unwrap_or_else(|_| target.default_model.to_owned());
    let base_url =
        std::env::var(target.base_url_env).unwrap_or_else(|_| target.default_base_url.to_owned());
    let model: Model = serde_json::from_value(json!({
        "id": model_id, "name": model_id, "api": target.api, "provider": target.provider,
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
        "contextWindow": 128000, "maxTokens": 64
    }))
    .expect("model");
    let options = StreamOptions {
        api_key: Some(api_key),
        max_tokens: Some(64),
        ..StreamOptions::default()
    };
    Some((model, options))
}

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![serde_json::from_value(
            json!({"role": "user", "content": "Reply with the single word: ok", "timestamp": 0}),
        )
        .expect("user")],
        tools: None,
    }
}

async fn smoke(events: Vec<StreamEvent>, label: &str) {
    let Some(StreamEvent::Done { reason, message }) = events.last() else {
        panic!("{label}: expected a terminal done event, got {events:?}");
    };
    assert_eq!(
        *reason,
        DoneReason::Stop,
        "{label}: {:?}",
        message.error_message
    );
    assert!(
        !message.content.is_empty(),
        "{label}: expected non-empty assistant content"
    );
    assert!(message.usage.output > 0, "{label}: expected output usage");
}

#[tokio::test]
async fn test_live_anthropic_messages() {
    let Some((model, options)) = gate(&ANTHROPIC) else {
        return;
    };
    let events = AnthropicMessages
        .stream(&model, &context(), Some(options))
        .collect()
        .await;
    smoke(events, "anthropic-messages").await;
}

#[tokio::test]
async fn test_live_openai_completions() {
    let Some((model, options)) = gate(&OPENAI_COMPLETIONS) else {
        return;
    };
    let events = OpenAiCompletions
        .stream(&model, &context(), Some(options))
        .collect()
        .await;
    smoke(events, "openai-completions").await;
}

#[tokio::test]
async fn test_live_openai_responses() {
    let Some((model, options)) = gate(&OPENAI_RESPONSES) else {
        return;
    };
    let events = OpenAiResponses
        .stream(&model, &context(), Some(options))
        .collect()
        .await;
    smoke(events, "openai-responses").await;
}
