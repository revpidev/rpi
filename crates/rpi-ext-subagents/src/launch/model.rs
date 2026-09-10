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

/// `resolveModelCandidate` (model-fallback.ts:195-209): resolve a possibly
/// loose model id to canonical `provider/id`; exact registry matches win,
/// thinking suffix is retried on the base when the whole id misses. The
/// lenient variant (miss → verbatim passthrough) — kept as the semantic
/// reference for the M3/TE19 model face (R7.1.10.2, modelExclusions);
/// subagent launches use the strict/required variants above (#1093), and
/// this function currently has no production caller (unit tests only).
#[allow(dead_code)]
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

/// `resolveSubagentModelCandidate` (model-fallback.ts:210-219 @ 0fc0eebb,
/// #1093): the strict variant used for subagent launches — an empty/absent
/// registry keeps the string verbatim (R7.1.4.4), but a non-empty registry
/// that cannot match the id (whole or thinking-suffix base) yields `None`
/// instead of passing the raw string through.
pub fn resolve_subagent_model_candidate(
    model: &str,
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
) -> Option<String> {
    let Some(models) = available_models else {
        return Some(model.to_string());
    };
    if models.is_empty() {
        return Some(model.to_string());
    }
    if let Some(resolved) = resolve_base_model_candidate(model, models, preferred_provider) {
        return Some(resolved);
    }
    let (base, suffix) = split_thinking_suffix(model);
    if !suffix.is_empty() {
        if let Some(resolved) = resolve_base_model_candidate(base, models, preferred_provider) {
            return Some(format!("{resolved}{suffix}"));
        }
    }
    None
}

/// `resolveRequiredSubagentModelCandidate` (model-fallback.ts:230-238
/// @ 0fc0eebb, #1093 / R7.1.4.5): the strict resolution with a fail-closed
/// error — a non-empty registry must match the requested model before the
/// child spawns, instead of forwarding an invalid `--model` to the child.
/// The upstream "Did you mean" cross-provider suggestion is not ported
/// (upstream `suggestAlternateProviderModel` depends on the in-process
/// registry shape); the model name + registry pointer are kept verbatim.
pub fn resolve_required_subagent_model_candidate(
    model: &str,
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
) -> Result<String, String> {
    resolve_subagent_model_candidate(model, available_models, preferred_provider)
        .ok_or_else(|| format!("Unknown subagent model '{model}' in the active Pi model registry."))
}

/// R7.1.4.4 (TE18 FR-D): the explicit empty-registry branch — fuzzy
/// resolution is skipped (verbatim passthrough), which must stay visible in
/// launch diagnostics instead of silently regressing to passthrough.
/// `Some(text)` only when a registry is unavailable AND a model string was
/// in play that would otherwise have been fuzzy-resolved.
pub fn registry_unavailable_diagnostic(has_model_input: bool) -> Option<&'static str> {
    has_model_input.then_some(
        "model registry unavailable (ctx.scopedModels empty) -> skipping fuzzy model resolution; passing model strings through verbatim",
    )
}

// ---------------------------------------------------------------------------
// Thinking ceiling (`maxThinking`, #1397 — shared/thinking-ceiling.ts)
// ---------------------------------------------------------------------------

/// `THINKING_LEVELS` (model-info.ts:3): rank order, index = rank.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `parseThinkingLevel` (thinking-ceiling.ts:15-19): valid level or error.
pub fn parse_thinking_level(value: &str) -> Result<&'static str, String> {
    THINKING_LEVELS
        .iter()
        .find(|level| **level == value)
        .copied()
        .ok_or_else(|| {
            format!(
                "Invalid thinking level; expected one of {}.",
                THINKING_LEVELS.join(", ")
            )
        })
}

fn thinking_rank(level: &str) -> Option<usize> {
    THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
}

/// `intersectThinkingCeilings` (thinking-ceiling.ts:21-25): the lowest of
/// the present ceilings (`None` entries drop out; all `None` → `None`).
pub fn intersect_thinking_ceilings<'a>(
    ceilings: impl IntoIterator<Item = Option<&'a str>>,
) -> Option<&'static str> {
    ceilings
        .into_iter()
        .flatten()
        .filter_map(|level| thinking_rank(level).map(|_| level))
        .min_by_key(|level| thinking_rank(level).unwrap_or(usize::MAX))
        .and_then(|level| parse_thinking_level(level).ok())
}

/// `resolveEffectiveThinking` (model-info.ts:56-61): the model string's
/// thinking suffix wins over the config thinking; `None` when neither
/// carries one (the model catalog's default does not count for the assert).
pub fn effective_requested_thinking(
    model: Option<&str>,
    config_thinking: Option<&str>,
) -> Option<String> {
    if let Some(model) = model {
        let (_, suffix) = split_thinking_suffix(model);
        let suffix = suffix.strip_prefix(':').unwrap_or(suffix);
        if !suffix.is_empty() && thinking_rank(suffix).is_some() {
            return Some(suffix.to_string());
        }
    }
    config_thinking
        .filter(|level| thinking_rank(level).is_some())
        .map(str::to_string)
}

/// `assertThinkingWithinCeiling` (thinking-ceiling.ts:43-56): an explicit
/// requested level above the ceiling fails the launch (fail-closed, not a
/// clamp).
pub fn assert_thinking_within_ceiling(
    requested: &str,
    ceiling: &str,
    agent: &str,
    run_id: &str,
) -> Result<(), String> {
    let (Some(requested_rank), Some(ceiling_rank)) =
        (thinking_rank(requested), thinking_rank(ceiling))
    else {
        return Ok(());
    };
    if requested_rank <= ceiling_rank {
        return Ok(());
    }
    Err(format!(
        "Thinking level '{requested}' exceeds configured maximum '{ceiling}' for agent '{agent}' run '{run_id}'."
    ))
}

// ---------------------------------------------------------------------------
// modelScope (model-scope.ts)
// ---------------------------------------------------------------------------

/// `ModelScopeConfig` (model-scope.ts:12-20 + #1328 `agents`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelScopeConfig {
    pub enforce: Option<bool>,
    pub strict: Option<bool>,
    pub allow: Option<Vec<String>>,
    /// #1328: additional restrictions keyed by canonical agent name.
    pub agents: std::collections::BTreeMap<String, ModelScopeConfig>,
}

impl ModelScopeConfig {
    pub fn enforced(&self) -> bool {
        self.enforce == Some(true)
    }

    /// `resolveModelScopesForAgent` (model-scope.ts:151-170): the global
    /// rule (when it carries an allow list) plus the matching per-agent rule,
    /// each with `inherit` allow patterns expanded to the parent's
    /// `provider/id`. Inherited per-agent fields fall back to the global
    /// rule.
    pub fn resolved_scopes_for_agent(
        &self,
        agent_name: &str,
        parent_model: Option<(&str, &str)>,
    ) -> Vec<(ModelScopeConfig, String)> {
        let expand = |patterns: &[String]| -> Vec<String> {
            patterns
                .iter()
                .map(|pattern| {
                    if pattern == "inherit" {
                        match parent_model {
                            Some((provider, id)) => format!("{provider}/{id}"),
                            None => pattern.clone(),
                        }
                    } else {
                        pattern.clone()
                    }
                })
                .collect()
        };
        let mut scopes = Vec::new();
        if let Some(allow) = &self.allow {
            let mut rule = ModelScopeConfig {
                enforce: self.enforce,
                strict: self.strict,
                allow: Some(expand(allow)),
                agents: Default::default(),
            };
            if rule.enforce.is_none() {
                rule.enforce = Some(false);
            }
            scopes.push((rule, "modelScope".to_string()));
        }
        if let Some(agent_rule) = self.agents.get(agent_name) {
            if let Some(allow) = &agent_rule.allow {
                let rule = ModelScopeConfig {
                    enforce: Some(agent_rule.enforce.unwrap_or(self.enforce.unwrap_or(false))),
                    strict: agent_rule.strict.or(self.strict),
                    allow: Some(expand(allow)),
                    agents: Default::default(),
                };
                scopes.push((rule, format!("modelScope.agents.{agent_name}")));
            }
        }
        scopes
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

/// `checkModelScope` (model-scope.ts:62-82): pure scope decision (one rule).
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
    // #1328 `modelScope.agents.<name>`: per-agent restrictions
    // (model-scope.ts:179-196); nested `agents` inside a rule is rejected.
    if let Some(agents) = object.get("agents") {
        let Some(agents_object) = agents.as_object() else {
            return Err(
                "have invalid 'modelScope.agents'; expected an object keyed by agent name."
                    .to_string(),
            );
        };
        for (raw_name, raw_rule) in agents_object {
            let name = raw_name.trim();
            if name.is_empty() {
                return Err(
                    "have invalid 'modelScope.agents' key; expected a non-empty agent name."
                        .to_string(),
                );
            }
            let Some(rule_object) = raw_rule.as_object() else {
                return Err(format!(
                    "have invalid 'modelScope.agents.{name}'; expected an object."
                ));
            };
            if rule_object.contains_key("agents") {
                return Err(format!(
                    "have invalid 'modelScope.agents.{name}.agents'; nested agent scopes are not supported."
                ));
            }
            let mut rule_wrapper = serde_json::Map::new();
            for key in ["enforce", "strict", "allow"] {
                if let Some(value) = rule_object.get(key) {
                    rule_wrapper.insert(key.to_string(), value.clone());
                }
            }
            let parsed_rule = parse_model_scope_config(Some(&Value::Object(rule_wrapper)))
                .map(|rule| rule.unwrap_or_default())
                .map_err(|message| {
                    message.replace("modelScope.", &format!("modelScope.agents.{name}."))
                })?;
            config.agents.insert(name.to_string(), parsed_rule);
        }
    }
    Ok(Some(config))
}

// ---------------------------------------------------------------------------
// Override resolution (model-fallback.ts:180+)
// ---------------------------------------------------------------------------

/// `resolveSubagentModelOverride` (model-fallback.ts:362-391 @ 0fc0eebb):
/// resolve the `--model` override for a spawned child. Empty/`inherit` →
/// parent session model. An explicit string is strict-resolved; when the
/// request source is `explicit` the #1093 required check fail-closes on a
/// registry miss, inherited sources keep the verbatim passthrough. Out of
/// scope: `Err` for explicit + strict scope, warn callback otherwise.
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
            let candidate =
                resolve_subagent_model_candidate(explicit, available_models, preferred_provider);
            if source == ModelSource::Explicit {
                // #1093 (R7.1.4.5): explicit requests must exist in a
                // non-empty registry before spawn — fail closed.
                Some(candidate.map_or_else(
                    || {
                        resolve_required_subagent_model_candidate(
                            explicit,
                            available_models,
                            preferred_provider,
                        )
                    },
                    Ok,
                )?)
            } else if let Some(candidate) = candidate {
                Some(candidate)
            } else {
                Some(explicit.to_string())
            }
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

/// How the primary model of a launch was selected (upstream `ModelOrigin`,
/// model-fallback.ts:422 @ 0fc0eebb): explicit call param, inherited from
/// the parent session, or agent-configured. Decides where the #1093
/// required (fail-closed) check applies in the candidate chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrigin {
    Explicit,
    Inherited,
    Configured,
}

/// `resolveModelOrigin` (model-fallback.ts:437-450 @ 0fc0eebb).
pub fn resolve_model_origin(
    explicit_model: Option<&str>,
    agent_model: Option<&str>,
    parent_model: Option<(&str, &str)>,
) -> ModelOrigin {
    // inheritsParentModel: parent present and the effective request
    // (explicit ?? agent) is empty or the `inherit` sentinel.
    let effective = explicit_model.or(agent_model).map(str::trim).unwrap_or("");
    if parent_model.is_some() && (effective.is_empty() || effective == INHERIT_MODEL) {
        return ModelOrigin::Inherited;
    }
    let explicit_trimmed = explicit_model.map(str::trim).unwrap_or("");
    if !explicit_trimmed.is_empty() && explicit_trimmed != INHERIT_MODEL {
        return ModelOrigin::Explicit;
    }
    ModelOrigin::Configured
}

/// `buildModelCandidates` (model-fallback.ts:461-541 @ 0fc0eebb): primary +
/// fallbacks, deduped, resolved against the registry. The primary passes
/// through when it was resolved by the caller (explicit/inherited origin); a
/// configured primary and fallback entries go through the strict resolution,
/// misses are skipped with a warn, and a chain left with no usable candidate
/// re-runs the first skip through the required check so the launch fails
/// closed (#1093).
///
/// v0.66 additions (TE19 FR-G): every resolved candidate then passes the
/// active exclusion filter (#1318 — excluded candidates drop with a warn
/// diagnostic; an all-excluded chain fails closed with per-exclusion
/// evidence, #1439) and the resolved global + per-agent scope checks
/// (#1328 — `resolveModelScopesForAgent`).
#[allow(clippy::too_many_arguments)]
pub fn build_model_candidates(
    primary_model: Option<&str>,
    fallback_models: &[String],
    available_models: Option<&[AvailableModel]>,
    preferred_provider: Option<&str>,
    scope: Option<&ModelScopeConfig>,
    agent_name: Option<&str>,
    parent_model: Option<(&str, &str)>,
    origin: ModelOrigin,
    on_warn: &mut dyn FnMut(&ModelScopeViolation),
) -> Result<Vec<String>, String> {
    let resolved_scopes: Vec<(ModelScopeConfig, String)> = scope
        .map(|scope| match agent_name {
            Some(name) => scope.resolved_scopes_for_agent(name, parent_model),
            None => scope
                .resolved_scopes_for_agent("", parent_model)
                .into_iter()
                .filter(|(_rule, origin)| origin == "modelScope")
                .collect(),
        })
        .unwrap_or_default();
    let enforce_scopes = |model: &str,
                          source: ModelSource,
                          warn: &mut dyn FnMut(&ModelScopeViolation)|
     -> Result<(), String> {
        for (rule, rule_origin) in &resolved_scopes {
            if let Some(mut violation) = check_model_scope(Some(model), Some(rule), source) {
                violation.message = format!(
                    "Model '{}' is outside the configured subagent model scope ({}). Allowed patterns: {}.",
                    violation.model,
                    rule_origin,
                    violation
                        .allowed_patterns
                        .join(", ")
                );
                if violation.is_error {
                    return Err(violation.message);
                }
                warn(&violation);
            }
        }
        Ok(())
    };

    if let ModelOrigin::Explicit = origin {
        // Explicit primaries fail closed immediately (upstream normalizes
        // them via the required resolution before the chain) and pass the
        // scope checks as "explicit" (hard error on violation).
        if let Some(primary) = primary_model.map(str::trim).filter(|m| !m.is_empty()) {
            let normalized = resolve_required_subagent_model_candidate(
                primary,
                available_models,
                preferred_provider,
            )?;
            // An explicitly requested model under an active exclusion fails
            // closed with the exclusion reason instead of silently swapping
            // (`throwForExplicitModelExclusion`, model-fallback.ts:337-341 —
            // the reason passes control-char normalization + secret
            // redaction + the 240 cap).
            if let Some(exclusion) =
                crate::launch::model_exclusions::find_model_exclusion(&normalized)
            {
                let reason = crate::launch::model_exclusions::sanitize_diagnostic(
                    &exclusion.reason,
                    "runtime-failure",
                );
                return Err(format!(
                    "Requested subagent model '{normalized}' is excluded and cannot be replaced by a fallback (reason: {reason}; expires: {}).",
                    exclusion.expires_at
                ));
            }
            if !resolved_scopes.is_empty() {
                enforce_scopes(&normalized, ModelSource::Explicit, on_warn)?;
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut candidates: Vec<String> = Vec::new();
    let raw: Vec<Option<&str>> = std::iter::once(primary_model)
        .chain(fallback_models.iter().map(|s: &String| Some(s.as_str())))
        .collect();
    let mut skipped_primary: Option<String> = None;
    let mut skipped_fallback: Option<String> = None;
    for (index, raw_entry) in raw.into_iter().enumerate() {
        let Some(raw) = raw_entry else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let normalized =
            if index == 0 && matches!(origin, ModelOrigin::Inherited | ModelOrigin::Explicit) {
                // Already resolved by the caller (or the parent session itself).
                raw.to_string()
            } else {
                match resolve_subagent_model_candidate(raw, available_models, preferred_provider) {
                    Some(normalized) => normalized,
                    None => {
                        if index == 0 {
                            skipped_primary = Some(raw.to_string());
                        } else {
                            if skipped_fallback.is_none() {
                                skipped_fallback = Some(raw.to_string());
                            }
                            tracing::warn!(
                                model = raw,
                                "skipping fallback model unavailable in this environment"
                            );
                        }
                        continue;
                    }
                }
            };
        if seen.contains(&normalized) {
            continue;
        }
        if index > 0
            || resolved_scopes
                .iter()
                .any(|(rule, _)| rule.enforced() && rule.strict == Some(true))
        {
            enforce_scopes(&normalized, ModelSource::Inherited, on_warn)?;
        }
        seen.insert(normalized.clone());
        candidates.push(normalized);
    }
    // #1318 exclusion filter: excluded candidates drop with a diagnostic;
    // the zero-usable case fails closed with evidence (#1439).
    let mut excluded_evidence: Vec<String> = Vec::new();
    let mut excluded_count = 0usize;
    {
        let mut on_excluded =
            |candidate: &str, exclusion: &crate::launch::model_exclusions::ModelExclusion| {
                excluded_count += 1;
                let cap = crate::launch::model_exclusions::MODEL_EXCLUSION_DIAGNOSTIC_MAX_ENTRIES;
                if excluded_evidence.len() < cap {
                    // `formatExcludedCandidateEvidence` (model-fallback.ts:309-316):
                    // every field passes the diagnostic sanitizer (control-char
                    // collapse + secret redaction + 240 cap).
                    let sanitize = crate::launch::model_exclusions::sanitize_diagnostic;
                    let display_candidate = sanitize(candidate, "unknown");
                    let display_model =
                        sanitize(exclusion.model_id.as_deref().unwrap_or(""), "unspecified");
                    let display_provider =
                        sanitize(exclusion.provider.as_deref().unwrap_or(""), "unspecified");
                    let reason = sanitize(&exclusion.reason, "runtime-failure");
                    excluded_evidence.push(format!(
                        "{display_candidate} — model: {display_model}; provider: {display_provider}; reason: {reason}; expires: {}",
                        exclusion.expires_at
                    ));
                }
            };
        candidates = crate::launch::model_exclusions::filter_fallback_candidates(
            candidates,
            Some(&mut on_excluded),
        );
    }
    if candidates.is_empty() {
        // A chain that skipped its only resolvable entry fails closed through
        // the required check (upstream re-runs the first skip).
        if let Some(primary) = skipped_primary {
            resolve_required_subagent_model_candidate(
                &primary,
                available_models,
                preferred_provider,
            )?;
        }
        if let Some(fallback) = skipped_fallback {
            resolve_required_subagent_model_candidate(
                &fallback,
                available_models,
                preferred_provider,
            )?;
        }
        // #1439 zero-usable evidence: every resolvable candidate was
        // excluded — fail closed naming them (`ZERO_USABLE_MODEL_CANDIDATES_ERROR`).
        if excluded_count > 0 {
            let evidence = if excluded_evidence.is_empty() {
                String::new()
            } else {
                let omitted = excluded_count - excluded_evidence.len();
                format!(
                    " (excluded: {}{})",
                    excluded_evidence.join("; "),
                    if omitted > 0 {
                        format!("; ... and {omitted} more")
                    } else {
                        String::new()
                    }
                )
            };
            return Err(format!(
                "No usable subagent models remain after registry, scope, and cached-exclusion filtering.{evidence}"
            ));
        }
    }
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// Retry classification (model-fallback.ts:320-340)
// ---------------------------------------------------------------------------

/// `RETRYABLE_MODEL_FAILURE_PATTERNS` (model-fallback.ts:537-577 @ v0.66.0
/// 0fc0eebb) — matched against the child error text. Every pattern carries
/// its own flags; `REQUEST_LIMIT_EXCEEDED` is anchored and case-sensitive
/// exactly like upstream.
static RETRYABLE_MODEL_FAILURE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^REQUEST_LIMIT_EXCEEDED$",
        r"(?i)rate\s*limit",
        r"(?i)usage\s*limit",
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
        r"(?i)connection\s+(?:error|reset|closed|aborted)",
        r"(?i)connection refused",
        r"(?i)fetch failed",
        r"(?i)network error",
        r"(?i)socket hang up",
        r"(?i)stream ended without finish_reason",
        r"(?i)upstream",
        r"(?i)timed? out",
        r"(?i)timeout",
        r"(?i)\b500\b",
        r"(?i)\b502\b",
        r"(?i)\b503\b",
        r"(?i)\b504\b",
        r"(?i)internal server error",
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

/// `isRetryableModelFailure` (model-fallback.ts:579-584 @ v0.66.0).
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

/// `CONTEXT_OVERFLOW_PATTERNS` (model-fallback.ts:625-637 @ v0.66.0).
/// Deliberately disjoint from [`RETRYABLE_MODEL_FAILURE_PATTERNS`]: an overflow
/// means the input exceeded the model's context window, so retrying the same
/// input (same model or a fallback) cannot succeed.
static CONTEXT_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)context(?: length| window| limit)? (?:exceed|overflow|too long)",
        r"(?i)maximum context length",
        r"(?i)too many tokens",
        r"(?i)token limit",
        r"(?i)context_length_exceeded",
        r"(?i)length_required",
        r"(?i)maximum.*tokens",
        r"(?i)prompt.*too long",
        r"(?i)input.*too long",
        r"(?i)exceeded.*context",
        r"(?i)context.*overflow",
    ]
    .iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// `isContextOverflow` (model-fallback.ts:643-647 @ v0.66.0): a terminal,
/// non-retryable classification — callers must not advance the fallback chain
/// (R7.1.2.2).
pub fn is_context_overflow(error: Option<&str>) -> bool {
    let Some(error) = error else {
        return false;
    };
    if TOOL_FAILURE_PREFIX.is_match(error.trim()) {
        return false;
    }
    CONTEXT_OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| pattern.is_match(error))
}

/// `formatEmptyTerminalAssistantResponseError` cold-start text
/// (utils.ts:481) — the empty-output attempt still advances the chain.
const EMPTY_OUTPUT_COLD_START_ERROR: &str =
    "Subagent produced no output (possible model cold-start or empty response).";

/// `/^Subagent produced no output after terminal assistant stopReason "[^"]+"\.$/`
/// (model-fallback.ts:603).
static EMPTY_OUTPUT_STOP_REASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^Subagent produced no output after terminal assistant stopReason "[^"]+"\.$"#)
        .expect("empty output stop reason regex")
});

/// `isRetryableModelFailureAttempt` (model-fallback.ts:601-611 @ v0.66.0,
/// R7.1.2.3): a retryable *model* failure may only replay the whole task when
/// the attempt produced **no tool activity** (`toolCount > 0` → false — a
/// replay would re-run tools against the same cwd/worktree). `messages`
/// carries the attempt transcript: the upstream tail only replays when the
/// error text is an errorMessage of one of those messages (or the attempt had
/// no messages at all).
pub fn is_retryable_model_failure_attempt(
    error: Option<&str>,
    messages: &[Value],
    tool_count: u64,
) -> bool {
    if !is_retryable_model_failure(error) {
        return false;
    }
    if tool_count > 0 {
        return false;
    }
    let Some(error_text) = error else {
        return false;
    };
    if error_text == EMPTY_OUTPUT_COLD_START_ERROR
        || EMPTY_OUTPUT_STOP_REASON_RE.is_match(error_text)
    {
        return true;
    }
    if messages.is_empty() {
        return true;
    }
    let trimmed = error_text.trim();
    !trimmed.is_empty()
        && messages.iter().any(|message| {
            message
                .get("errorMessage")
                .and_then(Value::as_str)
                .map(str::trim)
                == Some(trimmed)
        })
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

    pub(super) fn models() -> Vec<AvailableModel> {
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
            agents: Default::default(),
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
            agents: Default::default(),
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
            agents: Default::default(),
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
            // configured origin: the primary is strict-resolved like a
            // fallback (the test's "claude-5" hits the registry).
            None,
            None,
            ModelOrigin::Configured,
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
        // v0.66 additions (#1215/#1955/#1957, model-fallback.ts:537-577).
        assert!(is_retryable_model_failure(Some("REQUEST_LIMIT_EXCEEDED")));
        // Anchored + case-sensitive upstream (`/^REQUEST_LIMIT_EXCEEDED$/`).
        assert!(!is_retryable_model_failure(Some("request_limit_exceeded")));
        assert!(!is_retryable_model_failure(Some(
            "REQUEST_LIMIT_EXCEEDED retry later"
        )));
        assert!(is_retryable_model_failure(Some(
            "usage limit reached for this account"
        )));
        assert!(is_retryable_model_failure(Some("connection reset by peer")));
        assert!(is_retryable_model_failure(Some(
            "HTTP 500 Internal Server Error"
        )));
        assert!(is_retryable_model_failure(Some("Internal Server Error")));
    }

    #[test]
    fn context_overflow_classification() {
        // R7.1.2.2: independent, non-retryable classification.
        assert!(is_context_overflow(Some("context_length_exceeded")));
        assert!(is_context_overflow(Some(
            "maximum context length is 200000 tokens"
        )));
        assert!(is_context_overflow(Some(
            "prompt is too long for this model"
        )));
        assert!(!is_context_overflow(None));
        assert!(!is_context_overflow(Some(
            "plain provider failure without overflow wording"
        )));
        // TOOL_FAILURE_PREFIX precedence: a tool's own "token limit" text is
        // neither overflow nor a model failure.
        assert!(!is_context_overflow(Some(
            "write failed (exit 1): token limit exceeded"
        )));
        assert!(!is_retryable_model_failure(Some(
            "write failed (exit 1): token limit exceeded"
        )));
        // Overflow and retryable patterns stay mutually exclusive on the
        // same error text (task §6 A3).
        for error in [
            "context_length_exceeded",
            "maximum context length is 200000 tokens",
            "prompt is too long for this model",
        ] {
            assert!(!is_retryable_model_failure(Some(error)), "{error}");
        }
    }

    #[test]
    fn retryable_attempt_guard_blocks_replay_after_tools() {
        let retryable = Some("rate limit exceeded");
        // toolCount > 0 → never replay the whole task (R7.1.2.3 A1).
        assert!(!is_retryable_model_failure_attempt(retryable, &[], 1));
        assert!(!is_retryable_model_failure_attempt(
            retryable,
            &[serde_json::json!({ "role": "assistant", "errorMessage": "rate limit exceeded" })],
            3
        ));
        // toolCount == 0 + no messages → still replayable.
        assert!(is_retryable_model_failure_attempt(retryable, &[], 0));
        // Empty-output diagnostics replay even with messages present.
        assert!(is_retryable_model_failure_attempt(
            Some(EMPTY_OUTPUT_COLD_START_ERROR),
            &[serde_json::json!({ "role": "assistant", "content": [] })],
            0
        ));
        assert!(is_retryable_model_failure_attempt(
            Some("Subagent produced no output after terminal assistant stopReason \"length\"."),
            &[],
            0
        ));
        // With a transcript, the error must come from one of its messages
        // (model-fallback.ts:610 tail).
        assert!(is_retryable_model_failure_attempt(
            retryable,
            &[serde_json::json!({ "role": "assistant", "errorMessage": "rate limit exceeded" })],
            0
        ));
        assert!(!is_retryable_model_failure_attempt(
            retryable,
            &[serde_json::json!({ "role": "assistant", "errorMessage": "other failure" })],
            0
        ));
        // Non-retryable and context-overflow texts never replay.
        assert!(!is_retryable_model_failure_attempt(
            Some("some unrelated compile note"),
            &[],
            0
        ));
        assert!(!is_retryable_model_failure_attempt(
            Some("context_length_exceeded"),
            &[],
            0
        ));
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

/// TE18 (R7.1.4.4/.5, #1093): strict/required model resolution, origin-aware
/// candidate chains, and the explicit empty-registry diagnostic branch.
#[cfg(test)]
mod te18_model_tests {
    use super::tests::models;
    use super::*;

    fn sink(_violation: &ModelScopeViolation) {}

    #[test]
    fn strict_candidate_empty_registry_passes_through() {
        // R7.1.4.4: an empty/absent registry is NOT "no usable models" —
        // the string passes verbatim (diagnostic branch, not an error).
        for registry in [None, Some(&[][..])] {
            assert_eq!(
                resolve_subagent_model_candidate("faux/primary", registry, None),
                Some("faux/primary".to_string())
            );
        }
    }

    #[test]
    fn strict_candidate_registry_hit_and_miss() {
        let registry = models();
        // whole-id hit
        assert_eq!(
            resolve_subagent_model_candidate("anthropic/claude-5", Some(&registry), None),
            Some("anthropic/claude-5".to_string())
        );
        // shorthand hit canonicalizes
        assert_eq!(
            resolve_subagent_model_candidate("claude-5", Some(&registry), None),
            Some("anthropic/claude-5".to_string())
        );
        // thinking-suffix retry on the base
        assert_eq!(
            resolve_subagent_model_candidate("claude-5:high", Some(&registry), None),
            Some("anthropic/claude-5:high".to_string())
        );
        // miss → None (strict), never passthrough
        assert_eq!(
            resolve_subagent_model_candidate("faux/primary", Some(&registry), None),
            None
        );
    }

    #[test]
    fn required_candidate_fails_closed_upstream_message() {
        let registry = models();
        let error =
            resolve_required_subagent_model_candidate("faux/primary", Some(&registry), None)
                .unwrap_err();
        // Upstream message (model-fallback.ts:235-237 @ 0fc0eebb): model
        // name + active-registry pointer. (The cross-provider "Did you
        // mean" suggestion is not ported — in-process registry dependency.)
        assert_eq!(
            error,
            "Unknown subagent model 'faux/primary' in the active Pi model registry."
        );
        // Empty registry never fails (passthrough is correct there).
        assert_eq!(
            resolve_required_subagent_model_candidate("faux/primary", None, None),
            Ok("faux/primary".to_string())
        );
    }

    #[test]
    fn explicit_override_fails_closed_inherited_passes_through() {
        let registry = models();
        // Explicit source + registry miss → Err (#1093).
        let error = resolve_subagent_model_override(
            Some("faux/primary"),
            None,
            Some(&registry),
            None,
            None,
            ModelSource::Explicit,
            &mut sink,
        )
        .unwrap_err();
        assert!(error.contains("Unknown subagent model"), "{error}");
        // Inherited source (agent-config models resolve leniently here) →
        // verbatim passthrough; the fail-closed lands in the candidate chain.
        assert_eq!(
            resolve_subagent_model_override(
                Some("faux/primary"),
                None,
                Some(&registry),
                None,
                None,
                ModelSource::Inherited,
                &mut sink,
            )
            .unwrap(),
            Some("faux/primary".to_string())
        );
        // Explicit source + registry hit resolves canonically.
        assert_eq!(
            resolve_subagent_model_override(
                Some("claude-5"),
                None,
                Some(&registry),
                None,
                None,
                ModelSource::Explicit,
                &mut sink,
            )
            .unwrap(),
            Some("anthropic/claude-5".to_string())
        );
    }

    #[test]
    fn origin_classification() {
        let parent = ("anthropic", "claude-5");
        assert_eq!(
            resolve_model_origin(Some("openai/gpt-5.5"), None, Some(parent)),
            ModelOrigin::Explicit
        );
        assert_eq!(
            resolve_model_origin(None, None, Some(parent)),
            ModelOrigin::Inherited
        );
        assert_eq!(
            resolve_model_origin(None, Some("inherit"), Some(parent)),
            ModelOrigin::Inherited
        );
        assert_eq!(
            resolve_model_origin(None, Some("anthropic/claude-5"), Some(parent)),
            ModelOrigin::Configured
        );
        assert_eq!(
            resolve_model_origin(None, Some("anthropic/claude-5"), None),
            ModelOrigin::Configured
        );
    }

    #[test]
    fn candidates_origin_aware_required_checks() {
        let registry = models();
        // Explicit primary missing from the registry → immediate Err.
        let error = build_model_candidates(
            Some("faux/primary"),
            &[],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Explicit,
            &mut sink,
        )
        .unwrap_err();
        assert!(error.contains("Unknown subagent model"), "{error}");
        // Configured primary missing → skipped, then fail-closed at the end
        // when nothing usable remains.
        let error = build_model_candidates(
            Some("faux/primary"),
            &[],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Configured,
            &mut sink,
        )
        .unwrap_err();
        assert!(error.contains("Unknown subagent model"), "{error}");
        // Configured primary missing + usable fallback → primary skipped
        // (warned), fallback survives.
        let candidates = build_model_candidates(
            Some("faux/primary"),
            &["claude-5".to_string()],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Configured,
            &mut sink,
        )
        .unwrap();
        assert_eq!(candidates, vec!["anthropic/claude-5".to_string()]);
        // Primary hit + fallback miss → fallback skipped with a warn, the
        // chain stays usable (upstream only fail-closes when NOTHING
        // remains).
        let candidates = build_model_candidates(
            Some("anthropic/claude-5"),
            &["faux/secondary".to_string()],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Explicit,
            &mut sink,
        )
        .unwrap();
        assert_eq!(candidates, vec!["anthropic/claude-5".to_string()]);
        // No primary + an unresolvable fallback → the chain is empty, the
        // skipped fallback fails closed.
        let error = build_model_candidates(
            None,
            &["faux/secondary".to_string()],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Configured,
            &mut sink,
        )
        .unwrap_err();
        assert!(
            error.contains("Unknown subagent model 'faux/secondary'"),
            "{error}"
        );
        // Inherited primary passes through even when outside the registry
        // (parent session model is authoritative).
        let candidates = build_model_candidates(
            Some("faux/parent-model"),
            &[],
            Some(&registry),
            None,
            None,
            None,
            None,
            ModelOrigin::Inherited,
            &mut sink,
        )
        .unwrap();
        assert_eq!(candidates, vec!["faux/parent-model".to_string()]);
    }

    #[test]
    fn registry_unavailable_diagnostic_branch() {
        // The empty-registry branch must be explicit and observable (G3:
        // the diagnostic text is asserted so passthrough cannot regress
        // silently).
        assert!(registry_unavailable_diagnostic(true).is_some());
        let text = registry_unavailable_diagnostic(true).unwrap();
        assert!(
            text.contains("model registry unavailable")
                && text.contains("skipping fuzzy model resolution"),
            "{text}"
        );
        assert!(registry_unavailable_diagnostic(false).is_none());
    }
    // ---- TE19 (R7.1.10): thinking ceiling + scope agents + exclusions ----

    #[test]
    fn thinking_ceiling_helpers_match_upstream_ranks() {
        assert_eq!(parse_thinking_level("off").unwrap(), "off");
        assert!(parse_thinking_level("ultra").is_err());
        // Intersect keeps the LOWEST present ceiling; all-None stays None.
        assert_eq!(
            intersect_thinking_ceilings([Some("high"), Some("low"), None]),
            Some("low")
        );
        assert_eq!(intersect_thinking_ceilings([None::<&str>, None]), None);
        // Model suffix wins over config thinking; catalog defaults never
        // surface (resolveEffectiveThinking).
        assert_eq!(
            effective_requested_thinking(Some("prov/m:high"), Some("low")),
            Some("high".to_string())
        );
        assert_eq!(
            effective_requested_thinking(Some("prov/m"), Some("low")),
            Some("low".to_string())
        );
        assert_eq!(effective_requested_thinking(Some("prov/m"), None), None);
        // Within ceiling passes; above fails closed with the agent/run ids.
        assert!(assert_thinking_within_ceiling("low", "high", "a", "r").is_ok());
        let error = assert_thinking_within_ceiling("max", "high", "scout", "run-1").unwrap_err();
        assert!(
            error.contains("'max' exceeds configured maximum 'high'"),
            "{error}"
        );
        assert!(error.contains("agent 'scout' run 'run-1'"), "{error}");
    }

    #[test]
    fn model_scope_agents_and_inherit_alias() {
        // The exclusion store is process-global; isolate this test's
        // explicit-candidate checks from any recorded exclusion.
        std::env::set_var(
            "RPI_MODEL_EXCLUSIONS_PATH",
            std::env::temp_dir().join(format!("rpi-model-excl-scope-{}", std::process::id())),
        );
        crate::launch::model_exclusions::reset_for_test();
        let value: Value = serde_json::from_str(
            r#"{"enforce":true,"allow":["anthropic/*"],
                "agents":{"researcher":{"allow":["openai/*","inherit"]}}}"#,
        )
        .unwrap();
        let scope = parse_model_scope_config(Some(&value)).unwrap().unwrap();
        // Global rule for an agent without its own rule.
        let scopes = scope.resolved_scopes_for_agent("worker", Some(("openai", "gpt")));
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].1, "modelScope");
        // Per-agent rule: agent gets global + its own; `inherit` expands to
        // the parent's provider/id.
        let scopes = scope.resolved_scopes_for_agent("researcher", Some(("openai", "gpt")));
        assert_eq!(scopes.len(), 2, "{scopes:?}");
        assert_eq!(scopes[1].1, "modelScope.agents.researcher");
        let allow = scopes[1].0.allow.as_deref().unwrap();
        assert!(allow.contains(&"openai/gpt".to_string()), "{allow:?}");
        assert!(allow.contains(&"openai/*".to_string()), "{allow:?}");
        // Nested agent rules are rejected.
        let nested: Value =
            serde_json::from_str(r#"{"agents":{"x":{"agents":{"y":{"allow":["a/*"]}}}}}"#).unwrap();
        assert!(parse_model_scope_config(Some(&nested)).is_err());
        // build_model_candidates enforces the resolved rules: an explicit
        // model outside the agent's own allowlist fails closed naming the
        // rule origin, researcher's parent-matching model passes.
        let mut sink = |_violation: &ModelScopeViolation| {};
        let error = build_model_candidates(
            Some("openai/gpt"),
            &[],
            None,
            None,
            Some(&scope),
            Some("worker"),
            Some(("openai", "gpt")),
            ModelOrigin::Explicit,
            &mut sink,
        )
        .unwrap_err();
        assert!(error.contains("modelScope"), "{error}");
        let candidates = build_model_candidates(
            Some("openai/gpt"),
            &[],
            None,
            None,
            Some(&scope),
            Some("researcher"),
            Some(("openai", "gpt")),
            ModelOrigin::Inherited,
            &mut sink,
        )
        .unwrap();
        assert_eq!(candidates, vec!["openai/gpt".to_string()]);
    }
}
