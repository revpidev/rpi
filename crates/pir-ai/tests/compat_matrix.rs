//! Compat detection matrix hit tests (T13 W4).
//!
//! Covers the full `detectCompat` provider/baseUrl matrix of upstream
//! `packages/ai/src/api/openai-completions.ts` (zai / zai-coding-cn / together /
//! moonshotai(+-cn) / openrouter / cloudflare-workers-ai / cloudflare-ai-gateway /
//! nvidia / ant-ling / cerebras / xai / chutes / deepseek / opencode) plus the
//! compat deltas baked into the vendored catalog by `generate-models.ts`
//! (zaiToolStream, Kimi deferredToolsMode, OpenAI grammar tools, OpenRouter
//! cacheControlFormat, per-model thinkingLevelMap, …).

use pir_ai::api::anthropic_messages::get_anthropic_compat;
use pir_ai::api::openai_completions::{detect_compat, get_compat, ResolvedOpenAICompletionsCompat};
use pir_ai::generated::get_builtin_model;
use pir_ai::types::{
    CacheControlFormat, DeferredToolsMode, MaxTokensField, Model, ModelThinkingLevel,
    SessionAffinityFormat, ThinkingFormat,
};
use serde_json::json;

fn make_model(provider: &str, base_url: &str, id: &str) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": id, "api": "openai-completions", "provider": provider,
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 1.0, "output": 1.0, "cacheRead": 0.1, "cacheWrite": 1.0},
        "contextWindow": 1000, "maxTokens": 100
    }))
    .expect("model")
}

/// The fully standard (OpenAI first-party) detection result: every matrix
/// case below is expressed as mutations against this baseline.
fn standard() -> ResolvedOpenAICompletionsCompat {
    detect_compat(&make_model("openai", "https://api.openai.com/v1", "gpt-5"))
}

#[test]
fn test_detect_compat_standard_openai_baseline() {
    let compat = standard();
    assert!(compat.supports_store);
    assert!(compat.supports_developer_role);
    assert!(compat.supports_reasoning_effort);
    assert!(compat.supports_usage_in_streaming);
    assert_eq!(compat.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    assert!(!compat.requires_tool_result_name);
    assert!(!compat.requires_assistant_after_tool_result);
    assert!(!compat.requires_thinking_as_text);
    assert!(!compat.requires_reasoning_content_on_assistant_messages);
    assert_eq!(compat.thinking_format, ThinkingFormat::Openai);
    assert!(!compat.zai_tool_stream);
    assert!(compat.supports_strict_mode);
    assert!(!compat.supports_open_ai_grammar_tools);
    assert_eq!(compat.cache_control_format, None);
    assert!(!compat.send_session_affinity_headers);
    assert_eq!(compat.deferred_tools_mode, None);
    assert_eq!(
        compat.session_affinity_format,
        SessionAffinityFormat::Openai
    );
    assert!(compat.supports_long_cache_retention);
}

#[test]
fn test_detect_compat_zai_hits() {
    let mut expected = standard();
    expected.supports_store = false;
    expected.supports_developer_role = false;
    expected.supports_reasoning_effort = false;
    expected.thinking_format = ThinkingFormat::Zai;

    // Provider-id hits.
    for provider in ["zai", "zai-coding-cn"] {
        let model = make_model(provider, "https://example.com/v1", "glm-4.7");
        assert_eq!(detect_compat(&model), expected, "provider {provider}");
    }
    // baseUrl-only hits (custom provider id).
    for base_url in [
        "https://api.z.ai/api/coding/paas/v4",
        "https://open.bigmodel.cn/api/paas/v4",
    ] {
        let model = make_model("custom", base_url, "glm-4.7");
        assert_eq!(detect_compat(&model), expected, "baseUrl {base_url}");
    }
}

#[test]
fn test_detect_compat_together_hits() {
    let mut expected = standard();
    expected.supports_store = false;
    expected.supports_developer_role = false;
    expected.supports_reasoning_effort = false;
    expected.max_tokens_field = MaxTokensField::MaxTokens;
    expected.thinking_format = ThinkingFormat::Together;
    expected.supports_strict_mode = false;
    expected.supports_long_cache_retention = false;

    let model = make_model("together", "https://api.together.ai/v1", "meta-llama/x");
    assert_eq!(detect_compat(&model), expected);
    // baseUrl-only hits: both together.ai and the legacy together.xyz.
    for base_url in ["https://api.together.ai/v1", "https://api.together.xyz/v1"] {
        let model = make_model("custom", base_url, "meta-llama/x");
        assert_eq!(detect_compat(&model), expected, "baseUrl {base_url}");
    }
}

#[test]
fn test_detect_compat_moonshot_hits() {
    let mut expected = standard();
    expected.supports_store = false;
    expected.supports_developer_role = false;
    expected.supports_reasoning_effort = false;
    expected.max_tokens_field = MaxTokensField::MaxTokens;
    expected.supports_strict_mode = false;

    for provider in ["moonshotai", "moonshotai-cn"] {
        let model = make_model(provider, "https://example.com/v1", "kimi-k2");
        assert_eq!(detect_compat(&model), expected, "provider {provider}");
    }
    // baseUrl-only hit (api.moonshot. prefix covers .ai and .cn).
    let model = make_model("custom", "https://api.moonshot.cn/v1", "kimi-k2");
    assert_eq!(detect_compat(&model), expected);
}

#[test]
fn test_detect_compat_openrouter_hits() {
    let mut expected = standard();
    expected.supports_developer_role = false;
    expected.thinking_format = ThinkingFormat::Openrouter;
    expected.session_affinity_format = SessionAffinityFormat::Openrouter;

    // Generic OpenRouter model: no developer role.
    let model = make_model("openrouter", "https://openrouter.ai/api/v1", "meta-llama/x");
    assert_eq!(detect_compat(&model), expected);

    // anthropic/* and openai/* ids get the developer role back.
    let mut dev_expected = expected.clone();
    dev_expected.supports_developer_role = true;
    let model = make_model("openrouter", "https://openrouter.ai/api/v1", "openai/gpt-5");
    assert_eq!(detect_compat(&model), dev_expected);

    // anthropic/* ids additionally select Anthropic cache-control markers.
    let mut anthropic_expected = dev_expected;
    anthropic_expected.cache_control_format = Some(CacheControlFormat::Anthropic);
    let model = make_model(
        "openrouter",
        "https://openrouter.ai/api/v1",
        "anthropic/claude-sonnet-4.6",
    );
    assert_eq!(detect_compat(&model), anthropic_expected);

    // baseUrl-only hit behaves like the provider-id hit.
    let model = make_model("custom", "https://openrouter.ai/api/v1", "meta-llama/x");
    assert_eq!(detect_compat(&model), expected);
}

#[test]
fn test_detect_compat_cloudflare_hits() {
    // Workers AI: store/developer off, long cache retention off; reasoning
    // effort, strict mode and max_completion_tokens stay standard.
    let mut workers = standard();
    workers.supports_store = false;
    workers.supports_developer_role = false;
    workers.supports_long_cache_retention = false;
    let model = make_model(
        "cloudflare-workers-ai",
        "https://api.cloudflare.com/client/v4/accounts/x/ai/v1",
        "@cf/meta/llama",
    );
    assert_eq!(detect_compat(&model), workers);
    let model = make_model("custom", "https://api.cloudflare.com/client/v4", "@cf/x");
    assert_eq!(detect_compat(&model), workers);

    // AI Gateway: additionally no reasoning effort, max_tokens, no strict.
    let mut gateway = workers.clone();
    gateway.supports_reasoning_effort = false;
    gateway.max_tokens_field = MaxTokensField::MaxTokens;
    gateway.supports_strict_mode = false;
    let model = make_model(
        "cloudflare-ai-gateway",
        "https://gateway.ai.cloudflare.com/v1/x/y/openai",
        "gpt-5",
    );
    assert_eq!(detect_compat(&model), gateway);
    let model = make_model("custom", "https://gateway.ai.cloudflare.com/v1/x", "gpt-5");
    assert_eq!(detect_compat(&model), gateway);
}

#[test]
fn test_detect_compat_nvidia_ant_ling_hits() {
    let mut nvidia = standard();
    nvidia.supports_store = false;
    nvidia.supports_developer_role = false;
    nvidia.supports_reasoning_effort = false;
    nvidia.max_tokens_field = MaxTokensField::MaxTokens;
    nvidia.supports_strict_mode = false;
    nvidia.supports_long_cache_retention = false;
    let model = make_model("nvidia", "https://integrate.api.nvidia.com/v1", "nvidia/x");
    assert_eq!(detect_compat(&model), nvidia);
    let model = make_model("custom", "https://integrate.api.nvidia.com/v1", "x");
    assert_eq!(detect_compat(&model), nvidia);

    let mut ant_ling = nvidia.clone();
    ant_ling.supports_strict_mode = true;
    ant_ling.thinking_format = ThinkingFormat::AntLing;
    let model = make_model("ant-ling", "https://api.ant-ling.com/v1", "ring-1");
    assert_eq!(detect_compat(&model), ant_ling);
    let model = make_model("custom", "https://api.ant-ling.com/v1", "ring-1");
    assert_eq!(detect_compat(&model), ant_ling);
}

#[test]
fn test_detect_compat_cerebras_xai_chutes_deepseek_opencode_hits() {
    // Cerebras: only store/developer role flip.
    let mut cerebras = standard();
    cerebras.supports_store = false;
    cerebras.supports_developer_role = false;
    let model = make_model("cerebras", "https://api.cerebras.ai/v1", "llama-4");
    assert_eq!(detect_compat(&model), cerebras);
    let model = make_model("custom", "https://api.cerebras.ai/v1", "llama-4");
    assert_eq!(detect_compat(&model), cerebras);

    // xAI (Grok): additionally no reasoning effort.
    let mut xai = cerebras.clone();
    xai.supports_reasoning_effort = false;
    let model = make_model("xai", "https://api.x.ai/v1", "grok-4");
    assert_eq!(detect_compat(&model), xai);
    let model = make_model("custom", "https://api.x.ai/v1", "grok-4");
    assert_eq!(detect_compat(&model), xai);

    // Chutes (baseUrl-only hit): max_tokens instead of max_completion_tokens.
    let mut chutes = cerebras.clone();
    chutes.max_tokens_field = MaxTokensField::MaxTokens;
    let model = make_model("custom", "https://llm.chutes.ai/v1", "deepseek-x");
    assert_eq!(detect_compat(&model), chutes);

    // DeepSeek: deepseek thinking format + reasoning_content replay.
    let mut deepseek = cerebras.clone();
    deepseek.thinking_format = ThinkingFormat::Deepseek;
    deepseek.requires_reasoning_content_on_assistant_messages = true;
    let model = make_model("deepseek", "https://api.deepseek.com/v1", "deepseek-chat");
    assert_eq!(detect_compat(&model), deepseek);
    let model = make_model("custom", "https://api.deepseek.com/v1", "deepseek-chat");
    assert_eq!(detect_compat(&model), deepseek);

    // opencode provider id, and opencode-go via its opencode.ai baseUrl.
    let opencode = cerebras;
    let model = make_model("opencode", "https://opencode.ai/zen/v1", "grok-build-0.1");
    assert_eq!(detect_compat(&model), opencode);
    let model = make_model("opencode-go", "https://opencode.ai/go/v1", "glm-5.2");
    assert_eq!(detect_compat(&model), opencode);
}

// ---------------------------------------------------------------------------
// Compat deltas baked into the vendored catalog by generate-models.ts
// ---------------------------------------------------------------------------

#[test]
fn test_catalog_zai_tool_stream_baked() {
    let glm47 = get_builtin_model("zai", "glm-4.7").expect("glm-4.7");
    let compat = glm47.compat.as_ref().expect("compat");
    assert_eq!(compat.zai_tool_stream, Some(true));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Zai));
    // Merged over detection: zaiToolStream survives get_compat.
    assert!(get_compat(glm47).zai_tool_stream);

    // Older GLM 4.5 family does not support tool streaming.
    let glm45 = get_builtin_model("zai", "glm-4.5-air").expect("glm-4.5-air");
    assert_eq!(glm45.compat.as_ref().and_then(|c| c.zai_tool_stream), None);
    assert!(!get_compat(glm45).zai_tool_stream);

    // GLM 5.2 fixed thinking level map.
    let glm52 = get_builtin_model("zai", "glm-5.2").expect("glm-5.2");
    let map = glm52.thinking_level_map.as_ref().expect("thinkingLevelMap");
    assert_eq!(map.get(&ModelThinkingLevel::Minimal), Some(&None));
    assert_eq!(
        map.get(&ModelThinkingLevel::Low),
        Some(&Some("high".to_owned()))
    );
    assert_eq!(
        map.get(&ModelThinkingLevel::Max),
        Some(&Some("max".to_owned()))
    );
}

#[test]
fn test_catalog_kimi_deferred_tools_baked() {
    let k3 = get_builtin_model("moonshotai", "kimi-k3").expect("kimi-k3");
    let compat = k3.compat.as_ref().expect("compat");
    assert_eq!(compat.deferred_tools_mode, Some(DeferredToolsMode::Kimi));
    assert_eq!(
        compat.requires_reasoning_content_on_assistant_messages,
        Some(true)
    );
    assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    assert_eq!(
        get_compat(k3).deferred_tools_mode,
        Some(DeferredToolsMode::Kimi)
    );

    let k3_cn = get_builtin_model("moonshotai-cn", "kimi-k3").expect("kimi-k3 cn");
    assert_eq!(
        k3_cn.compat.as_ref().and_then(|c| c.deferred_tools_mode),
        Some(DeferredToolsMode::Kimi)
    );
}

#[test]
fn test_catalog_openai_grammar_tools_baked() {
    // GPT-5 family on first-party Responses APIs passes Lark/regex grammar
    // tools through.
    let gpt5 = get_builtin_model("openai", "gpt-5").expect("gpt-5");
    let compat = gpt5.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_open_ai_grammar_tools, Some(true));
    assert_eq!(compat.supports_strict_mode, Some(true));

    let codex = get_builtin_model("openai-codex", "gpt-5.4").expect("codex gpt-5.4");
    let compat = codex.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_open_ai_grammar_tools, Some(true));
    assert_eq!(compat.supports_tool_search, Some(true));

    // Pre-GPT-5 models are excluded (OpenAI rejects custom tools there).
    let gpt4o = get_builtin_model("openai", "gpt-4o").expect("gpt-4o");
    assert_eq!(
        gpt4o
            .compat
            .as_ref()
            .and_then(|c| c.supports_open_ai_grammar_tools),
        None
    );
}

#[test]
fn test_catalog_anthropic_compat_baked() {
    let fable = get_builtin_model("anthropic", "claude-fable-5").expect("fable-5");
    let compat = fable.compat.as_ref().expect("compat");
    assert_eq!(compat.force_adaptive_thinking, Some(true));
    assert_eq!(compat.supports_strict_tools, Some(true));
    let map = fable.thinking_level_map.as_ref().expect("thinkingLevelMap");
    assert_eq!(map.get(&ModelThinkingLevel::Off), Some(&None));
    assert_eq!(
        map.get(&ModelThinkingLevel::Xhigh),
        Some(&Some("xhigh".to_owned()))
    );
}

#[test]
fn test_catalog_github_copilot_eager_streaming_baked() {
    // Copilot's Claude Haiku 4.5 / Sonnet 4(x) reject eager tool input streaming.
    for id in ["claude-haiku-4.5", "claude-sonnet-4", "claude-sonnet-4.5"] {
        let model = get_builtin_model("github-copilot", id).expect(id);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|c| c.supports_eager_tool_input_streaming),
            Some(false),
            "{id}"
        );
        assert!(!get_anthropic_compat(model).supports_eager_tool_input_streaming);
    }
}

#[test]
fn test_catalog_together_reasoning_variants_baked() {
    // Reasoning-effort models: effort on, openai thinking format.
    let gpt_oss = get_builtin_model("together", "openai/gpt-oss-20b").expect("gpt-oss-20b");
    let compat = gpt_oss.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_reasoning_effort, Some(true));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Openai));
    assert!(get_compat(gpt_oss).supports_reasoning_effort);
    let map = gpt_oss.thinking_level_map.as_ref().expect("map");
    assert_eq!(map.get(&ModelThinkingLevel::Off), Some(&None));

    // Default reasoning models keep the together toggle format with effort off.
    let r1 = get_builtin_model("together", "deepseek-ai/DeepSeek-V4-Pro").expect("ds-v4-pro");
    let compat = r1.compat.as_ref().expect("compat");
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Together));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
}

#[test]
fn test_catalog_openrouter_cache_control_baked() {
    // "~anthropic/..." ids bypass runtime detection (detect keys on an
    // "anthropic/" prefix); the generator bakes cacheControlFormat instead.
    let tilde = get_builtin_model("openrouter", "~anthropic/claude-fable-latest").expect("tilde");
    assert_eq!(
        tilde.compat.as_ref().and_then(|c| c.cache_control_format),
        Some(CacheControlFormat::Anthropic)
    );
    assert_eq!(
        get_compat(tilde).cache_control_format,
        Some(CacheControlFormat::Anthropic)
    );

    // Plain "anthropic/..." ids resolve through detection as well.
    let plain = get_builtin_model("openrouter", "anthropic/claude-3-haiku").expect("plain");
    assert_eq!(
        get_compat(plain).cache_control_format,
        Some(CacheControlFormat::Anthropic)
    );
}

#[test]
fn test_anthropic_tool_references_default_matrix() {
    let anthropic = |id: &str| {
        let mut model = make_model("anthropic", "https://api.anthropic.com", id);
        model.api = pir_ai::types::ApiKind("anthropic-messages".to_owned());
        get_anthropic_compat(&model).supports_tool_references
    };
    // First-party, post-4.5 non-Haiku models support tool_reference blocks.
    assert!(anthropic("claude-opus-4-5"));
    assert!(anthropic("claude-sonnet-4-6"));
    assert!(anthropic("claude-fable-5"));
    // Haiku and pre-4.5 models reject them; non-anthropic providers default off.
    assert!(!anthropic("claude-haiku-4-5"));
    assert!(!anthropic("claude-sonnet-4"));
    assert!(!anthropic("claude-opus-4-1"));
    let other = make_model("bedrock", "https://example.com", "claude-opus-4-5");
    assert!(!get_anthropic_compat(&other).supports_tool_references);
}

// ---------------------------------------------------------------------------
// Routing preferences: full-field serde shape
// ---------------------------------------------------------------------------

#[test]
fn test_openrouter_routing_full_field_roundtrip() {
    // OpenRouterRouting contents are snake_case on the wire (upstream
    // types.ts; the object is sent verbatim as the `provider` request field).
    let value = json!({
        "allow_fallbacks": false,
        "require_parameters": true,
        "data_collection": "deny",
        "zdr": true,
        "enforce_distillable_text": true,
        "order": ["Anthropic", "OpenAI"],
        "only": ["Anthropic"],
        "ignore": ["Azure"],
        "quantizations": ["fp8"],
        "sort": {"by": "price", "partition": "model"},
        "max_price": {"prompt": 5.0, "completion": "10", "image": 1.0, "audio": 2.0, "request": "0.5"},
        "preferred_min_throughput": {"p50": 30.0},
        "preferred_max_latency": 2.5
    });
    let routing: pir_ai::types::OpenRouterRouting =
        serde_json::from_value(value.clone()).expect("routing");
    assert_eq!(serde_json::to_value(&routing).expect("json"), value);

    // String form of `sort` round-trips too.
    let routing: pir_ai::types::OpenRouterRouting =
        serde_json::from_value(json!({"sort": "price"})).expect("routing");
    assert_eq!(
        serde_json::to_value(&routing).expect("json"),
        json!({"sort": "price"})
    );
}

#[test]
fn test_grammar_tool_variants_lark_and_regex() {
    // Both OpenAI grammar variants (lark/regex) survive the compat flag flow.
    let value = json!({"type": "grammar", "variants": {"openai_lark": "start: x", "openai_regex": "[a-z]+"}});
    let config: pir_ai::types::ConstrainedSamplingConfig =
        serde_json::from_value(value.clone()).expect("config");
    assert_eq!(serde_json::to_value(&config).expect("json"), value);
}
