//! Model resolution, scoping, and initial selection.
//!
//! Port of `packages/coding-agent/src/core/model-resolver.ts` @ pi 0.82.1
//! (2efa728), plus `defaults.ts` (`DEFAULT_THINKING_LEVEL`).
//!
//! Side effects are lifted to the caller: upstream prints warnings/errors and
//! `process.exit(1)` inside `findInitialModel`; here the fallible path returns
//! `Err(display_message)` and the app layer prints to stderr and exits 1.

use pir_ai::models::models_are_equal;
use pir_ai::types::{Model, ModelThinkingLevel as ThinkingLevel};

use crate::cli::args::parse_thinking_level;
use crate::core::model_runtime::ModelRuntime;

/// `DEFAULT_THINKING_LEVEL` (defaults.ts:3).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// `defaultModelPerProvider` (model-resolver.ts:14-53) — ids pinned to the
/// upstream commit; unknown providers fall through to "first available".
pub const DEFAULT_MODEL_PER_PROVIDER: [(&str, &str); 38] = [
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-5.5"),
    ("radius", "auto"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.5"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "zai-glm-4.7"),
    ("zai", "glm-5.1"),
    ("zai-coding-cn", "glm-5.1"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
    ("together", "moonshotai/Kimi-K2.6"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k2.6"),
    ("kimi-coding", "kimi-for-coding"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    (
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    ),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

/// `defaultModelPerProvider` lookup (`hasDefaultModelProvider` /
/// `defaultModelPerProvider[providerId]`, interactive-mode.ts:246-248,
/// 5103) — used by the post-login model auto-selection.
pub(crate) fn default_model_for_provider(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, id)| *id)
}
/// `ScopedModel` (model-resolver.ts:55-59).
#[derive(Debug, Clone)]
pub struct ScopedModel {
    pub model: Model,
    /// Thinking level if explicitly specified in pattern (e.g., "model:high").
    pub thinking_level: Option<ThinkingLevel>,
}

/// `isAlias` (model-resolver.ts:65-72): ends with `-latest`, or does not end
/// with a `-YYYYMMDD` date suffix.
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    if bytes.len() < 9 {
        return true;
    }
    let tail = &bytes[bytes.len() - 9..];
    !(tail[0] == b'-' && tail[1..].iter().all(|b| b.is_ascii_digit()))
}

/// `findExactModelReferenceMatch` (model-resolver.ts:79-121).
pub fn find_exact_model_reference_match(
    model_reference: &str,
    available_models: &[Model],
) -> Option<Model> {
    let trimmed = model_reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_lowercase();

    let canonical_matches: Vec<&Model> = available_models
        .iter()
        .filter(|model| format!("{}/{}", model.provider, model.id).to_lowercase() == normalized)
        .collect();
    if canonical_matches.len() == 1 {
        return Some(canonical_matches[0].clone());
    }
    if canonical_matches.len() > 1 {
        return None;
    }

    if let Some(slash_index) = trimmed.find('/') {
        let provider = trimmed[..slash_index].trim();
        let model_id = trimmed[slash_index + 1..].trim();
        if !provider.is_empty() && !model_id.is_empty() {
            let provider_matches: Vec<&Model> = available_models
                .iter()
                .filter(|model| {
                    model.provider.to_lowercase() == provider.to_lowercase()
                        && model.id.to_lowercase() == model_id.to_lowercase()
                })
                .collect();
            if provider_matches.len() == 1 {
                return Some(provider_matches[0].clone());
            }
            if provider_matches.len() > 1 {
                return None;
            }
        }
    }

    let id_matches: Vec<&Model> = available_models
        .iter()
        .filter(|model| model.id.to_lowercase() == normalized)
        .collect();
    if id_matches.len() == 1 {
        Some(id_matches[0].clone())
    } else {
        None
    }
}

/// `tryMatchModel` (model-resolver.ts:127-157).
fn try_match_model(model_pattern: &str, available_models: &[Model]) -> Option<Model> {
    if let Some(exact) = find_exact_model_reference_match(model_pattern, available_models) {
        return Some(exact);
    }

    let lower = model_pattern.to_lowercase();
    let matches: Vec<&Model> = available_models
        .iter()
        .filter(|m| m.id.to_lowercase().contains(&lower) || m.name.to_lowercase().contains(&lower))
        .collect();
    if matches.is_empty() {
        return None;
    }

    let mut aliases: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|m| is_alias(&m.id))
        .collect();
    if !aliases.is_empty() {
        // Prefer the alias that sorts highest.
        aliases.sort_by(|a, b| b.id.cmp(&a.id));
        return Some(aliases[0].clone());
    }
    let mut dated: Vec<&Model> = matches;
    dated.sort_by(|a, b| b.id.cmp(&a.id));
    Some(dated[0].clone())
}

/// `ParsedModelResult` (model-resolver.ts:159-164).
#[derive(Debug, Default)]
pub struct ParsedModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
}

fn build_fallback_model(
    provider: &str,
    model_id: &str,
    available_models: &[Model],
) -> Option<Model> {
    let provider_models: Vec<&Model> = available_models
        .iter()
        .filter(|m| m.provider == provider)
        .collect();
    if provider_models.is_empty() {
        return None;
    }
    let base_model = match default_model_for_provider(provider) {
        Some(default_id) => provider_models
            .iter()
            .find(|m| m.id == default_id)
            .copied()
            .unwrap_or(provider_models[0]),
        None => provider_models[0],
    };
    let mut model = base_model.clone();
    model.id = model_id.to_owned();
    model.name = model_id.to_owned();
    Some(model)
}

/// `parseModelPattern` (model-resolver.ts:195-248).
pub fn parse_model_pattern(
    pattern: &str,
    available_models: &[Model],
    allow_invalid_thinking_level_fallback: bool,
) -> ParsedModelResult {
    // Try exact match first.
    if let Some(exact) = try_match_model(pattern, available_models) {
        return ParsedModelResult {
            model: Some(exact),
            thinking_level: None,
            warning: None,
        };
    }

    let Some(last_colon_index) = pattern.rfind(':') else {
        return ParsedModelResult::default();
    };
    let prefix = &pattern[..last_colon_index];
    let suffix = &pattern[last_colon_index + 1..];

    if let Some(level) = parse_thinking_level(suffix) {
        // Valid thinking level — recurse on prefix and use this level.
        let result = parse_model_pattern(
            prefix,
            available_models,
            allow_invalid_thinking_level_fallback,
        );
        if result.model.is_some() {
            return ParsedModelResult {
                model: result.model,
                thinking_level: if result.warning.is_some() {
                    None
                } else {
                    Some(level)
                },
                warning: result.warning,
            };
        }
        result
    } else {
        if !allow_invalid_thinking_level_fallback {
            // Strict mode (CLI --model parsing): treat the suffix as part of
            // the model id and fail.
            return ParsedModelResult::default();
        }
        // Scope mode: recurse on prefix and warn.
        let result = parse_model_pattern(
            prefix,
            available_models,
            allow_invalid_thinking_level_fallback,
        );
        if result.model.is_some() {
            return ParsedModelResult {
                model: result.model,
                thinking_level: None,
                warning: Some(format!(
                    "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
                )),
            };
        }
        result
    }
}

// ============================================================================
// minimatch subset (model-resolver.ts:307-310)
// ============================================================================

/// `minimatch(value, pattern, { nocase: true })` subset: `*` (within a path
/// segment), `**` (across segments), `?`, and `[...]` character classes with
/// ranges and `!`/`^` negation. Brace expansion and extglobs are not needed
/// by the model-scope patterns and are not supported.
pub fn minimatch_nocase(value: &str, pattern: &str) -> bool {
    let value: Vec<char> = value.to_lowercase().chars().collect();
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    glob_match(&value, &normalize_globstars(&pattern))
}

/// node minimatch treats `**` as a globstar only when it forms a complete
/// path segment (bounded by `/` or the pattern edges); anywhere else it
/// collapses to a single `*`.
fn normalize_globstars(pattern: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut i = 0;
    while i < pattern.len() {
        if pattern[i] == '*' && i + 1 < pattern.len() && pattern[i + 1] == '*' {
            let segment_start = i == 0 || pattern[i - 1] == '/';
            let segment_end = i + 2 == pattern.len() || pattern[i + 2] == '/';
            if segment_start && segment_end {
                out.push('*');
                out.push('*');
            } else {
                out.push('*');
            }
            i += 2;
            continue;
        }
        out.push(pattern[i]);
        i += 1;
    }
    out
}

fn glob_match(value: &[char], pattern: &[char]) -> bool {
    // Backtracking matcher: `**` crosses `/`, `*` does not.
    if pattern.is_empty() {
        return value.is_empty();
    }
    if pattern[0] == '*' {
        let crosses_slash = pattern.len() > 1 && pattern[1] == '*';
        // A globstar may match zero path segments: `a/**/b` also matches
        // `a/b` (the trailing `/` is absorbed).
        if crosses_slash
            && pattern.len() > 2
            && pattern[2] == '/'
            && glob_match(value, &pattern[3..])
        {
            return true;
        }
        let rest = if crosses_slash {
            &pattern[2..]
        } else {
            &pattern[1..]
        };
        for count in 0..=value.len() {
            if glob_match(&value[count..], rest) {
                return true;
            }
            if !crosses_slash && count < value.len() && value[count] == '/' {
                return false;
            }
        }
        return false;
    }
    if value.is_empty() {
        return false;
    }
    match pattern[0] {
        // `?` never matches the path separator.
        '?' => value[0] != '/' && glob_match(&value[1..], &pattern[1..]),
        '[' => match match_class(pattern) {
            Some(((ranges, negate), rest)) => {
                let c = value[0];
                let inside = ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi);
                // Character classes never match the path separator, negated
                // or not.
                c != '/' && (inside != negate) && glob_match(&value[1..], rest)
            }
            None => value[0] == '[' && glob_match(&value[1..], &pattern[1..]),
        },
        literal => value[0] == literal && glob_match(&value[1..], &pattern[1..]),
    }
}

/// A parsed `[...]` class: ranges plus negation flag.
type CharClass = (Vec<(char, char)>, bool);

/// Parse a `[...]` class at the head of `pattern`. Returns the ranges plus
/// negation flag and the pattern remainder, or `None` when the class is
/// unterminated (treated as a literal `[`, like minimatch).
fn match_class(pattern: &[char]) -> Option<(CharClass, &[char])> {
    debug_assert_eq!(pattern[0], '[');
    let mut i = 1;
    let negate = i < pattern.len() && (pattern[i] == '!' || pattern[i] == '^');
    if negate {
        i += 1;
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    let mut first = true;
    while i < pattern.len() && (pattern[i] != ']' || first) {
        first = false;
        let lo = pattern[i];
        if i + 2 < pattern.len() && pattern[i + 1] == '-' && pattern[i + 2] != ']' {
            ranges.push((lo, pattern[i + 2]));
            i += 3;
        } else {
            ranges.push((lo, lo));
            i += 1;
        }
    }
    if i >= pattern.len() {
        return None; // unterminated
    }
    let rest = &pattern[i + 1..];
    Some(((ranges, negate), rest))
}

// ============================================================================
// Model scope (--models)
// ============================================================================

/// `ModelScopeDiagnostic` (model-resolver.ts:261-266).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelScopeDiagnostic {
    /// `"no-match" | "invalid-thinking-level"`.
    pub code: &'static str,
    pub message: String,
    pub pattern: String,
}

/// `ResolveModelScopeResult` (model-resolver.ts:268-271).
#[derive(Debug, Default)]
pub struct ResolveModelScopeResult {
    pub scoped_models: Vec<ScopedModel>,
    pub diagnostics: Vec<ModelScopeDiagnostic>,
}

/// `resolveModelScopeWithDiagnostics` (model-resolver.ts:273-353).
pub async fn resolve_model_scope_with_diagnostics(
    patterns: &[String],
    model_runtime: &ModelRuntime,
) -> ResolveModelScopeResult {
    let available_models = model_runtime.get_available(None).await.unwrap_or_default();
    let mut scoped_models: Vec<ScopedModel> = Vec::new();
    let mut diagnostics: Vec<ModelScopeDiagnostic> = Vec::new();

    for pattern in patterns {
        // Check if pattern contains glob characters.
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            // Extract optional thinking level suffix (e.g., "provider/*:high").
            let mut glob_pattern = pattern.as_str();
            let mut thinking_level: Option<ThinkingLevel> = None;
            if let Some(colon_idx) = pattern.rfind(':') {
                let suffix = &pattern[colon_idx + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    thinking_level = Some(level);
                    glob_pattern = &pattern[..colon_idx];
                }
            }

            if let Some(exact) = find_exact_model_reference_match(glob_pattern, &available_models) {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(Some(&sm.model), Some(&exact)))
                {
                    scoped_models.push(ScopedModel {
                        model: exact,
                        thinking_level,
                    });
                }
                continue;
            }

            // Match against "provider/modelId" format OR just model ID.
            let matching: Vec<Model> = available_models
                .iter()
                .filter(|m| {
                    let full_id = format!("{}/{}", m.provider, m.id);
                    minimatch_nocase(&full_id, glob_pattern)
                        || minimatch_nocase(&m.id, glob_pattern)
                })
                .cloned()
                .collect();

            if matching.is_empty() {
                diagnostics.push(ModelScopeDiagnostic {
                    code: "no-match",
                    message: format!("No models match pattern \"{pattern}\""),
                    pattern: pattern.clone(),
                });
                continue;
            }
            for model in matching {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(Some(&sm.model), Some(&model)))
                {
                    scoped_models.push(ScopedModel {
                        model,
                        thinking_level,
                    });
                }
            }
            continue;
        }

        let ParsedModelResult {
            model,
            thinking_level,
            warning,
        } = parse_model_pattern(pattern, &available_models, true);

        if let Some(warning) = warning {
            diagnostics.push(ModelScopeDiagnostic {
                code: "invalid-thinking-level",
                message: warning,
                pattern: pattern.clone(),
            });
        }

        let Some(model) = model else {
            diagnostics.push(ModelScopeDiagnostic {
                code: "no-match",
                message: format!("No models match pattern \"{pattern}\""),
                pattern: pattern.clone(),
            });
            continue;
        };

        if !scoped_models
            .iter()
            .any(|sm| models_are_equal(Some(&sm.model), Some(&model)))
        {
            scoped_models.push(ScopedModel {
                model,
                thinking_level,
            });
        }
    }

    ResolveModelScopeResult {
        scoped_models,
        diagnostics,
    }
}

// ============================================================================
// CLI model resolution
// ============================================================================

/// `ResolveCliModelResult` (model-resolver.ts:363-372).
#[derive(Debug, Default)]
pub struct ResolveCliModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
    /// Error message suitable for CLI display. When set, model is `None`.
    pub error: Option<String>,
}

pub struct ResolveCliModelOptions<'a> {
    pub cli_provider: Option<&'a str>,
    pub cli_model: Option<&'a str>,
    pub cli_thinking: Option<ThinkingLevel>,
    pub model_runtime: &'a ModelRuntime,
}

/// `resolveCliModel` (model-resolver.ts:385-556).
pub fn resolve_cli_model(options: ResolveCliModelOptions) -> ResolveCliModelResult {
    let ResolveCliModelOptions {
        cli_provider,
        cli_model,
        cli_thinking,
        model_runtime,
    } = options;

    let Some(cli_model) = cli_model else {
        return ResolveCliModelResult::default();
    };

    // Important: use *all* models here, not just models with pre-configured
    // auth. This allows "--api-key" to be used for first-time setup.
    let available_models = model_runtime.get_models(None);
    if available_models.is_empty() {
        return ResolveCliModelResult {
            error: Some(
                "No models available. Check your installation or add models to models.json."
                    .to_owned(),
            ),
            ..Default::default()
        };
    }

    // Build canonical provider lookup (case-insensitive).
    let mut provider_map: Vec<(String, String)> = Vec::new();
    for m in &available_models {
        let lower = m.provider.to_lowercase();
        if !provider_map.iter().any(|(l, _)| *l == lower) {
            provider_map.push((lower, m.provider.clone()));
        }
    }
    let lookup = |name: &str| -> Option<String> {
        let lower = name.to_lowercase();
        provider_map
            .iter()
            .find(|(l, _)| *l == lower)
            .map(|(_, canonical)| canonical.clone())
    };

    let mut provider = cli_provider.and_then(lookup);
    if cli_provider.is_some() && provider.is_none() {
        return ResolveCliModelResult {
            error: Some(format!(
                "Unknown provider \"{}\". Use --list-models to see available providers/models.",
                cli_provider.unwrap_or_default()
            )),
            ..Default::default()
        };
    }

    // If no explicit --provider, try to interpret "provider/model" format
    // first (model-resolver.ts:423-442).
    let mut pattern = cli_model.to_owned();
    let mut inferred_provider = false;

    if provider.is_none() {
        if let Some(slash_index) = cli_model.find('/') {
            let maybe_provider = &cli_model[..slash_index];
            if let Some(canonical) = lookup(maybe_provider) {
                provider = Some(canonical);
                pattern = cli_model[slash_index + 1..].to_owned();
                inferred_provider = true;
            }
        }
    }

    // No provider inferred: try exact matches without provider inference
    // (handles ids that naturally contain slashes).
    if provider.is_none() {
        let lower = cli_model.to_lowercase();
        if let Some(exact) = available_models.iter().find(|m| {
            m.id.to_lowercase() == lower
                || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
        }) {
            return ResolveCliModelResult {
                model: Some(exact.clone()),
                ..Default::default()
            };
        }
    }

    if cli_provider.is_some() && provider.is_some() {
        // Tolerate --model <provider>/<pattern> by stripping the prefix.
        let provider_name = provider.clone().unwrap_or_default();
        let prefix = format!("{provider_name}/");
        if cli_model.to_lowercase().starts_with(&prefix.to_lowercase()) {
            pattern = cli_model[prefix.len()..].to_owned();
        }
    }

    let candidates: Vec<Model> = match &provider {
        Some(provider) => available_models
            .iter()
            .filter(|m| m.provider == *provider)
            .cloned()
            .collect(),
        None => available_models.clone(),
    };
    let ParsedModelResult {
        model,
        thinking_level,
        warning,
    } = parse_model_pattern(&pattern, &candidates, false);

    if let Some(model) = model {
        // Prefer an authenticated exact raw-id match over an unauthenticated
        // provider-inferred match (model-resolver.ts:469-491).
        if inferred_provider {
            let raw_exact_matches: Vec<&Model> = available_models
                .iter()
                .filter(|m| {
                    m.id.to_lowercase() == cli_model.to_lowercase()
                        && !models_are_equal(Some(m), Some(&model))
                })
                .collect();
            if !raw_exact_matches.is_empty() && !model_runtime.has_configured_auth(&model.provider)
            {
                let authenticated: Vec<&&Model> = raw_exact_matches
                    .iter()
                    .filter(|m| model_runtime.has_configured_auth(&m.provider))
                    .collect();
                if authenticated.len() == 1 {
                    return ResolveCliModelResult {
                        model: Some((*authenticated[0]).clone()),
                        ..Default::default()
                    };
                }
            }
        }
        return ResolveCliModelResult {
            model: Some(model),
            thinking_level,
            warning,
            error: None,
        };
    }

    // Provider inferred but no match within it: fall back to the full input
    // as a raw model id across all models (OpenRouter-style ids).
    if inferred_provider {
        let lower = cli_model.to_lowercase();
        if let Some(exact) = available_models.iter().find(|m| {
            m.id.to_lowercase() == lower
                || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
        }) {
            return ResolveCliModelResult {
                model: Some(exact.clone()),
                ..Default::default()
            };
        }
        let fallback = parse_model_pattern(cli_model, &available_models, false);
        if fallback.model.is_some() {
            return ResolveCliModelResult {
                model: fallback.model,
                thinking_level: fallback.thinking_level,
                warning: fallback.warning,
                error: None,
            };
        }
    }

    if let Some(provider) = &provider {
        // Parse the thinking level suffix before building the fallback model,
        // but only when --thinking is not explicitly provided.
        let mut fallback_pattern = pattern.as_str();
        let mut fallback_thinking: Option<ThinkingLevel> = None;
        if cli_thinking.is_none() {
            if let Some(last_colon) = pattern.rfind(':') {
                let suffix = &pattern[last_colon + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    fallback_pattern = &pattern[..last_colon];
                    fallback_thinking = Some(level);
                }
            }
        }

        if let Some(fallback_model) =
            build_fallback_model(provider, fallback_pattern, &available_models)
        {
            let requested_thinking = cli_thinking.or(fallback_thinking);
            let mut model = fallback_model;
            if requested_thinking.is_some() && requested_thinking != Some(ThinkingLevel::Off) {
                model.reasoning = true;
            }
            let fallback_warning = match warning {
                Some(warning) => format!(
                    "{warning} Model \"{fallback_pattern}\" not found for provider \"{provider}\". Using custom model id."
                ),
                None => format!(
                    "Model \"{fallback_pattern}\" not found for provider \"{provider}\". Using custom model id."
                ),
            };
            return ResolveCliModelResult {
                model: Some(model),
                thinking_level: fallback_thinking,
                warning: Some(fallback_warning),
                error: None,
            };
        }
    }

    let display = match &provider {
        Some(provider) => format!("{provider}/{pattern}"),
        None => cli_model.to_owned(),
    };
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning,
        error: Some(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        )),
    }
}

// ============================================================================
// Initial model selection
// ============================================================================

/// `InitialModelResult` (model-resolver.ts:558-562).
#[derive(Debug)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub fallback_message: Option<String>,
}

pub struct FindInitialModelOptions<'a> {
    pub cli_provider: Option<&'a str>,
    pub cli_model: Option<&'a str>,
    pub scoped_models: &'a [ScopedModel],
    pub is_continuing: bool,
    pub default_provider: Option<&'a str>,
    pub default_model_id: Option<&'a str>,
    pub default_thinking_level: Option<ThinkingLevel>,
    pub model_runtime: &'a ModelRuntime,
}

/// `findInitialModel` (model-resolver.ts:572-652). The CLI-resolution error
/// path (`process.exit(1)` upstream) returns `Err(display_message)`.
pub async fn find_initial_model(
    options: FindInitialModelOptions<'_>,
) -> Result<InitialModelResult, String> {
    let FindInitialModelOptions {
        cli_provider,
        cli_model,
        scoped_models,
        is_continuing,
        default_provider,
        default_model_id,
        default_thinking_level,
        model_runtime,
    } = options;

    // 1. CLI args take priority.
    if cli_provider.is_some() && cli_model.is_some() {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider,
            cli_model,
            cli_thinking: None,
            model_runtime,
        });
        if let Some(error) = resolved.error {
            return Err(error);
        }
        if let Some(model) = resolved.model {
            return Ok(InitialModelResult {
                model: Some(model),
                thinking_level: DEFAULT_THINKING_LEVEL,
                fallback_message: None,
            });
        }
    }

    // 2. Use first model from scoped models (skip if continuing/resuming).
    if !scoped_models.is_empty() && !is_continuing {
        let first = &scoped_models[0];
        return Ok(InitialModelResult {
            model: Some(first.model.clone()),
            thinking_level: first
                .thinking_level
                .or(default_thinking_level)
                .unwrap_or(DEFAULT_THINKING_LEVEL),
            fallback_message: None,
        });
    }

    // 3. Try saved default from settings if auth is configured.
    if let (Some(default_provider), Some(default_model_id)) = (default_provider, default_model_id) {
        if let Some(found) = model_runtime.get_model(default_provider, default_model_id) {
            if model_runtime.has_configured_auth(&found.provider) {
                return Ok(InitialModelResult {
                    model: Some(found),
                    thinking_level: default_thinking_level.unwrap_or(DEFAULT_THINKING_LEVEL),
                    fallback_message: None,
                });
            }
        }
    }

    // 4. Try first available model with valid API key.
    let available_models = model_runtime.get_available(None).await.unwrap_or_default();
    if !available_models.is_empty() {
        for (provider, default_id) in DEFAULT_MODEL_PER_PROVIDER {
            if let Some(found) = available_models
                .iter()
                .find(|m| m.provider == provider && m.id == default_id)
            {
                return Ok(InitialModelResult {
                    model: Some(found.clone()),
                    thinking_level: DEFAULT_THINKING_LEVEL,
                    fallback_message: None,
                });
            }
        }
        return Ok(InitialModelResult {
            model: Some(available_models[0].clone()),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        });
    }

    // 5. No model found.
    Ok(InitialModelResult {
        model: None,
        thinking_level: DEFAULT_THINKING_LEVEL,
        fallback_message: None,
    })
}

/// `restoreModelFromSession` result; print-side messages are surfaced as
/// data so the caller decides where they go (upstream writes to stdout/stderr
/// directly when `shouldPrintMessages`).
#[derive(Debug)]
pub struct RestoreModelResult {
    pub model: Option<Model>,
    pub fallback_message: Option<String>,
    /// `Restored model: {provider}/{id}` notice (upstream `chalk.dim`).
    pub restored_notice: Option<String>,
}

/// `restoreModelFromSession` (model-resolver.ts:657-726).
pub async fn restore_model_from_session(
    saved_provider: &str,
    saved_model_id: &str,
    current_model: Option<&Model>,
    model_runtime: &ModelRuntime,
) -> RestoreModelResult {
    let restored_model = model_runtime.get_model(saved_provider, saved_model_id);
    let has_configured_auth = restored_model
        .as_ref()
        .map(|m| model_runtime.has_configured_auth(&m.provider))
        .unwrap_or(false);
    let reason = if restored_model.is_none() {
        "model no longer exists"
    } else {
        "no auth configured"
    };

    if let Some(restored) = restored_model.filter(|_| has_configured_auth) {
        return RestoreModelResult {
            model: Some(restored),
            fallback_message: None,
            restored_notice: Some(format!("Restored model: {saved_provider}/{saved_model_id}")),
        };
    }

    if let Some(current) = current_model {
        return RestoreModelResult {
            model: Some(current.clone()),
            fallback_message: Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                current.provider, current.id
            )),
            restored_notice: None,
        };
    }

    let available_models = model_runtime.get_available(None).await.unwrap_or_default();
    if !available_models.is_empty() {
        let mut fallback: Option<&Model> = None;
        for (provider, default_id) in DEFAULT_MODEL_PER_PROVIDER {
            if let Some(found) = available_models
                .iter()
                .find(|m| m.provider == provider && m.id == default_id)
            {
                fallback = Some(found);
                break;
            }
        }
        let fallback = fallback.unwrap_or(&available_models[0]);
        return RestoreModelResult {
            model: Some(fallback.clone()),
            fallback_message: Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                fallback.provider, fallback.id
            )),
            restored_notice: None,
        };
    }

    RestoreModelResult {
        model: None,
        fallback_message: None,
        restored_notice: None,
    }
}

#[cfg(test)]
mod tests {
    //! Port of the pattern/scope/CLI-resolution test intent from
    //! `packages/coding-agent/test/model-resolver.test.ts` (fixture models
    //! replicated locally; the upstream catalog fixtures are T13).

    use std::sync::Arc;

    use pir_ai::auth::credential_store::InMemoryCredentialStore;
    use pir_ai::auth::helpers::env_api_key_auth;
    use pir_ai::auth::types::ProviderAuth;
    use pir_ai::models::{create_provider, CreateProviderOptions, ProviderApi};
    use pir_ai::types::{ApiKind, InputModality, Model};

    use super::*;
    use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};

    fn model(provider: &str, id: &str, name: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: ApiKind::from("openai-completions"),
            provider: provider.to_owned(),
            base_url: "https://example.com".to_owned(),
            reasoning,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: Default::default(),
            context_window: 200000,
            max_tokens: 8192,
            headers: None,
            compat: None,
        }
    }

    fn mock_models() -> Vec<Model> {
        vec![
            model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5", true),
            model("openai", "gpt-4o", "GPT-4o", false),
        ]
    }

    fn mock_openrouter_models() -> Vec<Model> {
        vec![
            model(
                "openrouter",
                "qwen/qwen3-coder:exacto",
                "Qwen3 Coder Exacto",
                true,
            ),
            model(
                "openrouter",
                "openai/gpt-4o:extended",
                "GPT-4o Extended",
                false,
            ),
        ]
    }

    fn all_models() -> Vec<Model> {
        let mut models = mock_models();
        models.extend(mock_openrouter_models());
        models
    }

    // ------------------------------------------------------------------
    // parseModelPattern
    // ------------------------------------------------------------------

    #[test]
    fn test_exact_match_returns_model_with_undefined_thinking_level() {
        let result = parse_model_pattern("claude-sonnet-4-5", &mock_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(result.thinking_level, None);
        assert_eq!(result.warning, None);
    }

    #[test]
    fn test_partial_match_returns_best_model_with_undefined_thinking_level() {
        let result = parse_model_pattern("sonnet", &mock_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn test_no_match_returns_undefined_model_and_thinking_level() {
        let result = parse_model_pattern("nonexistent", &mock_models(), true);
        assert!(result.model.is_none());
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn test_sonnet_high_returns_sonnet_with_high_thinking_level() {
        let result = parse_model_pattern("sonnet:high", &mock_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
    }

    #[test]
    fn test_gpt_4o_medium_returns_gpt_4o_with_medium_thinking_level() {
        let result = parse_model_pattern("gpt-4o:medium", &mock_models(), true);
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("gpt-4o"));
        assert_eq!(result.thinking_level, Some(ThinkingLevel::Medium));
    }

    #[test]
    fn test_all_valid_thinking_levels_work() {
        for (suffix, level) in [
            ("off", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::Xhigh),
            ("max", ThinkingLevel::Max),
        ] {
            let result = parse_model_pattern(&format!("sonnet:{suffix}"), &mock_models(), true);
            assert_eq!(result.thinking_level, Some(level), "suffix {suffix}");
        }
    }

    #[test]
    fn test_sonnet_random_returns_sonnet_with_undefined_thinking_level_and_warning() {
        let result = parse_model_pattern("sonnet:random", &mock_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(result.thinking_level, None);
        assert_eq!(
            result.warning.as_deref(),
            Some("Invalid thinking level \"random\" in pattern \"sonnet:random\". Using default instead.")
        );
    }

    #[test]
    fn test_gpt_4o_invalid_returns_gpt_4o_with_undefined_thinking_level_and_warning() {
        let result = parse_model_pattern("gpt-4o:invalid", &mock_models(), true);
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("gpt-4o"));
        assert_eq!(result.thinking_level, None);
        assert!(result.warning.is_some());
    }

    #[test]
    fn test_qwen3_coder_exacto_matches_the_model_with_undefined_thinking_level() {
        let result = parse_model_pattern("qwen3-coder:exacto", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn test_openrouter_qwen_qwen3_coder_exacto_matches_with_provider_prefix() {
        let result = parse_model_pattern("openrouter/qwen/qwen3-coder:exacto", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn test_qwen3_coder_exacto_high_matches_model_with_high_thinking_level() {
        let result = parse_model_pattern("qwen3-coder:exacto:high", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
    }

    #[test]
    fn test_openrouter_qwen_qwen3_coder_exacto_high_matches_with_provider_and_thinking_level() {
        let result = parse_model_pattern(
            "openrouter/qwen/qwen3-coder:exacto:high",
            &all_models(),
            true,
        );
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
    }

    #[test]
    fn test_gpt_4o_extended_matches_the_extended_model_with_undefined_thinking_level() {
        let result = parse_model_pattern("gpt-4o:extended", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("openai/gpt-4o:extended")
        );
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn test_qwen3_coder_exacto_random_returns_model_with_undefined_thinking_level_and_warning() {
        let result = parse_model_pattern("qwen3-coder:exacto:random", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, None);
        assert!(result.warning.is_some());
    }

    #[test]
    fn test_qwen3_coder_exacto_high_random_returns_model_with_warning() {
        let result = parse_model_pattern("qwen3-coder:exacto:high:random", &all_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("qwen/qwen3-coder:exacto")
        );
        assert_eq!(result.thinking_level, None);
        assert!(result.warning.is_some());
    }

    #[test]
    fn test_empty_pattern_matches_via_partial_matching() {
        // `"".toLowerCase().includes` is always true upstream — partial match
        // against every model, best alias wins.
        let result = parse_model_pattern("", &mock_models(), true);
        assert!(result.model.is_some());
    }

    #[test]
    fn test_pattern_ending_with_colon_treats_empty_suffix_as_invalid() {
        let result = parse_model_pattern("sonnet:", &mock_models(), true);
        assert_eq!(
            result.model.map(|m| m.id).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(result.thinking_level, None);
        assert!(result.warning.is_some());
    }

    #[test]
    fn test_strict_mode_rejects_invalid_thinking_suffix() {
        // CLI --model parsing: invalid suffix is treated as part of the id.
        let result = parse_model_pattern("sonnet:random", &mock_models(), false);
        assert!(result.model.is_none());
    }

    // ------------------------------------------------------------------
    // minimatch subset
    // ------------------------------------------------------------------

    #[test]
    fn test_minimatch_star_does_not_cross_slash() {
        assert!(minimatch_nocase("anthropic/claude-x", "anthropic/*"));
        assert!(!minimatch_nocase("openai/gpt-4o", "anthropic/*"));
        assert!(!minimatch_nocase("a/b/c", "a/*"));
        assert!(minimatch_nocase("a/b/c", "a/**"));
    }

    #[test]
    fn test_minimatch_substring_patterns_and_classes() {
        assert!(minimatch_nocase("claude-sonnet-4-5", "*sonnet*"));
        assert!(minimatch_nocase("CLAUDE-Sonnet-4-5", "*SONNET*"));
        assert!(minimatch_nocase("gpt-4o", "gpt-?o"));
        assert!(minimatch_nocase("gpt-4o", "gpt-[0-9]o"));
        assert!(!minimatch_nocase("gpt-xo", "gpt-[0-9]o"));
        assert!(minimatch_nocase("gpt-xo", "gpt-[!0-9]o"));
    }

    #[test]
    fn test_minimatch_path_separator_semantics() {
        // `?` / `[...]` never match `/` (node minimatch).
        assert!(!minimatch_nocase("a/b", "a?b"));
        assert!(!minimatch_nocase("a/b", "a[/]b"));
        assert!(!minimatch_nocase("a/b", "a[!x]b"));
        // `**` is a globstar only as a complete path segment; anywhere else
        // it collapses to `*` and cannot cross `/`.
        assert!(!minimatch_nocase("foo/bar", "foo**bar"));
        assert!(!minimatch_nocase("a/x/b", "a**b"));
        assert!(minimatch_nocase("axxb", "a**b"));
        // Globstar matches zero path segments: `a/**/b` ≡ `a/b` as well.
        assert!(minimatch_nocase("a/b", "a/**/b"));
        assert!(minimatch_nocase("a/x/b", "a/**/b"));
        assert!(minimatch_nocase("a/x/y/b", "a/**/b"));
    }

    // ------------------------------------------------------------------
    // Runtime-backed resolution (scope / CLI / initial)
    // ------------------------------------------------------------------

    const TEST_ENV_KEY: &str = "PIR_TEST_MODEL_RESOLVER_KEY";

    async fn runtime_with_models(models: Vec<Model>) -> Arc<ModelRuntime> {
        std::env::set_var(TEST_ENV_KEY, "test-key");
        let credentials: Arc<dyn pir_ai::auth::types::CredentialStore> =
            Arc::new(InMemoryCredentialStore::new());
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(credentials),
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        runtime
            .register_native_provider(create_provider(CreateProviderOptions {
                id: "test-provider".to_owned(),
                name: None,
                base_url: None,
                headers: None,
                auth: ProviderAuth {
                    api_key: Some(Arc::new(env_api_key_auth("Test API key", &[TEST_ENV_KEY]))),
                    oauth: None,
                },
                models,
                api: ProviderApi::Single(Arc::new(
                    pir_ai::api::openai_completions::OpenAiCompletions,
                )),
            }))
            .await
            .expect("register provider");
        runtime
    }

    fn test_model(id: &str) -> Model {
        model("test-provider", id, id, false)
    }

    #[tokio::test]
    async fn test_resolve_model_scope_glob_and_diagnostics() {
        let runtime = runtime_with_models(vec![
            test_model("alpha-1"),
            test_model("alpha-2"),
            test_model("beta-1"),
        ])
        .await;
        let result = resolve_model_scope_with_diagnostics(
            &[
                "alpha-*".to_owned(),
                "gamma-*".to_owned(),
                "beta-1:bogus".to_owned(),
            ],
            &runtime,
        )
        .await;
        let ids: Vec<&str> = result
            .scoped_models
            .iter()
            .map(|sm| sm.model.id.as_str())
            .collect();
        assert_eq!(ids, vec!["alpha-1", "alpha-2", "beta-1"]);
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(result.diagnostics[0].code, "no-match");
        assert_eq!(
            result.diagnostics[0].message,
            "No models match pattern \"gamma-*\""
        );
        assert_eq!(result.diagnostics[1].code, "invalid-thinking-level");
    }

    #[tokio::test]
    async fn test_resolve_model_scope_glob_thinking_suffix() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("alpha-2")]).await;
        let result =
            resolve_model_scope_with_diagnostics(&["test-provider/*:high".to_owned()], &runtime)
                .await;
        assert_eq!(result.scoped_models.len(), 2);
        assert!(result
            .scoped_models
            .iter()
            .all(|sm| sm.thinking_level == Some(ThinkingLevel::High)));
    }

    #[tokio::test]
    async fn test_resolve_cli_model_provider_id_without_provider_flag() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: None,
            cli_model: Some("test-provider/alpha-1"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("alpha-1"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_resolve_cli_model_fuzzy_within_explicit_provider() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("beta-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some("test-provider"),
            cli_model: Some("alph"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("alpha-1"));
    }

    #[tokio::test]
    async fn test_resolve_cli_model_supports_pattern_thinking_shorthand() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: None,
            cli_model: Some("alpha-1:high"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("alpha-1"));
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
    }

    #[tokio::test]
    async fn test_resolve_cli_model_invalid_suffix_treated_as_raw_id() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: None,
            cli_model: Some("alpha-1:random"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        assert!(result.model.is_none());
        assert_eq!(
            result.error.as_deref(),
            Some("Model \"alpha-1:random\" not found. Use --list-models to see available models.")
        );
    }

    #[tokio::test]
    async fn test_resolve_cli_model_unknown_provider_is_error() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some("nope"),
            cli_model: Some("alpha-1"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        assert!(result.model.is_none());
        assert_eq!(
            result.error.as_deref(),
            Some("Unknown provider \"nope\". Use --list-models to see available providers/models.")
        );
    }

    #[tokio::test]
    async fn test_resolve_cli_model_custom_model_id_fallback_with_warning() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some("test-provider"),
            cli_model: Some("custom-model:high"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        let model = result.model.expect("fallback model");
        assert_eq!(model.id, "custom-model");
        assert!(model.reasoning);
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
        assert_eq!(
            result.warning.as_deref(),
            Some("Model \"custom-model\" not found for provider \"test-provider\". Using custom model id.")
        );
    }

    #[tokio::test]
    async fn test_resolve_cli_model_explicit_thinking_keeps_suffix_in_id() {
        let runtime = runtime_with_models(vec![test_model("alpha-1")]).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some("test-provider"),
            cli_model: Some("custom-model:high"),
            cli_thinking: Some(ThinkingLevel::Low),
            model_runtime: &runtime,
        });
        let model = result.model.expect("fallback model");
        assert_eq!(model.id, "custom-model:high");
        assert_eq!(result.thinking_level, None);
    }

    #[tokio::test]
    async fn test_resolve_cli_model_no_models_is_error() {
        let runtime = runtime_with_models(Vec::new()).await;
        let result = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: None,
            cli_model: Some("anything"),
            cli_thinking: None,
            model_runtime: &runtime,
        });
        // The built-in catalog is always seeded (model-runtime.ts:181-190),
        // so the unmatched-model branch applies (model-resolver.ts:603);
        // the "No models available" branch is unreachable with built-ins.
        assert_eq!(
            result.error.as_deref(),
            Some("Model \"anything\" not found. Use --list-models to see available models.")
        );
    }

    #[tokio::test]
    async fn test_find_initial_model_cli_priority() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("beta-1")]).await;
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: Some("test-provider"),
            cli_model: Some("beta-1"),
            scoped_models: &[],
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await
        .expect("no error");
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("beta-1"));
        assert_eq!(result.thinking_level, DEFAULT_THINKING_LEVEL);
    }

    #[tokio::test]
    async fn test_find_initial_model_scoped_first_when_not_continuing() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("beta-1")]).await;
        let scoped = vec![ScopedModel {
            model: test_model("beta-1"),
            thinking_level: Some(ThinkingLevel::High),
        }];
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &scoped,
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await
        .expect("no error");
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("beta-1"));
        assert_eq!(result.thinking_level, ThinkingLevel::High);
    }

    #[tokio::test]
    async fn test_find_initial_model_saved_default_and_first_available() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("beta-1")]).await;
        // Saved default wins.
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &[],
            is_continuing: true,
            default_provider: Some("test-provider"),
            default_model_id: Some("beta-1"),
            default_thinking_level: Some(ThinkingLevel::Low),
            model_runtime: &runtime,
        })
        .await
        .expect("no error");
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("beta-1"));
        assert_eq!(result.thinking_level, ThinkingLevel::Low);

        // Unknown saved default → first available.
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &[],
            is_continuing: true,
            default_provider: Some("test-provider"),
            default_model_id: Some("nonexistent"),
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await
        .expect("no error");
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("alpha-1"));
    }

    #[tokio::test]
    async fn test_find_initial_model_ignores_unauthenticated_saved_default() {
        // A dedicated never-set env var keeps this test race-free against the
        // parallel tests that set TEST_ENV_KEY.
        const UNSET_ENV_KEY: &str = "PIR_TEST_MODEL_RESOLVER_KEY_UNSET";
        std::env::remove_var(UNSET_ENV_KEY);
        let credentials: Arc<dyn pir_ai::auth::types::CredentialStore> =
            Arc::new(InMemoryCredentialStore::new());
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(credentials),
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        runtime
            .register_native_provider(create_provider(CreateProviderOptions {
                id: "test-provider".to_owned(),
                name: None,
                base_url: None,
                headers: None,
                auth: ProviderAuth {
                    api_key: Some(Arc::new(env_api_key_auth("Test API key", &[UNSET_ENV_KEY]))),
                    oauth: None,
                },
                models: vec![test_model("alpha-1")],
                api: ProviderApi::Single(Arc::new(
                    pir_ai::api::openai_completions::OpenAiCompletions,
                )),
            }))
            .await
            .expect("register provider");
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &[],
            is_continuing: false,
            default_provider: Some("test-provider"),
            default_model_id: Some("alpha-1"),
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await
        .expect("no error");
        assert!(result.model.is_none());
    }

    #[tokio::test]
    async fn test_restore_model_from_session_paths() {
        let runtime = runtime_with_models(vec![test_model("alpha-1"), test_model("beta-1")]).await;

        // Restored directly.
        let result = restore_model_from_session("test-provider", "beta-1", None, &runtime).await;
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("beta-1"));
        assert_eq!(
            result.restored_notice.as_deref(),
            Some("Restored model: test-provider/beta-1")
        );

        // Unknown saved model → current model fallback with message.
        let current = test_model("alpha-1");
        let result =
            restore_model_from_session("test-provider", "gone", Some(&current), &runtime).await;
        assert_eq!(result.model.map(|m| m.id).as_deref(), Some("alpha-1"));
        assert_eq!(
            result.fallback_message.as_deref(),
            Some(
                "Could not restore model test-provider/gone (model no longer exists). Using test-provider/alpha-1."
            )
        );
    }
}
