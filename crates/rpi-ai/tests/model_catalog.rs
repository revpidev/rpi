//! Built-in model catalog validation (T13 W4).
//!
//! Three layers:
//! 1. Integrity: every vendored `src/providers/data/*.json` matches the sha256
//!    recorded in the vendored `.manifest.json` (upstream `model-data.ts`).
//! 2. Provenance: the vendored set is byte-compared, model-by-model and
//!    field-by-field, against the pinned upstream
//!    `external/pi/packages/ai/src/providers/data/` (read-only reference).
//! 3. Runtime shape: the parsed catalog (`rpi_ai::generated`) preserves
//!    provider set, model order, and every field.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rpi_ai::auth::{find_env_keys, get_env_api_key};
use rpi_ai::generated::{builtin_catalog, get_builtin_model_data_generated_at, get_builtin_models};
use rpi_ai::types::{InputModality, MaxTokensField};
use rpi_ai::types::{ProviderEnv, ThinkingFormat};
use sha2::Digest;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/providers/data")
}

fn upstream_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../external/pi/packages/ai/src/providers/data")
        .canonicalize()
        .expect("upstream data dir (submodule external/pi must be checked out)")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn provider_files(dir: &std::path::Path) -> BTreeMap<String, PathBuf> {
    std::fs::read_dir(dir)
        .expect("read data dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
        .map(|name| (name.clone(), dir.join(name)))
        .collect()
}

#[test]
fn test_vendored_files_match_manifest_sha256() {
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(data_dir().join(".manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    let files = manifest["files"].as_object().expect("files");
    assert_eq!(files.len(), 39);
    for (name, hash) in files {
        let bytes = std::fs::read(data_dir().join(name)).expect("vendored file");
        assert_eq!(
            &sha256_hex(&bytes),
            hash.as_str().expect("hash string"),
            "sha256 mismatch for {name}"
        );
    }
}

#[test]
fn test_vendored_set_matches_upstream_file_set() {
    let vendored = provider_files(&data_dir());
    let upstream = provider_files(&upstream_data_dir());
    assert_eq!(vendored.len(), 39);
    assert_eq!(
        vendored.keys().collect::<Vec<_>>(),
        upstream.keys().collect::<Vec<_>>(),
        "vendored file set diverges from upstream"
    );
}

/// TS has a single `number` type; our typed cost fields are `f64` and
/// re-serialize integer JSON as `0.0`. Normalize every number to f64 so the
/// comparison is about field content, not int/float representation.
fn normalize_numbers(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) => serde_json::Value::from(n.as_f64().expect("finite")),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_numbers).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_numbers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn test_catalog_field_by_field_against_upstream() {
    let catalog = builtin_catalog().expect("catalog parses");
    let upstream = provider_files(&upstream_data_dir());
    let mut total = 0usize;

    for (name, path) in &upstream {
        let provider = name.trim_end_matches(".json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("upstream file"))
                .expect("upstream json");
        let groups = json.as_object().expect("groups");

        // Upstream flattenModelCatalog order: groups in file order, models in
        // group order (both key-sorted by the generator).
        let expected: Vec<(&str, &serde_json::Value)> = groups
            .values()
            .flat_map(|group| {
                group
                    .as_object()
                    .expect("model group")
                    .iter()
                    .map(|(id, model)| (id.as_str(), model))
            })
            .collect();

        let models = catalog.models(provider);
        assert_eq!(
            models.len(),
            expected.len(),
            "model count diverges for {provider}"
        );
        for (model, (expected_id, expected_value)) in models.iter().zip(&expected) {
            assert_eq!(
                model.id, *expected_id,
                "model order diverges for {provider}"
            );
            assert_eq!(model.provider, provider);
            let actual = normalize_numbers(&serde_json::to_value(model).expect("serialize model"));
            assert_eq!(
                &actual,
                &normalize_numbers(expected_value),
                "field-level divergence for {provider}:{expected_id}"
            );
            total += 1;
        }
    }
    assert_eq!(total, 1217);
}

#[test]
fn test_catalog_accessors_and_generated_at() {
    // Every registry provider with a catalog entry yields models.
    for spec in rpi_ai::providers::BUILTIN_PROVIDERS {
        if spec.in_catalog {
            assert!(!get_builtin_models(spec.id).is_empty(), "{}", spec.id);
        }
    }
    // Pinned to the vendored .manifest.json generatedAt
    // (2026-08-11T04:37:23.682Z); update on catalog refresh.
    assert_eq!(
        get_builtin_model_data_generated_at(),
        Some(1_786_423_043_682)
    );
}

// ---------------------------------------------------------------------------
// T26: Baseten golden tests (port of baseten-models.test.ts @ c1019d920)
// ---------------------------------------------------------------------------

fn get_model(provider: &str, id: &str) -> rpi_ai::types::Model {
    get_builtin_models(provider)
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("model {provider}/{id} not found"))
        .clone()
}

/// "registers GLM 5.2 as the default OpenAI-compatible reasoning model"
/// (baseten-models.test.ts).
#[test]
fn test_baseten_glm52_reasoning_model_fields() {
    let model = get_model("baseten", "zai-org/GLM-5.2");
    assert_eq!(model.api.as_str(), "openai-completions");
    assert_eq!(model.provider, "baseten");
    assert_eq!(model.base_url, "https://inference.baseten.co/v1");
    assert!(model.reasoning);
    // thinkingLevelMap: off→none, high→high, max→max (rest null)
    let tlm = model.thinking_level_map.as_ref().expect("thinkingLevelMap");
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::Off),
        Some(&Some("none".to_owned()))
    );
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::High),
        Some(&Some("high".to_owned()))
    );
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::Max),
        Some(&Some("max".to_owned()))
    );
    assert_eq!(model.input, vec![InputModality::Text]);
    assert_eq!(model.context_window, 1048576);
    assert_eq!(model.max_tokens, 262144);
    assert_eq!(model.cost.rates.input, 1.4);
    assert_eq!(model.cost.rates.output, 4.4);
    assert_eq!(model.cost.rates.cache_read, 0.3);
    assert_eq!(model.cost.rates.cache_write, 0.0);

    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_store, Some(false));
    assert_eq!(compat.supports_developer_role, Some(false));
    assert_eq!(compat.supports_reasoning_effort, Some(true));
    assert_eq!(compat.supports_usage_in_streaming, Some(true));
    assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    assert_eq!(compat.supports_strict_mode, Some(true));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Baseten));
    // chatTemplateArgs: { enable_thinking: { $var: "thinking.enabled" } }
    let cta = compat
        .chat_template_args
        .as_ref()
        .expect("chatTemplateArgs");
    assert!(cta.contains_key("enable_thinking"));
}

/// "models Kimi K2.6 reasoning as an explicit off/on toggle"
/// (baseten-models.test.ts).
#[test]
fn test_baseten_kimi_k26_toggle_thinking() {
    let model = get_model("baseten", "moonshotai/Kimi-K2.6");
    let tlm = model.thinking_level_map.as_ref().expect("thinkingLevelMap");
    // off→off, high→high (rest null): explicit off/on toggle
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::Off),
        Some(&Some("off".to_owned()))
    );
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::High),
        Some(&Some("high".to_owned()))
    );
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::Max),
        Some(&None)
    );

    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.supports_reasoning_effort, Some(false));
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Baseten));
    assert!(compat.chat_template_args.is_some());
}

/// "resolves BASETEN_API_KEY from the environment" (baseten-models.test.ts).
#[test]
fn test_baseten_env_key_resolution() {
    let env = ProviderEnv::from([("BASETEN_API_KEY".to_owned(), "test-baseten-key".to_owned())]);
    assert_eq!(
        find_env_keys("baseten", Some(&env)),
        Some(vec!["BASETEN_API_KEY".to_owned()])
    );
    assert_eq!(
        get_env_api_key("baseten", Some(&env)).as_deref(),
        Some("test-baseten-key")
    );
}

/// All 16 Baseten models are non-deprecated (the generator's
/// `processBasetenModels` skips status=="deprecated").
#[test]
fn test_baseten_deprecated_models_filtered() {
    let models = get_builtin_models("baseten");
    // 16 models = upstream catalog after deprecated filtering.
    assert_eq!(
        models.len(),
        16,
        "Baseten should have 16 non-deprecated models"
    );
    // No model name or id contains "deprecated".
    for model in models.iter() {
        assert!(
            !model.id.to_lowercase().contains("deprecat")
                && !model.name.to_lowercase().contains("deprecat"),
            "deprecated model leaked: {}",
            model.id
        );
    }
}

// ---------------------------------------------------------------------------
// T26: Qwen Token Plan Individual strict whitelist
// (port of generate-models-strict.test.ts @ c03d78bdc)
// ---------------------------------------------------------------------------

/// The upstream `QWEN_TOKEN_PLAN_INDIVIDUAL_MODEL_IDS` whitelist has exactly 7
/// IDs; the vendored catalog must match. Upstream drift = explicit failure.
/// Port of `assertExactModelIds` intent (generate-models-strict.test.ts).
#[test]
fn test_qwen_token_plan_individual_whitelist_exact() {
    const EXPECTED: &[&str] = &[
        "deepseek-v4-flash-0731",
        "deepseek-v4-pro",
        "glm-5.2",
        "qwen3.6-flash",
        "qwen3.7-max",
        "qwen3.7-plus",
        "qwen3.8-max",
    ];
    let models = get_builtin_models("qwen-token-plan-individual");
    let mut actual: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    actual.sort();
    let mut expected: Vec<&str> = EXPECTED.to_vec();
    expected.sort();
    assert_eq!(
        actual, expected,
        "qwen-token-plan-individual whitelist diverged"
    );
    assert_eq!(models.len(), 7);
}

/// Individual variant shares QWEN_TOKEN_PLAN_API_KEY and the international
/// endpoint (port of qwen-token-plan-individual.ts).
#[test]
fn test_qwen_token_plan_individual_shares_api_key() {
    let env = ProviderEnv::from([(
        "QWEN_TOKEN_PLAN_API_KEY".to_owned(),
        "shared-key".to_owned(),
    )]);
    assert_eq!(
        find_env_keys("qwen-token-plan-individual", Some(&env)),
        Some(vec!["QWEN_TOKEN_PLAN_API_KEY".to_owned()])
    );
    let models = get_builtin_models("qwen-token-plan-individual");
    let first = models.first().expect("has models");
    assert_eq!(
        first.base_url,
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"
    );
}

// ---------------------------------------------------------------------------
// T26: Catalog correction assertions (8 items, data baked into vendored JSON)
// ---------------------------------------------------------------------------

/// 1. qwen3.8-max-preview → qwen3.8-max (2f7f75a20, #7670).
#[test]
fn test_correction_qwen38_max_rename() {
    let models = get_builtin_models("qwen-token-plan-individual");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"qwen3.8-max"),
        "qwen3.8-max should be present"
    );
    assert!(
        !ids.contains(&"qwen3.8-max-preview"),
        "qwen3.8-max-preview should be absent"
    );
}

/// 2. Copilot Grok 4.5 routed via Responses API (720f0e8ee, #7560).
#[test]
fn test_correction_copilot_grok45_responses() {
    let model = get_model("github-copilot", "grok-4.5");
    assert_eq!(model.api.as_str(), "openai-responses");
}

/// 3. Fireworks Kimi K3: openai-completions + reasoning-effort + deferred
///    tools (a688e257c, #7199).
#[test]
fn test_correction_fireworks_kimi_k3_compat() {
    let model = get_model("fireworks", "accounts/fireworks/models/kimi-k3");
    assert_eq!(model.api.as_str(), "openai-completions");
    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.thinking_format, Some(ThinkingFormat::Openai));
    assert_eq!(
        compat.requires_reasoning_content_on_assistant_messages,
        Some(true)
    );
    assert_eq!(
        compat.deferred_tools_mode,
        Some(rpi_ai::types::DeferredToolsMode::Kimi)
    );
    assert_eq!(compat.send_session_affinity_headers, Some(true));
}

/// 4. Fireworks GLM 5.2: session affinity + no long cache retention
///    (b9497c8c1, #7676).
#[test]
fn test_correction_fireworks_glm52_session_affinity() {
    let model = get_model("fireworks", "accounts/fireworks/models/glm-5p2");
    assert_eq!(model.api.as_str(), "openai-completions");
    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(compat.send_session_affinity_headers, Some(true));
    assert_eq!(compat.supports_long_cache_retention, Some(false));
}

/// 5. GPT-5.6 Terra/Luna price reduction (b889a0ce3).
#[test]
fn test_correction_gpt56_pricing() {
    let luna = get_model("openai", "gpt-5.6-luna");
    assert_eq!(luna.cost.rates.input, 0.2);
    assert_eq!(luna.cost.rates.output, 1.2);
    assert_eq!(luna.cost.rates.cache_read, 0.02);
    assert_eq!(luna.cost.rates.cache_write, 0.25);

    let terra = get_model("openai", "gpt-5.6-terra");
    assert_eq!(terra.cost.rates.input, 2.0);
    assert_eq!(terra.cost.rates.output, 12.0);
    assert_eq!(terra.cost.rates.cache_read, 0.2);
    assert_eq!(terra.cost.rates.cache_write, 2.5);
}

/// 6. Groq Qwen reasoning override → qwen/qwen3.6-27b (71f6c25c3).
#[test]
fn test_correction_groq_qwen_reasoning_override() {
    let model = get_model("groq", "qwen/qwen3.6-27b");
    let tlm = model.thinking_level_map.as_ref().expect("thinkingLevelMap");
    assert_eq!(
        tlm.get(&rpi_ai::types::ModelThinkingLevel::High),
        Some(&Some("default".to_owned()))
    );
    // The old qwen/qwen3-32b should be absent.
    let models = get_builtin_models("groq");
    assert!(
        !models.iter().any(|m| m.id == "qwen/qwen3-32b"),
        "old qwen/qwen3-32b should be replaced by qwen/qwen3.6-27b"
    );
}

/// 7. OpenCode Go display name (05558a792, #7157).
#[test]
fn test_correction_opencode_go_display_name() {
    let go_provider = rpi_ai::providers::builtin_providers()
        .into_iter()
        .find(|p| p.id() == "opencode-go")
        .expect("opencode-go provider");
    assert_eq!(go_provider.name(), "OpenCode Go");
}

/// 8. Copilot policy fallback is tested in auth/oauth/github_copilot.rs
///    (parse_available_model_ids_policy_fallback_* tests). This test
///    verifies the Individual-endpoint base URL is correct.
#[test]
fn test_correction_copilot_individual_endpoint() {
    let copilot = rpi_ai::providers::builtin_providers()
        .into_iter()
        .find(|p| p.id() == "github-copilot")
        .expect("github-copilot provider");
    assert_eq!(
        copilot.base_url(),
        Some("https://api.individual.githubcopilot.com")
    );
}
