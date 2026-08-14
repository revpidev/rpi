//! Model / thinking override resolution, fuzzy matching, fallback candidate
//! chains and model-scope enforcement (FR-P1-05).
//!
//! Port of pi-subagents `src/runs/shared/model-fallback.ts` and
//! `src/runs/shared/model-scope.ts` @ v0.48.0 (56f97234). All functions here
//! are pure (no filesystem, no host calls) so the fuzzy/scope behaviors stay
//! unit-testable and parity-checkable against the upstream sources.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Sentinel model value requesting that a subagent inherit the parent
/// session's model (`INHERIT_MODEL`, model-fallback.ts:35).
pub const INHERIT_MODEL: &str = "inherit";

/// One registry entry for fuzzy resolution (`AvailableModelInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableModel {
    /// Full `provider/id` id (`fullId`).
    pub full_id: String,
    pub provider: String,
    pub id: String,
}

/// `splitThinkingSuffix` (model-fallback.ts:15-21): split at the *last* colon.
pub fn split_thinking_suffix(model: &str) -> (&str, &str) {
    match model.rfind(':') {
        None => (model, ""),
        Some(index) => (&model[..index], &model[index..]),
    }
}

/// `normalizeModelSegment` (model-fallback.ts:46): case-fold, dots/underscores
/// → dashes (so `4.5` matches `4-5`), collapse repeats, trim edges.
pub fn normalize_model_segment(segment: &str) -> String {
    let lower = segment.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_dash = false;
    for ch in lower.chars() {
        if ch == '.' || ch == '_' || ch == '-' {
            if !last_dash {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(ch);
            last_dash = false;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.to_string()
}

/// `isPlausibleDateStamp` (model-fallback.ts:56-60).
fn is_plausible_date_stamp(year: &str, month: &str, day: &str) -> bool {
    let (Ok(yyyy), Ok(mm), Ok(dd)) = (
        year.parse::<u32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return false;
    };
    (1900..=2099).contains(&yyyy) && (1..=12).contains(&mm) && (1..=31).contains(&dd)
}

/// `stripTrailingDateStamp` (model-fallback.ts:62): drop `-YYYY-MM-DD` or
/// `-YYYYMMDD` so dated and undated ids match. Operates on an already
/// normalized (dash-separated) segment.
fn strip_trailing_date_stamp(segment: &str) -> String {
    // Dashed full form `prefix-YYYY-MM-DD`
    if let Some(rest) = strip_date_suffix_dashed(segment) {
        return rest;
    }
    // Compact form `prefix-YYYYMMDD`
    if let Some(rest) = strip_date_suffix_compact(segment) {
        return rest;
    }
    segment.to_string()
}

/// Match `^(.*)-(\d{4})-(\d{2})-(\d{2})$` with plausible date parts.
fn strip_date_suffix_dashed(segment: &str) -> Option<String> {
    let first = segment.char_indices().rev().collect::<Vec<_>>();
    let _ = first;
    // Parse from the end: DD-MM-YYYY.
    let bytes = segment.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let n = bytes.len();
    // Last group: DD (2)
    let dd = &segment[n - 2..];
    if !dd.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if segment.as_bytes()[n - 3] != b'-' {
        return None;
    }
    // Middle group: MM (2)
    let mm = &segment[n - 5..n - 3];
    if !mm.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if segment.as_bytes()[n - 6] != b'-' {
        return None;
    }
    // Year group: YYYY (4)
    let yyyy = &segment[n - 10..n - 6];
    if !yyyy.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if segment.as_bytes()[n - 11] != b'-' {
        return None;
    }
    if !is_plausible_date_stamp(yyyy, mm, dd) {
        return None;
    }
    // The dash before the year sits at n-11; the prefix excludes it
    // (`^(.*)-(\d{4})-(\d{2})-(\d{2})$` — group 1 stops before that dash).
    Some(segment[..n - 11].to_string())
}

/// Match `^(.*)-(\d{4})(\d{2})(\d{2})$` (compact date) with plausible parts.
fn strip_date_suffix_compact(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    if bytes.len() < 9 {
        return None;
    }
    let n = bytes.len();
    if segment.as_bytes()[n - 9] != b'-' {
        return None;
    }
    let digits = &segment[n - 8..];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let yyyy = &digits[0..4];
    let mm = &digits[4..6];
    let dd = &digits[6..8];
    if !is_plausible_date_stamp(yyyy, mm, dd) {
        return None;
    }
    Some(segment[..n - 9].to_string())
}

/// `fuzzyResolveModel` (model-fallback.ts:99-145): resolve a base model id
/// (thinking suffix already stripped) against the registry tolerating
/// separator/case/date-stamp differences. A qualified `provider/id` query only
/// matches within the named provider; ambiguous matches resolve to
/// `None` unless `preferred_provider` disambiguates.
pub fn fuzzy_resolve_model(
    base_model: &str,
    available_models: &[AvailableModel],
    preferred_provider: Option<&str>,
) -> Option<String> {
    let mut query_provider: Option<String> = None;
    let mut query_id_raw = base_model;
    if let Some(slash_idx) = base_model.find('/') {
        query_provider = Some(normalize_model_segment(&base_model[..slash_idx]));
        query_id_raw = &base_model[slash_idx + 1..];
    } else {
        // Try `:` / `.` prefixes, but only when the prefix is a known provider.
        for separator in [':', '.'] {
            if let Some(separator_idx) = base_model.find(separator) {
                if separator_idx == 0 {
                    continue;
                }
                let provider_part = normalize_model_segment(&base_model[..separator_idx]);
                let known = available_models
                    .iter()
                    .any(|entry| normalize_model_segment(&entry.provider) == provider_part);
                if !known {
                    continue;
                }
                query_provider = Some(provider_part);
                query_id_raw = &base_model[separator_idx + 1..];
                break;
            }
        }
    }
    let query_id = normalize_model_segment(query_id_raw);
    let query_id_no_date = strip_trailing_date_stamp(&query_id);

    let candidates: Vec<&AvailableModel> = available_models
        .iter()
        .filter(|entry| {
            let entry_id = normalize_model_segment(&entry.id);
            if entry_id != query_id && strip_trailing_date_stamp(&entry_id) != query_id_no_date {
                return false;
            }
            if let Some(provider) = &query_provider {
                if normalize_model_segment(&entry.provider) != *provider {
                    return false;
                }
            }
            true
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if let Some(preferred) = preferred_provider {
        let preferred_norm = normalize_model_segment(preferred);
        if let Some(hit) = candidates
            .iter()
            .find(|entry| normalize_model_segment(&entry.provider) == preferred_norm)
        {
            return Some(hit.full_id.clone());
        }
    }
    if candidates.len() == 1 {
        return Some(candidates[0].full_id.clone());
    }
    None
}

/// `resolveBaseModelCandidate` (model-fallback.ts:70-96): exact match first
/// (qualified wins for `provider/id`; unqualified requires a unique id unless
/// the preferred provider matches), then fuzzy.
pub fn resolve_base_model_candidate(
    base_model: &str,
    available_models: &[AvailableModel],
    preferred_provider: Option<&str>,
) -> Option<String> {
    if base_model.contains('/') {
        if let Some(exact) = available_models
            .iter()
            .find(|entry| entry.full_id == base_model)
        {
            return Some(exact.full_id.clone());
        }
    } else {
        let exact_matches: Vec<&AvailableModel> = available_models
            .iter()
            .filter(|entry| entry.id == base_model)
            .collect();
        if let Some(preferred) = preferred_provider {
            if let Some(hit) = exact_matches
                .iter()
                .find(|entry| entry.provider == preferred)
            {
                return Some(hit.full_id.clone());
            }
        }
        if exact_matches.len() == 1 {
            return Some(exact_matches[0].full_id.clone());
        }
    }
    fuzzy_resolve_model(base_model, available_models, preferred_provider)
}

/// `resolveModelCandidate` (model-fallback.ts:148-163): resolve a possibly
/// loose model id to canonical `provider/id`; exact registry matches win,
/// thinking suffix is retried on the base when the whole id misses.
pub fn resolve_model_candidate(
    model: Option<&str>,
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
) -> Option<String> {
    let model = model?;
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Some(models) = available_models else {
        return Some(trimmed.to_string());
    };
    if models.is_empty() {
        return Some(trimmed.to_string());
    }
    if let Some(resolved) = resolve_base_model_candidate(trimmed, models, preferred_provider) {
        return Some(resolved);
    }
    let (base, suffix) = split_thinking_suffix(trimmed);
    if suffix.is_empty() {
        return Some(trimmed.to_string());
    }
    if let Some(resolved) = resolve_base_model_candidate(base, models, preferred_provider) {
        return Some(format!("{resolved}{suffix}"));
    }
    Some(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// modelScope (model-scope.ts)
// ---------------------------------------------------------------------------

/// `ModelScopeConfig` (model-scope.ts:12-20).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelScopeConfig {
    pub enforce: Option<bool>,
    pub strict: Option<bool>,
    pub allow: Option<Vec<String>>,
}

impl ModelScopeConfig {
    pub fn enforced(&self) -> bool {
        self.enforce == Some(true)
    }
}

/// `globToRegExp` (model-scope.ts:42): escape regex specials except `*`,
/// `*` → `.*`, anchored, case-insensitive.
fn glob_matches(model: &str, pattern: &str) -> bool {
    let mut regex = String::from("(?i)^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            _ => regex.push(ch),
        }
    }
    regex.push('$');
    Regex::new(&regex)
        .map(|re| re.is_match(model))
        .unwrap_or(false)
}

/// `matchesScopePattern` (model-scope.ts:51): case-insensitive full
/// `provider/id` compare with the thinking suffix stripped.
pub fn matches_scope_pattern(model: &str, pattern: &str) -> bool {
    let (base, _) = split_thinking_suffix(model);
    glob_matches(base, pattern)
}

/// Where a resolved model originated, deciding enforcement severity
/// (model-scope.ts:24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Explicit,
    Inherited,
}

/// `ModelScopeViolation` (model-scope.ts:26-34).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelScopeViolation {
    pub model: String,
    /// `warn` | `error` (kept as bool `is_error` for ergonomics).
    pub is_error: bool,
    pub message: String,
    pub allowed_patterns: Vec<String>,
}

/// `checkModelScope` (model-scope.ts:62-82): pure scope decision.
pub fn check_model_scope(
    model: Option<&str>,
    scope: Option<&ModelScopeConfig>,
    source: ModelSource,
) -> Option<ModelScopeViolation> {
    let model = model?;
    let scope = scope?;
    if !scope.enforced() {
        return None;
    }
    let allow = scope.allow.as_ref()?;
    if allow.is_empty() {
        return None;
    }
    if allow
        .iter()
        .any(|pattern| matches_scope_pattern(model, pattern))
    {
        return None;
    }
    let (base, _) = split_thinking_suffix(model);
    let is_error = source == ModelSource::Explicit || scope.strict == Some(true);
    Some(ModelScopeViolation {
        model: base.to_string(),
        is_error,
        message: format!(
            "Model '{base}' is outside the configured subagent model scope. Allowed patterns: {}.",
            allow.join(", ")
        ),
        allowed_patterns: allow.clone(),
    })
}

/// `parseModelScopeConfig` (model-scope.ts:89+): settings-parsing style
/// validation; `Err` on malformed shapes.
pub fn parse_model_scope_config(value: Option<&Value>) -> Result<Option<ModelScopeConfig>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Err("have invalid 'modelScope'; expected an object.".to_string());
    };
    let mut config = ModelScopeConfig::default();
    if let Some(enforce) = object.get("enforce") {
        config.enforce =
            Some(enforce.as_bool().ok_or_else(|| {
                "have invalid 'modelScope.enforce'; expected a boolean.".to_string()
            })?);
    }
    if let Some(strict) = object.get("strict") {
        config.strict =
            Some(strict.as_bool().ok_or_else(|| {
                "have invalid 'modelScope.strict'; expected a boolean.".to_string()
            })?);
    }
    if let Some(allow) = object.get("allow") {
        let Some(items) = allow.as_array() else {
            return Err(
                "have invalid 'modelScope.allow'; expected an array of strings.".to_string(),
            );
        };
        let mut patterns = Vec::new();
        for item in items {
            let pattern = item.as_str().map(str::trim).filter(|s| !s.is_empty());
            let Some(pattern) = pattern else {
                return Err(
                    "have invalid 'modelScope.allow'; expected an array of non-empty strings."
                        .to_string(),
                );
            };
            patterns.push(pattern.to_string());
        }
        config.allow = Some(patterns);
    }
    Ok(Some(config))
}

// ---------------------------------------------------------------------------
// Override resolution (model-fallback.ts:180+)
// ---------------------------------------------------------------------------

/// `resolveSubagentModelOverride` (model-fallback.ts:231-258): resolve the
/// `--model` override for a spawned child. Empty/`inherit` → parent session
/// model. Out-of-scope: `Err` for explicit + strict, warn callback otherwise.
pub fn resolve_subagent_model_override(
    requested_model: Option<&str>,
    parent_model: Option<(&str, &str)>,
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
    scope: Option<&ModelScopeConfig>,
    source: ModelSource,
    on_warn: &mut dyn FnMut(&ModelScopeViolation),
) -> Result<Option<String>, String> {
    let trimmed = requested_model.map(str::trim).unwrap_or("");
    let explicit = if trimmed.is_empty() || trimmed == INHERIT_MODEL {
        None
    } else {
        Some(trimmed)
    };
    let resolved = match explicit {
        None => parent_model.map(|(provider, id)| format!("{provider}/{id}")),
        Some(explicit) => {
            resolve_model_candidate(Some(explicit), available_models, preferred_provider)
        }
    };
    if let Some(resolved) = resolved.as_deref() {
        if scope.is_some_and(|s| s.enforced()) {
            if let Some(violation) = check_model_scope(Some(resolved), scope, source) {
                if violation.is_error {
                    return Err(violation.message);
                }
                on_warn(&violation);
            }
        }
    }
    Ok(resolved)
}

/// `resolveEffectiveSubagentModel` (model-fallback.ts:260-281): explicit →
/// agent → parent, with the explicit attempt falling back to the agent model
/// when it resolves to nothing.
#[allow(clippy::too_many_arguments)]
pub fn resolve_effective_subagent_model(
    explicit_model: Option<&str>,
    agent_model: Option<&str>,
    parent_model: Option<(&str, &str)>,
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
    scope: Option<&ModelScopeConfig>,
    on_warn: &mut dyn FnMut(&ModelScopeViolation),
) -> Result<Option<String>, String> {
    let resolved = resolve_subagent_model_override(
        explicit_model.or(agent_model),
        parent_model,
        available_models,
        preferred_provider,
        scope,
        if explicit_model.is_some() {
            ModelSource::Explicit
        } else {
            ModelSource::Inherited
        },
        on_warn,
    )?;
    if resolved.is_some() || explicit_model.is_none() {
        return Ok(resolved);
    }
    resolve_subagent_model_override(
        agent_model,
        parent_model,
        available_models,
        preferred_provider,
        scope,
        ModelSource::Inherited,
        on_warn,
    )
}

/// `buildModelCandidates` (model-fallback.ts:285-318): primary + fallbacks,
/// deduped, resolved against the registry. Fallback entries and (under strict
/// enforcement) even the primary are scope-checked as inherited models.
pub fn build_model_candidates(
    primary_model: Option<&str>,
    fallback_models: &[String],
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
    scope: Option<&ModelScopeConfig>,
    on_warn: &mut dyn FnMut(&ModelScopeViolation),
) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut candidates: Vec<String> = Vec::new();
    let raw: Vec<Option<&str>> = std::iter::once(primary_model)
        .chain(fallback_models.iter().map(|s: &String| Some(s.as_str())))
        .collect();
    for (index, raw_entry) in raw.into_iter().enumerate() {
        let Some(raw) = raw_entry else {
            continue;
        };
        let Some(normalized) =
            resolve_model_candidate(Some(raw), available_models, preferred_provider)
        else {
            continue;
        };
        if seen.contains(&normalized) {
            continue;
        }
        if (index > 0 || scope.is_some_and(|s| s.strict == Some(true)))
            && scope.is_some_and(|s| s.enforced())
        {
            if let Some(violation) =
                check_model_scope(Some(&normalized), scope, ModelSource::Inherited)
            {
                if violation.is_error {
                    return Err(violation.message);
                }
                on_warn(&violation);
            }
        }
        seen.insert(normalized.clone());
        candidates.push(normalized);
    }
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// Retry classification (model-fallback.ts:320-340)
// ---------------------------------------------------------------------------

/// `RETRYABLE_MODEL_FAILURE_PATTERNS` (model-fallback.ts:282-318) — matched
/// case-insensitively against the child error text.
static RETRYABLE_MODEL_FAILURE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)rate\s*limit",
        r"(?i)too many requests",
        r"(?i)\b429\b",
        r"(?i)quota",
        r"(?i)billing",
        r"(?i)credit",
        r"(?i)auth(?:entication)?",
        r"(?i)unauthori[sz]ed",
        r"(?i)forbidden",
        r"(?i)api key",
        r"(?i)token expired",
        r"(?i)invalid key",
        r"(?i)provider.*unavailable",
        r"(?i)model.*unavailable",
        r"(?i)model.*disabled",
        r"(?i)model.*not found",
        r"(?i)unknown model",
        r"(?i)overloaded",
        r"(?i)service unavailable",
        r"(?i)temporar(?:ily)? unavailable",
        r"(?i)connection refused",
        r"(?i)fetch failed",
        r"(?i)network error",
        r"(?i)socket hang up",
        r"(?i)stream ended without finish_reason",
        r"(?i)upstream",
        r"(?i)timed? out",
        r"(?i)timeout",
        r"(?i)\b502\b",
        r"(?i)\b503\b",
        r"(?i)\b504\b",
        r"(?i)cold.?start",
        r"(?i)empty response",
        r"(?i)no output",
        r"(?i)model.*(?:load|fail|error)",
    ]
    .iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// `TOOL_FAILURE_PREFIX` (model-fallback.ts:327): `<tool> failed (exit N):` /
/// `with exit code N` errors come from a tool inside the child task, not the
/// provider — a model retry cannot fix them.
static TOOL_FAILURE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[\w.:@/-]+ failed (?:(?:\(exit \d+\):)|(?:with exit code \d+))(?:\s|$)")
        .expect("tool failure prefix regex")
});

/// `isRetryableModelFailure` (model-fallback.ts:329-333).
pub fn is_retryable_model_failure(error: Option<&str>) -> bool {
    let Some(error) = error else {
        return false;
    };
    if TOOL_FAILURE_PREFIX.is_match(error.trim()) {
        return false;
    }
    RETRYABLE_MODEL_FAILURE_PATTERNS
        .iter()
        .any(|pattern| pattern.is_match(error))
}

/// `formatModelAttemptNote` (model-fallback.ts:335-340).
pub fn format_model_attempt_note(
    model: &str,
    error: Option<&str>,
    exit_code: Option<i32>,
    next_model: Option<&str>,
) -> String {
    let failure = error
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("exit {}", exit_code.unwrap_or(1)));
    match next_model {
        Some(next) => format!("[fallback] {model} failed: {failure}. Retrying with {next}."),
        None => format!("[fallback] {model} failed: {failure}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<AvailableModel> {
        vec![
            AvailableModel {
                full_id: "anthropic/claude-5".into(),
                provider: "anthropic".into(),
                id: "claude-5".into(),
            },
            AvailableModel {
                full_id: "anthropic/claude-5-2025-10-01".into(),
                provider: "anthropic".into(),
                id: "claude-5-2025-10-01".into(),
            },
            AvailableModel {
                full_id: "openai/gpt-5.5".into(),
                provider: "openai".into(),
                id: "gpt-5.5".into(),
            },
            AvailableModel {
                full_id: "openai/gpt-4o".into(),
                provider: "openai".into(),
                id: "gpt-4o".into(),
            },
            AvailableModel {
                full_id: "google/gemini-3-pro".into(),
                provider: "google".into(),
                id: "gemini-3-pro".into(),
            },
        ]
    }

    #[test]
    fn normalize_segment_folds_separators() {
        assert_eq!(normalize_model_segment("GPT_4.5"), "gpt-4-5");
        assert_eq!(normalize_model_segment("--a--b--"), "a-b");
    }

    #[test]
    fn date_stamp_stripping() {
        assert_eq!(strip_trailing_date_stamp("claude-5-2025-10-01"), "claude-5");
        assert_eq!(strip_trailing_date_stamp("claude-5-20251001"), "claude-5");
        // Implausible dates stay.
        assert_eq!(
            strip_trailing_date_stamp("claude-5-3050-13-45"),
            "claude-5-3050-13-45"
        );
    }

    #[test]
    fn fuzzy_resolves_separator_and_date_variants() {
        let registry = models();
        // A dated query resolves against an undated registry entry.
        let undated_only = vec![AvailableModel {
            full_id: "anthropic/claude-5".into(),
            provider: "anthropic".into(),
            id: "claude-5".into(),
        }];
        assert_eq!(
            fuzzy_resolve_model("claude-5-2025-10-01", &undated_only, None),
            Some("anthropic/claude-5".to_string())
        );
        // Both dated and undated ids registered → ambiguous (upstream
        // returns undefined rather than guessing).
        assert_eq!(fuzzy_resolve_model("claude-5", &registry, None), None);
        // Separator/case differences resolve (`GPT_5_5` ≡ `gpt-5.5`).
        assert_eq!(
            fuzzy_resolve_model("openai/GPT_5_5", &registry, None),
            Some("openai/gpt-5.5".to_string())
        );
        // Qualified query never crosses providers.
        assert_eq!(
            fuzzy_resolve_model("openai/claude-5", &registry, None),
            None
        );
        // Unknown model: no match.
        assert_eq!(fuzzy_resolve_model("nope", &registry, None), None);
    }

    #[test]
    fn provider_prefix_separator_forms() {
        let registry = models();
        // `openai:gpt-4o` and `openai.gpt-4o` resolve when the prefix is a
        // known provider.
        assert_eq!(
            fuzzy_resolve_model("openai:gpt-4o", &registry, None),
            Some("openai/gpt-4o".to_string())
        );
        assert_eq!(
            fuzzy_resolve_model("openai.gpt-4o", &registry, None),
            Some("openai/gpt-4o".to_string())
        );
        // Unknown prefix is not treated as a provider.
        assert_eq!(fuzzy_resolve_model("unknown.x", &registry, None), None);
    }

    #[test]
    fn resolve_model_candidate_keeps_thinking_suffix() {
        let registry = models();
        assert_eq!(
            resolve_model_candidate(Some("claude-5:high"), Some(&registry), None),
            Some("anthropic/claude-5:high".to_string())
        );
        // No registry → verbatim.
        assert_eq!(
            resolve_model_candidate(Some("x/y:low"), None, None),
            Some("x/y:low".to_string())
        );
    }

    #[test]
    fn scope_glob_matching() {
        assert!(matches_scope_pattern("anthropic/claude-5", "anthropic/*"));
        assert!(matches_scope_pattern("Anthropic/Claude-5", "anthropic/*"));
        assert!(matches_scope_pattern(
            "anthropic/claude-5:high",
            "anthropic/*"
        ));
        assert!(!matches_scope_pattern("openai/gpt-4o", "anthropic/*"));
        assert!(matches_scope_pattern("openai/gpt-4o", "*/gpt-*"));
        // Regex specials are escaped: a pattern dot only matches a dot.
        assert!(!matches_scope_pattern("openai/gpt-xo", "*/gpt.o"));
    }

    #[test]
    fn scope_decision_severity_by_source() {
        let scope = ModelScopeConfig {
            enforce: Some(true),
            strict: None,
            allow: Some(vec!["anthropic/*".to_string()]),
        };
        let violation =
            check_model_scope(Some("openai/gpt-4o"), Some(&scope), ModelSource::Explicit).unwrap();
        assert!(violation.is_error);
        let violation =
            check_model_scope(Some("openai/gpt-4o"), Some(&scope), ModelSource::Inherited).unwrap();
        assert!(!violation.is_error);
        // In-scope → None; no allow list → None.
        assert!(
            check_model_scope(Some("anthropic/x"), Some(&scope), ModelSource::Explicit).is_none()
        );
        let empty = ModelScopeConfig {
            enforce: Some(true),
            strict: None,
            allow: Some(vec![]),
        };
        assert!(check_model_scope(Some("openai/x"), Some(&empty), ModelSource::Explicit).is_none());
    }

    #[test]
    fn effective_model_precedence_and_fallback_to_agent_model() {
        let registry = models();
        let parent = ("anthropic".to_string(), "claude-5".to_string());
        let parent_ref: (&str, &str) = (&parent.0, &parent.1);
        let mut warnings = Vec::new();
        let mut sink = |v: &ModelScopeViolation| warnings.push(v.message.clone());
        // explicit wins.
        assert_eq!(
            resolve_effective_subagent_model(
                Some("gpt_4o"),
                Some("claude-5"),
                Some(parent_ref),
                Some(&registry),
                None,
                None,
                &mut sink
            )
            .unwrap(),
            Some("openai/gpt-4o".to_string())
        );
        // neither explicit nor agent → parent.
        assert_eq!(
            resolve_effective_subagent_model(
                None,
                None,
                Some(parent_ref),
                Some(&registry),
                None,
                None,
                &mut sink
            )
            .unwrap(),
            Some("anthropic/claude-5".to_string())
        );
        // inherit sentinel → parent.
        assert_eq!(
            resolve_effective_subagent_model(
                Some("inherit"),
                Some("claude-5"),
                Some(parent_ref),
                Some(&registry),
                None,
                None,
                &mut sink
            )
            .unwrap(),
            Some("anthropic/claude-5".to_string())
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn candidates_dedupe_and_scope_check_fallbacks() {
        let registry = models();
        let scope = ModelScopeConfig {
            enforce: Some(true),
            strict: None,
            allow: Some(vec!["anthropic/*".to_string()]),
        };
        let mut warnings = Vec::new();
        let mut sink = |v: &ModelScopeViolation| {
            warnings.push(v.message.clone());
        };
        let candidates = build_model_candidates(
            Some("claude-5"),
            &[
                "openai/gpt-4o".to_string(),
                "anthropic/claude-5".to_string(),
            ],
            Some(&registry),
            None,
            Some(&scope),
            &mut sink,
        )
        .unwrap();
        // primary + out-of-scope fallback (warned, kept) + duplicate dropped.
        assert_eq!(candidates.len(), 2);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn retryable_failure_classification() {
        assert!(is_retryable_model_failure(Some("rate limit exceeded")));
        assert!(is_retryable_model_failure(Some(
            "Error 429: too many requests"
        )));
        assert!(is_retryable_model_failure(Some(
            "model overloaded, try again"
        )));
        assert!(!is_retryable_model_failure(None));
        // Tool failures inside the child are not model failures — but the
        // upstream prefix only anchors to single-token tool names, so
        // "cargo test failed …" (space in the name) is NOT classified as a
        // tool failure and stays retryable. Document both sides.
        assert!(!is_retryable_model_failure(Some(
            "bash failed (exit 1): quota exceeded"
        )));
        assert!(!is_retryable_model_failure(Some(
            "mcp.server/write failed with exit code 1 rate limit"
        )));
        // The "with exit code N" branch has no trailing colon in the upstream
        // pattern, so "with exit code 1:" does not classify as a tool failure.
        assert!(is_retryable_model_failure(Some(
            "mcp.server/write failed with exit code 1: rate limit"
        )));
        assert!(is_retryable_model_failure(Some(
            "cargo test failed (exit 101): upstream is down"
        )));
        assert!(!is_retryable_model_failure(Some(
            "some unrelated compile note"
        )));
    }

    #[test]
    fn attempt_note_format() {
        assert_eq!(
            format_model_attempt_note("openai/x", Some("boom "), None, Some("openai/y")),
            "[fallback] openai/x failed: boom. Retrying with openai/y."
        );
        assert_eq!(
            format_model_attempt_note("openai/x", None, Some(3), None),
            "[fallback] openai/x failed: exit 3."
        );
    }
}
