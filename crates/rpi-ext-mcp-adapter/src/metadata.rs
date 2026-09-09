//! Core config/metadata types and tool-name formatting.
//!
//! Port of `types.ts` + `tool-metadata.ts` (+ `resourceNameToToolName` from
//! `resource-tools.ts`) @ pi-mcp-adapter v2.24.0 (3d953f90); functions rebased
//! onto v2.32.1 (`10a45367`) by TE21–TE23 carry an explicit `@ 10a45367`
//! anchor, the remaining comments keep the v2.24.0 anchor from the original
//! port.
//!
//! Intentional differences:
//! - `ServerEntry` / `McpSettings` are kept as order-preserving JSON maps
//!   (newtypes over `serde_json::Map`) instead of fully typed structs: the
//!   upstream merge semantics (`mergeServerMaps` per-field spread) and the
//!   cache wire format both require unknown keys to round-trip untouched,
//!   and `serde_json`'s `preserve_order` reproduces JS object key order.
//! - MCP UI `_meta` extraction (`uiResourceUri` / `uiVisibility` /
//!   `uiStreamMode`) is P2 [non-goal in TE02]; `build_tool_metadata` skips it.

use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// `ToolPrefix` (types.ts:445).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolPrefix {
    /// `server` — `<sanitizedServer>_<tool>` (upstream default).
    #[default]
    Server,
    /// `none` — bare tool names.
    None,
    /// `short` — server prefix with a trailing `-?mcp` stripped.
    Short,
    /// `mcp` — `mcp__<sanitizedServer>_<tool>`.
    Mcp,
}

impl ToolPrefix {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolPrefix::Server => "server",
            ToolPrefix::None => "none",
            ToolPrefix::Short => "short",
            ToolPrefix::Mcp => "mcp",
        }
    }

    pub fn from_config_value(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "server" => Some(ToolPrefix::Server),
            "none" => Some(ToolPrefix::None),
            "short" => Some(ToolPrefix::Short),
            "mcp" => Some(ToolPrefix::Mcp),
            _ => None,
        }
    }
}

/// `ServerEntry` (types.ts:360-427) as an order-preserving JSON object.
///
/// Field access goes through typed helpers; unknown/extra keys survive
/// merges and cache round-trips exactly like the upstream record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerEntry(pub Map<String, Value>);

impl ServerEntry {
    pub fn from_value(value: Value) -> Option<Self> {
        value.as_object().cloned().map(Self)
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    pub fn as_map_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.0
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// `isServerDisabled` (types.ts:430-432): only the literal boolean
    /// `true` disables a server.
    pub fn is_disabled(&self) -> bool {
        self.0.get("disabled") == Some(&Value::Bool(true))
    }

    /// Per-server `toolPrefix` override (types.ts:394).
    pub fn tool_prefix(&self) -> Option<ToolPrefix> {
        self.0
            .get("toolPrefix")
            .and_then(ToolPrefix::from_config_value)
    }

    /// `exposeResources !== false` (tool-metadata.ts:59).
    pub fn exposes_resources(&self) -> bool {
        self.0.get("exposeResources") != Some(&Value::Bool(false))
    }

    pub fn include_tools(&self) -> Option<&Value> {
        self.0.get("includeTools")
    }

    pub fn exclude_tools(&self) -> Option<&Value> {
        self.0.get("excludeTools")
    }

    pub fn search_keywords(&self) -> Option<&Map<String, Value>> {
        self.0.get("searchKeywords").and_then(Value::as_object)
    }
}

/// `McpConfig` (types.ts:536-540).
///
/// `imports` is parsed and preserved but NOT expanded in P0 [VARIANT,
/// requirements FR-P0-01 / design §3.2].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpConfig {
    /// Insertion-ordered (JS object key order).
    pub mcp_servers: IndexMap<String, ServerEntry>,
    pub imports: Option<Vec<String>>,
    pub settings: Option<Map<String, Value>>,
}

impl McpConfig {
    /// `state.config.settings?.toolPrefix ?? "server"` (search-ranking.ts:166).
    pub fn global_tool_prefix(&self) -> ToolPrefix {
        self.settings
            .as_ref()
            .and_then(|s| s.get("toolPrefix"))
            .and_then(ToolPrefix::from_config_value)
            .unwrap_or_default()
    }

    pub fn is_server_disabled(&self, name: &str) -> bool {
        self.mcp_servers
            .get(name)
            .is_some_and(ServerEntry::is_disabled)
    }
}

/// `resolveToolPrefix` (types.ts:678-683): per-server override beats the
/// global setting, which defaults to `server`.
pub fn resolve_tool_prefix(definition: Option<&ServerEntry>, global: ToolPrefix) -> ToolPrefix {
    definition
        .and_then(ServerEntry::tool_prefix)
        .unwrap_or(global)
}

/// `McpTool` — the adapter's deliberately smaller public surface of the SDK
/// tool wire shape (types.ts:63-69). `_meta` is P2 (MCP UI) and not modeled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
}

/// `McpResource` (types.ts:71-77, minus `_meta`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
}

/// `ToolMetadata` (types.ts:550-559, minus the P2 UI fields).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolMetadata {
    /// Prefixed tool name (e.g., `xcodebuild_list_sims`).
    pub name: String,
    /// Original MCP tool name (e.g., `list_sims`).
    pub original_name: String,
    pub description: String,
    /// For resource tools: the URI to read.
    pub resource_uri: Option<String>,
    /// JSON Schema for parameters (stored for describe/errors).
    pub input_schema: Option<Value>,
}

/// `sanitizeServerPrefix` (types.ts:734-740 @ 10a45367): provider-valid
/// `-`/`_` are preserved; every other non-alphanumeric code point becomes
/// `_x{hex}_` (lowercase hex, no padding). This is the v2.32.1 default
/// (`preserveProviderValid = true`); the legacy encoder is
/// [`sanitize_server_prefix_with`] with `preserve_provider_valid == false`.
fn sanitize_server_prefix(server_name: &str) -> String {
    sanitize_server_prefix_with(server_name, true)
}

/// `sanitizeServerPrefix(serverName, preserveProviderValid)`
/// (types.ts:734-740 @ 10a45367): `true` keeps the provider-valid `-`/`_`
/// characters (the v2.32.1 naming rules consumed by the approval glob
/// candidates), `false` is the legacy `_x{hex}_` encoder.
fn sanitize_server_prefix_with(server_name: &str, preserve_provider_valid: bool) -> String {
    let mut out = String::with_capacity(server_name.len());
    for ch in server_name.chars() {
        let valid = if preserve_provider_valid {
            ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
        } else {
            ch.is_ascii_alphanumeric()
        };
        if valid {
            out.push(ch);
        } else {
            out.push_str(&format!("_{:x}_", ch as u32));
        }
    }
    out
}

/// Strip a trailing `-?mcp` (case-insensitive) — the JS regex
/// `/-?mcp$/i` in `getServerPrefix`.
fn strip_short_suffix(name: &str) -> &str {
    let bytes = name.as_bytes();
    let ends_with_ci = |suffix: &[u8]| {
        bytes.len() >= suffix.len()
            && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    };
    if ends_with_ci(b"-mcp") {
        &name[..name.len() - 4]
    } else if ends_with_ci(b"mcp") {
        &name[..name.len() - 3]
    } else {
        name
    }
}

/// `getServerPrefix` (types.ts:741-753 @ 10a45367): provider-valid `-`/`_`
/// preserved. The legacy `_x{hex}_` encoder is only used for the legacy
/// candidate forms (`get_legacy_server_prefix`).
pub fn get_server_prefix(server_name: &str, mode: ToolPrefix) -> String {
    get_server_prefix_with(server_name, mode, true)
}

/// `getServerPrefix` with the v2.32.1 `preserveProviderValid` switch
/// (types.ts:741-753 @ 10a45367).
fn get_server_prefix_with(server_name: &str, mode: ToolPrefix, preserve: bool) -> String {
    match mode {
        ToolPrefix::None => String::new(),
        ToolPrefix::Short => {
            let short = sanitize_server_prefix_with(strip_short_suffix(server_name), preserve);
            if short.is_empty() {
                "mcp".to_string()
            } else {
                short
            }
        }
        ToolPrefix::Mcp => format!(
            "mcp__{}",
            sanitize_server_prefix_with(server_name, preserve)
        ),
        ToolPrefix::Server => sanitize_server_prefix_with(server_name, preserve),
    }
}

/// `formatToolName` (types.ts:758-766 @ 10a45367): dots in tool names become
/// `_`, then the provider-valid-preserving server prefix is prepended with a
/// `_` separator.
pub fn format_tool_name(tool_name: &str, server_name: &str, prefix: ToolPrefix) -> String {
    format_tool_name_with(tool_name, server_name, prefix, true)
}

/// `formatToolName` with the v2.32.1 `preserveProviderValid` prefix mode
/// (types.ts:758-766 @ 10a45367).
fn format_tool_name_with(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    preserve_provider_valid: bool,
) -> String {
    let p = get_server_prefix_with(server_name, prefix, preserve_provider_valid);
    let sanitized = tool_name.replace('.', "_");
    if p.is_empty() {
        sanitized
    } else {
        format!("{p}_{sanitized}")
    }
}

/// `getLegacyServerPrefix` (types.ts:830-836 @ 10a45367): the legacy
/// `_x{hex}_` server prefix per tool-prefix mode.
fn get_legacy_server_prefix(server_name: &str, mode: ToolPrefix) -> String {
    match mode {
        ToolPrefix::None => String::new(),
        ToolPrefix::Short => {
            let short = sanitize_server_prefix_with(strip_short_suffix(server_name), false);
            if short.is_empty() {
                "mcp".to_string()
            } else {
                short
            }
        }
        ToolPrefix::Mcp => format!("mcp__{}", sanitize_server_prefix_with(server_name, false)),
        ToolPrefix::Server => sanitize_server_prefix_with(server_name, false),
    }
}

/// `formatLegacyToolName` (types.ts:837-841 @ 10a45367): legacy server
/// prefix + `[.-]` → `_` in the tool name.
fn format_legacy_tool_name(tool_name: &str, server_name: &str, prefix: ToolPrefix) -> String {
    let server_prefix = get_legacy_server_prefix(server_name, prefix);
    let sanitized = tool_name.replace(['.', '-'], "_");
    if server_prefix.is_empty() {
        sanitized
    } else {
        format!("{server_prefix}_{sanitized}")
    }
}

/// `resolveServerFromToolName` (types.ts:702-723): find the longest
/// configured server prefix the tool name starts with; fail safe (None) when
/// two servers share the winning prefix or the mode is `none`.
pub fn resolve_server_from_tool_name<'a>(
    tool_name: &str,
    server_names: impl IntoIterator<Item = &'a str>,
    prefix: ToolPrefix,
) -> Option<&'a str> {
    if prefix == ToolPrefix::None {
        return None;
    }
    let mut candidates: Vec<(&str, String)> = Vec::new();
    for name in server_names {
        let p = get_server_prefix(name, prefix);
        if !p.is_empty() && tool_name.starts_with(&format!("{p}_")) {
            candidates.push((name, p));
        }
    }
    if candidates.is_empty() {
        return None;
    }
    // JS `Array.prototype.sort` is stable; Rust `sort_by_key` is stable too,
    // so equal-length ties keep server-iteration order exactly.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1.len()));
    let best = &candidates[0];
    if candidates.iter().any(|c| c.1 == best.1 && c.0 != best.0) {
        return None;
    }
    Some(best.0)
}

/// `sanitizePromptName` (types.ts:725-729).
pub fn sanitize_prompt_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    let mut last_was_sep = true; // leading runs collapse and then trim
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            cleaned.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            cleaned.push('_');
            last_was_sep = true;
        }
    }
    let cleaned = cleaned.trim_matches(['_', '-']).to_string();
    if cleaned.is_empty() {
        return "prompt".to_string();
    }
    if cleaned.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return format!("_{cleaned}");
    }
    cleaned
}

/// `formatPromptCommandName` (types.ts:731-738).
pub fn format_prompt_command_name(
    prompt_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
) -> String {
    let server_part = {
        let p = get_server_prefix(server_name, prefix);
        if p.is_empty() {
            let sanitized = sanitize_server_prefix(server_name);
            if sanitized.is_empty() {
                "server".to_string()
            } else {
                sanitized
            }
        } else {
            p
        }
    };
    format!(
        "mcp__{}__{}",
        server_part,
        sanitize_prompt_name(prompt_name)
    )
}

/// `getToolNameCandidates` (types.ts:843-868 @ 10a45367) with
/// `includeLegacy = true`: the raw upstream tool name first, then the four
/// current prefix forms (provider-valid `-`/`_` preserved), then the legacy
/// `_x{hex}_`/normalized forms. Iteration order matches the upstream Set
/// (insertion order, deduplicated).
pub fn get_tool_name_candidates(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
) -> Vec<String> {
    get_tool_name_candidates_with(tool_name, server_name, prefix, true)
}

/// `getToolNameCandidates(toolName, serverName, prefix, includeLegacy)`
/// (types.ts:843-868 @ 10a45367): insertion-ordered candidate set.
///
/// `include_legacy == false` returns the *current* candidates only — the
/// v2.32.1 naming rules (`-`/`_` preserved in the server prefix).
/// `include_legacy == true` adds the legacy `_x{hex}_`/normalized forms.
/// TE21's approval glob and TE23's include/exclude selectors both consume
/// this split via `matches_tool_selector`.
pub fn get_tool_name_candidates_with(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    include_legacy: bool,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !candidates.contains(&value) {
            candidates.push(value);
        }
    };
    push(tool_name.to_string());
    push(format_tool_name_with(tool_name, server_name, prefix, true));
    push(format_tool_name_with(
        tool_name,
        server_name,
        ToolPrefix::Server,
        true,
    ));
    push(format_tool_name_with(
        tool_name,
        server_name,
        ToolPrefix::Short,
        true,
    ));
    push(format_tool_name_with(
        tool_name,
        server_name,
        ToolPrefix::Mcp,
        true,
    ));
    if include_legacy {
        let legacy_tool_name = tool_name.replace('-', "_");
        push(legacy_tool_name.clone());
        push(format_tool_name_with(
            &legacy_tool_name,
            server_name,
            prefix,
            true,
        ));
        push(format_tool_name_with(
            &legacy_tool_name,
            server_name,
            ToolPrefix::Server,
            true,
        ));
        push(format_tool_name_with(
            &legacy_tool_name,
            server_name,
            ToolPrefix::Short,
            true,
        ));
        push(format_tool_name_with(
            &legacy_tool_name,
            server_name,
            ToolPrefix::Mcp,
            true,
        ));
        push(format_legacy_tool_name(tool_name, server_name, prefix));
        push(format_legacy_tool_name(
            tool_name,
            server_name,
            ToolPrefix::Server,
        ));
        push(format_legacy_tool_name(
            tool_name,
            server_name,
            ToolPrefix::Short,
        ));
        push(format_legacy_tool_name(
            tool_name,
            server_name,
            ToolPrefix::Mcp,
        ));
        push(format_tool_name_with(tool_name, server_name, prefix, true).replace('-', "_"));
        push(
            format_tool_name_with(tool_name, server_name, ToolPrefix::Server, true)
                .replace('-', "_"),
        );
        push(
            format_tool_name_with(tool_name, server_name, ToolPrefix::Short, true)
                .replace('-', "_"),
        );
        push(
            format_tool_name_with(tool_name, server_name, ToolPrefix::Mcp, true).replace('-', "_"),
        );
    }
    candidates
}

/// Glob match for patterns whose only metacharacters are `*` (any run,
/// including empty) and `?` (exactly one char). Equivalent to the anchored
/// regex upstream builds in `globToRegExp` (types.ts:754-757); JS `.` does
/// not match line terminators, which cannot appear in tool names.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut star_next) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            star_next = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            star_next += 1;
            ti = star_next;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `matchesToolPattern` (types.ts:894-908 @ 10a45367): exact candidate
/// matching with `*`/`?` glob support. Patterns are NOT `-`→`_` normalized
/// (the legacy normalization was removed upstream in #346); legacy shapes
/// are covered by [`get_tool_name_candidates`] instead.
pub fn matches_tool_pattern(candidates: &[String], patterns: Option<&Value>) -> bool {
    let Some(Value::Array(patterns)) = patterns else {
        return false;
    };
    if patterns.is_empty() {
        return false;
    }
    patterns.iter().any(|pattern| {
        pattern
            .as_str()
            .is_some_and(|pattern| matches_single_pattern(candidates, pattern))
    })
}

fn matches_single_pattern(candidates: &[String], pattern: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return candidates.iter().any(|candidate| candidate == pattern);
    }
    candidates
        .iter()
        .any(|candidate| glob_matches(pattern, candidate))
}

/// `ToolSelectorCandidateIndex` (types.ts:875-880 @ 10a45367): the current
/// (non-legacy) candidate names of every tool in the selector's scope.
///
/// `additional_current_candidates_by_tool_name` mirrors upstream's
/// `additionalCurrentCandidatesByToolName`: when no metadata is known for a
/// configured server, `buildToolMetadata` derives that server's candidates
/// per evaluated tool name (types.ts:912-953 / tool-metadata.ts:60-72
/// @ 10a45367).
#[derive(Debug, Default, Clone)]
pub struct ToolSelectorCandidateIndex {
    all_current_candidates: HashSet<String>,
    additional_current_candidates_by_tool_name: IndexMap<String, Vec<String>>,
}

impl ToolSelectorCandidateIndex {
    pub fn from_candidates(candidates: impl IntoIterator<Item = String>) -> Self {
        Self {
            all_current_candidates: candidates.into_iter().collect(),
            additional_current_candidates_by_tool_name: IndexMap::new(),
        }
    }

    pub fn from_candidates_with_additional(
        candidates: impl IntoIterator<Item = String>,
        additional_current_candidates_by_tool_name: IndexMap<String, Vec<String>>,
    ) -> Self {
        Self {
            all_current_candidates: candidates.into_iter().collect(),
            additional_current_candidates_by_tool_name,
        }
    }

    pub fn contains(&self, candidate: &str) -> bool {
        self.all_current_candidates.contains(candidate)
    }

    /// `hasCandidate` (types.ts:916-918 @ 10a45367): the global set or the
    /// per-tool-name additional set.
    fn has_candidate(&self, tool_name: &str, candidate: &str) -> bool {
        self.contains(candidate)
            || self
                .additional_current_candidates_by_tool_name
                .get(tool_name)
                .is_some_and(|candidates| candidates.iter().any(|value| value == candidate))
    }

    pub fn is_empty(&self) -> bool {
        self.all_current_candidates.is_empty()
            && self.additional_current_candidates_by_tool_name.is_empty()
    }
}

/// `indexHasOtherCurrentMatch` (types.ts:912-953 @ 10a45367): does `pattern`
/// match any indexed candidate outside `current_candidates`?
fn index_has_other_current_match(
    index: &ToolSelectorCandidateIndex,
    tool_name: &str,
    current_candidates: &[String],
    pattern: &str,
) -> bool {
    if !pattern.contains(['*', '?']) {
        return index.has_candidate(tool_name, pattern)
            && !current_candidates.iter().any(|c| c == pattern);
    }
    let additional = index
        .additional_current_candidates_by_tool_name
        .get(tool_name);
    let mut total = index
        .all_current_candidates
        .iter()
        .filter(|candidate| glob_matches(pattern, candidate))
        .count();
    if let Some(additional) = additional {
        total += additional
            .iter()
            .filter(|candidate| {
                !index.all_current_candidates.contains(*candidate)
                    && glob_matches(pattern, candidate)
            })
            .count();
    }
    if total == 0 {
        return false;
    }
    let current_matching = current_candidates
        .iter()
        .filter(|candidate| {
            index.has_candidate(tool_name, candidate) && glob_matches(pattern, candidate)
        })
        .count();
    total > current_matching
}

/// `matchesToolSelector` (types.ts:955-975 @ 10a45367): current candidates
/// first; without a cross-server index the full legacy candidate set is the
/// fallback; with an index a legacy-only candidate must not collide with
/// another tool's current name (the #346 collision suppression).
fn matches_tool_selector(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    patterns: Option<&Value>,
    other_current_candidates: Option<&ToolSelectorCandidateIndex>,
) -> bool {
    let Some(Value::Array(list)) = patterns else {
        return false;
    };
    if list.is_empty() {
        return false;
    }
    let current = get_tool_name_candidates_with(tool_name, server_name, prefix, false);
    if matches_tool_pattern(&current, patterns) {
        return true;
    }
    let Some(index) = other_current_candidates else {
        return matches_tool_pattern(
            &get_tool_name_candidates_with(tool_name, server_name, prefix, true),
            patterns,
        );
    };
    let legacy: Vec<String> = get_tool_name_candidates_with(tool_name, server_name, prefix, true)
        .into_iter()
        .filter(|candidate| !current.contains(candidate))
        .collect();
    list.iter().any(|pattern| {
        pattern.as_str().is_some_and(|pattern| {
            matches_single_pattern(&legacy, pattern)
                && !index_has_other_current_match(index, tool_name, &current, pattern)
        })
    })
}

/// `isToolIncluded` (types.ts:977-986 @ 10a45367).
pub fn is_tool_included(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    include_tools: Option<&Value>,
    other_current_candidates: Option<&ToolSelectorCandidateIndex>,
) -> bool {
    match include_tools {
        Some(Value::Array(list)) if !list.is_empty() => matches_tool_selector(
            tool_name,
            server_name,
            prefix,
            include_tools,
            other_current_candidates,
        ),
        _ => true,
    }
}

/// `isToolExcluded` (types.ts:988-996 @ 10a45367).
pub fn is_tool_excluded(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    exclude_tools: Option<&Value>,
    other_current_candidates: Option<&ToolSelectorCandidateIndex>,
) -> bool {
    matches_tool_selector(
        tool_name,
        server_name,
        prefix,
        exclude_tools,
        other_current_candidates,
    )
}

/// `isToolAllowed` (types.ts:998-1008 @ 10a45367): exclude wins over include.
pub fn is_tool_allowed(
    tool_name: &str,
    server_name: &str,
    prefix: ToolPrefix,
    include_tools: Option<&Value>,
    exclude_tools: Option<&Value>,
    other_current_candidates: Option<&ToolSelectorCandidateIndex>,
) -> bool {
    is_tool_included(
        tool_name,
        server_name,
        prefix,
        include_tools,
        other_current_candidates,
    ) && !is_tool_excluded(
        tool_name,
        server_name,
        prefix,
        exclude_tools,
        other_current_candidates,
    )
}

/// `resourceNameToToolName` (resource-tools.ts:3-16).
pub fn resource_name_to_tool_name(name: &str) -> String {
    let mut collapsed = String::with_capacity(name.len());
    let mut last_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            collapsed.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            collapsed.push('_');
            last_underscore = true;
        }
    }
    let result = collapsed.trim_matches('_').to_string();
    if result.is_empty() || result.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return if result.is_empty() {
            "resource".to_string()
        } else {
            format!("resource_{result}")
        };
    }
    result
}

/// Outcome of [`build_tool_metadata`] — upstream returns
/// `{ metadata, failedTools }` (tool-metadata.ts:9-82).
#[derive(Debug, Default)]
pub struct BuildToolMetadataResult {
    pub metadata: Vec<ToolMetadata>,
    pub failed_tools: Vec<String>,
}

/// `hasToolFilters` (tool-metadata.ts:23-25 @ 10a45367): a non-empty
/// `includeTools`/`excludeTools` array enables the cross-server selector
/// candidate scan.
pub fn has_tool_filters(definition: &ServerEntry) -> bool {
    [definition.include_tools(), definition.exclude_tools()]
        .into_iter()
        .any(|value| value.is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())))
}

/// `buildToolMetadata` (tool-metadata.ts:9-130 @ 10a45367).
///
/// P0 scope note: upstream also extracts `_meta` UI fields and consults
/// `isUiToolVisibleToModel`; both are MCP UI (P2 non-goal) and are skipped.
/// The unnamed-tool guard is also absent: the wire parse layer (transport
/// wave) drops nameless tools before they reach this function, so
/// `failed_tools` stays empty for now.
///
/// `configured_servers` + `known_metadata` feed the #346 cross-server
/// selector candidate index (upstream
/// `buildToolMetadata(..., configuredServers, knownMetadata)`): when this
/// server declares `includeTools`/`excludeTools`, a legacy-only selector
/// must not sweep another server's current tool name. The
/// `includeMissingConfiguredCandidates = true` startup variant is not
/// modeled (rpi reconstructs startup metadata through
/// [`crate::cache::reconstruct_tool_metadata`]).
pub fn build_tool_metadata(
    tools: &[McpTool],
    resources: &[McpResource],
    definition: &ServerEntry,
    server_name: &str,
    prefix: ToolPrefix,
    configured_servers: Option<&IndexMap<String, ServerEntry>>,
    known_metadata: Option<&IndexMap<String, Vec<ToolMetadata>>>,
) -> BuildToolMetadataResult {
    let mut result = BuildToolMetadataResult::default();
    let mut seen_names: Vec<String> = Vec::new();
    let effective_prefix = resolve_tool_prefix(Some(definition), prefix);
    let selector_candidate_index = if has_tool_filters(definition) {
        configured_servers.map(|configured_servers| {
            build_selector_candidate_index(
                tools,
                resources,
                definition,
                server_name,
                effective_prefix,
                prefix,
                configured_servers,
                known_metadata,
            )
        })
    } else {
        None
    };

    for tool in tools {
        if !is_tool_allowed(
            &tool.name,
            server_name,
            effective_prefix,
            definition.include_tools(),
            definition.exclude_tools(),
            selector_candidate_index.as_ref(),
        ) {
            continue;
        }
        let name = format_tool_name(&tool.name, server_name, effective_prefix);
        if seen_names.contains(&name) {
            continue;
        }
        seen_names.push(name.clone());
        result.metadata.push(ToolMetadata {
            name,
            original_name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: tool.input_schema.clone(),
            resource_uri: None,
        });
    }

    if definition.exposes_resources() {
        for resource in resources {
            let base_name = format!("read_{}", resource_name_to_tool_name(&resource.name));
            if !is_tool_allowed(
                &base_name,
                server_name,
                effective_prefix,
                definition.include_tools(),
                definition.exclude_tools(),
                selector_candidate_index.as_ref(),
            ) {
                continue;
            }
            let name = format_tool_name(&base_name, server_name, effective_prefix);
            if seen_names.contains(&name) {
                continue;
            }
            seen_names.push(name.clone());
            result.metadata.push(ToolMetadata {
                name,
                original_name: base_name,
                description: resource
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Read resource: {}", resource.uri)),
                resource_uri: Some(resource.uri.clone()),
                input_schema: None,
            });
        }
    }

    result
}

/// `createToolSelectorCandidateIndex` construction inside
/// `buildToolMetadata` (tool-metadata.ts:23-76 @ 10a45367): this server's
/// evaluated candidates plus every *other* configured server's known current
/// candidates.
#[allow(clippy::too_many_arguments)] // upstream-shaped index builder: the
                                     // tool surface, definition, prefix modes
                                     // and metadata maps are all explicit.
fn build_selector_candidate_index(
    tools: &[McpTool],
    resources: &[McpResource],
    definition: &ServerEntry,
    server_name: &str,
    effective_prefix: ToolPrefix,
    prefix: ToolPrefix,
    configured_servers: &IndexMap<String, ServerEntry>,
    known_metadata: Option<&IndexMap<String, Vec<ToolMetadata>>>,
) -> ToolSelectorCandidateIndex {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !candidates.contains(&value) {
            candidates.push(value);
        }
    };
    let mut evaluated_tool_names: Vec<String> = Vec::new();
    for tool in tools {
        if tool.name.is_empty() {
            continue;
        }
        evaluated_tool_names.push(tool.name.clone());
        for candidate in
            get_tool_name_candidates_with(&tool.name, server_name, effective_prefix, false)
        {
            push(candidate);
        }
    }
    if definition.exposes_resources() {
        for resource in resources {
            let base_name = format!("read_{}", resource_name_to_tool_name(&resource.name));
            evaluated_tool_names.push(base_name.clone());
            if resource.name.is_empty() || resource.uri.is_empty() {
                continue;
            }
            for candidate in
                get_tool_name_candidates_with(&base_name, server_name, effective_prefix, false)
            {
                push(candidate);
            }
        }
    }
    // Upstream `additionalCurrentCandidatesByToolName`
    // (tool-metadata.ts:60-72 @ 10a45367): when no metadata map is provided,
    // every configured server without known tools contributes its current
    // candidates for each evaluated tool name.
    let mut additional: IndexMap<String, Vec<String>> = IndexMap::new();
    for (other_server_name, other_definition) in configured_servers {
        if other_server_name == server_name {
            continue;
        }
        let other_prefix = resolve_tool_prefix(Some(other_definition), prefix);
        let known_tools = known_metadata.and_then(|map| map.get(other_server_name));
        if let Some(known_tools) = known_tools {
            for tool in known_tools {
                push(tool.name.clone());
                for candidate in get_tool_name_candidates_with(
                    &tool.original_name,
                    other_server_name,
                    other_prefix,
                    false,
                ) {
                    push(candidate);
                }
            }
            continue;
        }
        if known_metadata.is_some() {
            // `includeMissingConfiguredCandidates = false` and a known map:
            // upstream adds nothing for a server without known tools.
            continue;
        }
        for tool_name in &evaluated_tool_names {
            let entry = additional.entry(tool_name.clone()).or_default();
            for candidate in
                get_tool_name_candidates_with(tool_name, other_server_name, other_prefix, false)
            {
                if !entry.contains(&candidate) {
                    entry.push(candidate);
                }
            }
        }
    }
    ToolSelectorCandidateIndex::from_candidates_with_additional(candidates, additional)
}

/// `findToolByName` (tool-metadata.ts:154-160 @ 10a45367): exact match
/// first, then a `-` → `_` normalized comparison. Server-scoped call
/// resolution additionally matches `originalName` via
/// `crate::proxy::get_server_scoped_tool_match`.
pub fn find_tool_by_name<'a>(
    metadata: &'a [ToolMetadata],
    tool_name: &str,
) -> Option<&'a ToolMetadata> {
    if let Some(exact) = metadata.iter().find(|m| m.name == tool_name) {
        return Some(exact);
    }
    let normalized = tool_name.replace('-', "_");
    metadata
        .iter()
        .find(|m| m.name.replace('-', "_") == normalized)
}

/// `formatSchema` (tool-metadata.ts:104-137) and its helpers — the describe
/// fallback when ts-shape rendering fails.
pub fn format_schema(schema: &Value, indent: &str) -> String {
    let Some(s) = schema.as_object() else {
        return format!("{indent}(no schema)");
    };

    if s.get("type").and_then(Value::as_str) == Some("object") {
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            let required = required_set(s);
            if props.is_empty() {
                return format!("{indent}(no parameters)");
            }
            let mut lines: Vec<String> = Vec::new();
            for (name, prop_schema) in props {
                lines.extend(format_property(
                    name,
                    prop_schema,
                    required.contains(name),
                    indent,
                ));
            }
            return lines.join("\n");
        }
    }

    let lines = format_nested_schema(s, indent);
    if !lines.is_empty() {
        return lines.join("\n");
    }

    let type_str = format_type(s);
    if !type_str.is_empty() {
        return format!("{indent}({type_str})");
    }

    format!("{indent}(complex schema)")
}

fn required_set(s: &Map<String, Value>) -> Vec<String> {
    match s.get("required") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn format_property(name: &str, schema: &Value, required: bool, indent: &str) -> Vec<String> {
    let Some(s) = schema.as_object() else {
        return vec![format!(
            "{indent}{name}{}",
            if required { " *required*" } else { "" }
        )];
    };
    let mut parts = vec![format!("{indent}{name}")];
    let type_str = format_type(s);
    if !type_str.is_empty() {
        parts.push(format!("({type_str})"));
    }
    if required {
        parts.push("*required*".to_string());
    }
    append_schema_annotations(&mut parts, s);
    let mut lines = vec![parts.join(" ")];
    lines.extend(format_nested_schema(s, &format!("{indent}  ")));
    lines
}

fn format_nested_schema(schema: &Map<String, Value>, indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(Value::Array(variants)) = schema.get("anyOf") {
        lines.extend(format_variants("anyOf", variants, indent));
    }
    if let Some(Value::Array(variants)) = schema.get("oneOf") {
        lines.extend(format_variants("oneOf", variants, indent));
    }
    if let Some(items) = schema.get("items") {
        lines.extend(format_property("items", items, false, indent));
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        let required = required_set(schema);
        for (name, prop_schema) in props {
            lines.extend(format_property(
                name,
                prop_schema,
                required.contains(name),
                indent,
            ));
        }
    }
    lines
}

fn format_variants(keyword: &str, variants: &[Value], indent: &str) -> Vec<String> {
    let mut lines = vec![format!("{indent}{keyword}:")];
    for variant in variants {
        let Some(s) = variant.as_object() else {
            lines.push(format!(
                "{indent}  - {}",
                crate::utils::js_json_stringify(variant)
            ));
            continue;
        };
        let type_str = {
            let t = format_type(s);
            if t.is_empty() {
                "schema".to_string()
            } else {
                t
            }
        };
        let mut parts = vec![format!("{indent}  - {type_str}")];
        append_schema_annotations(&mut parts, s);
        lines.push(parts.join(" "));
        lines.extend(format_nested_schema(s, &format!("{indent}    ")));
    }
    lines
}

fn format_type(schema: &Map<String, Value>) -> String {
    if schema.contains_key("const") {
        return format!(
            "const {}",
            crate::utils::js_json_stringify(&schema["const"])
        );
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        let rendered: Vec<String> = values.iter().map(crate::utils::js_json_stringify).collect();
        return format!("enum: {}", rendered.join(", "));
    }
    if let Some(Value::Array(types)) = schema.get("type") {
        let rendered: Vec<String> = types.iter().map(value_to_display_string).collect();
        return rendered.join(" | ");
    }
    // JS `if (schema.type)` truthiness: null/"" do not count as a type.
    match schema.get("type") {
        Some(Value::String(s)) if !s.is_empty() => return s.clone(),
        Some(other) if !other.is_null() => return value_to_display_string(other),
        _ => {}
    }
    if schema.get("properties").is_some_and(Value::is_object) {
        return "object".to_string();
    }
    if schema.contains_key("items") {
        return "array".to_string();
    }
    String::new()
}

/// JS `String(value)` for the `type` field (always a string in practice).
fn value_to_display_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => crate::utils::js_json_stringify(other),
    }
}

fn append_schema_annotations(parts: &mut Vec<String>, schema: &Map<String, Value>) {
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        parts.push(format!("- {description}"));
    }
    for key in [
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
        "format",
        "pattern",
    ] {
        if let Some(value) = schema.get(key) {
            parts.push(format!(
                "[{key}: {}]",
                crate::utils::js_json_stringify(value)
            ));
        }
    }
    if let Some(default) = schema.get("default") {
        parts.push(format!(
            "[default: {}]",
            crate::utils::js_json_stringify(default)
        ));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Intent ports of `__tests__/resolve-server-from-tool-name.test.ts`
    // @ 10a45367 (coding-standards §12.2). The `_x{hex}_` server-prefix
    // expectations changed in #346 (provider-valid `-`/`_` preserved); the
    // legacy forms remain reachable through [`get_tool_name_candidates`].

    fn entry(map: Map<String, Value>) -> ServerEntry {
        ServerEntry(map)
    }

    #[test]
    fn format_tool_name_sanitizes_prefixes_and_dots() {
        assert_eq!(
            format_tool_name("web_search", "searxng", ToolPrefix::Server),
            "searxng_web_search"
        );
        assert_eq!(
            format_tool_name("web.search", "my server", ToolPrefix::Server),
            "my_20_server_web_search"
        );
        assert_eq!(
            format_tool_name("search", "my_server", ToolPrefix::Server),
            "my_server_search"
        );
        assert_eq!(
            format_tool_name("read.file", "fs", ToolPrefix::None),
            "read_file"
        );
        assert_eq!(
            format_tool_name("run", "my-server", ToolPrefix::Mcp),
            "mcp__my-server_run"
        );
    }

    #[test]
    fn short_prefix_strips_mcp_suffix() {
        assert_eq!(
            get_server_prefix("filesystem-mcp", ToolPrefix::Short),
            "filesystem"
        );
        assert_eq!(get_server_prefix("-mcp", ToolPrefix::Short), "mcp");
        assert_eq!(get_server_prefix("searxng", ToolPrefix::None), "");
    }

    #[test]
    fn resolve_server_round_trips_format_tool_name() {
        // upstream `keeps colliding sanitized server names distinct across mappings`.
        let spaced = format_tool_name("web_search", "my server", ToolPrefix::Server);
        assert_eq!(spaced, "my_20_server_web_search");
        assert_eq!(
            resolve_server_from_tool_name(&spaced, ["my server"], ToolPrefix::Server),
            Some("my server")
        );

        let spaced_tool = format_tool_name("search", "my server", ToolPrefix::Server);
        let underscored_tool = format_tool_name("search", "my_server", ToolPrefix::Server);
        assert_eq!(spaced_tool, "my_20_server_search");
        assert_eq!(underscored_tool, "my_server_search");
        assert_ne!(spaced_tool, underscored_tool);
        assert_eq!(
            resolve_server_from_tool_name(
                &spaced_tool,
                ["my server", "my_server"],
                ToolPrefix::Server
            ),
            Some("my server")
        );
        assert_eq!(
            resolve_server_from_tool_name(
                &underscored_tool,
                ["my server", "my_server"],
                ToolPrefix::Server
            ),
            Some("my_server")
        );
        assert_eq!(
            format_prompt_command_name("plan", "my_server", ToolPrefix::Server),
            "mcp__my_server__plan"
        );
    }

    #[test]
    fn resolve_server_picks_longest_prefix() {
        assert_eq!(
            resolve_server_from_tool_name(
                "searxng-extra_deep_search",
                ["searxng", "searxng-extra"],
                ToolPrefix::Server
            ),
            Some("searxng-extra")
        );
    }

    #[test]
    fn resolve_server_fails_safe_on_ambiguous_short_prefix() {
        assert_eq!(
            resolve_server_from_tool_name("foo_query", ["foo", "foo-mcp"], ToolPrefix::Short),
            None
        );
        assert_eq!(
            resolve_server_from_tool_name(
                "filesystem_read_file",
                ["filesystem-mcp"],
                ToolPrefix::Short
            ),
            Some("filesystem-mcp")
        );
    }

    #[test]
    fn resolve_server_requires_prefix_boundary() {
        assert_eq!(
            resolve_server_from_tool_name("notsearxng_search", ["searxng"], ToolPrefix::Server),
            None
        );
        assert_eq!(
            resolve_server_from_tool_name("searxngweb_search", ["searxng"], ToolPrefix::Server),
            None
        );
        assert_eq!(
            resolve_server_from_tool_name("searxng_search", ["searxng"], ToolPrefix::None),
            None
        );
    }

    #[test]
    fn glob_and_candidate_matching() {
        let candidates =
            get_tool_name_candidates("search_records_advanced", "demo", ToolPrefix::Server);
        assert!(candidates.contains(&"search_records_advanced".to_string()));
        assert!(candidates.contains(&"demo_search_records_advanced".to_string()));
        assert!(matches_tool_pattern(
            &candidates,
            Some(&json!(["search_*"]))
        ));
        assert!(matches_tool_pattern(&candidates, Some(&json!(["*"]))));
        assert!(!matches_tool_pattern(
            &candidates,
            Some(&json!(["other_tool"]))
        ));
        assert!(!matches_tool_pattern(
            &candidates,
            Some(&json!("not-an-array"))
        ));
        assert!(!matches_tool_pattern(&candidates, Some(&json!([]))));
    }

    // #346/#463 (types.ts:843-868 @ 10a45367): the emitted prefix preserves
    // provider-valid `-`/`_`; the legacy `_x{hex}_` escape stays reachable as
    // a fallback candidate only.
    #[test]
    fn hyphen_prefix_preserved_and_legacy_escape_still_matchable() {
        assert_eq!(get_server_prefix("a-b", ToolPrefix::Server), "a-b");
        assert_eq!(
            format_tool_name("run", "a-b", ToolPrefix::Server),
            "a-b_run"
        );
        let candidates = get_tool_name_candidates("run", "a-b", ToolPrefix::Server);
        assert!(candidates.contains(&"a-b_run".to_string()));
        assert!(candidates.contains(&"a_2d_b_run".to_string()));
        assert_ne!(
            format_tool_name("run", "a-b", ToolPrefix::Server),
            "a_2d_b_run"
        );
    }

    #[test]
    fn my_server_no_longer_generates_legacy_escape() {
        assert_eq!(
            format_tool_name("search", "my-server", ToolPrefix::Server),
            "my-server_search"
        );
        assert_ne!(
            format_tool_name("search", "my-server", ToolPrefix::Server),
            "my_2d_server_search"
        );
    }

    #[test]
    fn candidates_put_raw_tool_name_first() {
        let candidates = get_tool_name_candidates("do-thing", "demo", ToolPrefix::Server);
        assert_eq!(candidates.first().map(String::as_str), Some("do-thing"));
    }

    /// Upstream `direct tool selector candidates` cases
    /// (`__tests__/resolve-server-from-tool-name.test.ts` @ 10a45367).
    #[test]
    fn candidate_sets_keep_hyphen_and_underscore_prefixes_distinct() {
        let hyphen = get_tool_name_candidates("do_thing", "my-server", ToolPrefix::Server);
        let underscore = get_tool_name_candidates("do_thing", "my_server", ToolPrefix::Server);
        assert!(matches_tool_pattern(
            &hyphen,
            Some(&json!(["my-server_do_thing"]))
        ));
        assert!(matches_tool_pattern(
            &hyphen,
            Some(&json!(["my_server_do_thing"]))
        ));
        assert!(!matches_tool_pattern(
            &underscore,
            Some(&json!(["my-server_do_thing"]))
        ));
        assert!(matches_tool_pattern(
            &hyphen,
            Some(&json!(["my_2d_server_do_thing"]))
        ));
    }

    #[test]
    fn selector_index_suppresses_legacy_collisions() {
        let empty = ToolSelectorCandidateIndex::from_candidates(Vec::<String>::new());
        let collision =
            ToolSelectorCandidateIndex::from_candidates(["my_server_do_thing".to_string()]);
        assert!(is_tool_excluded(
            "do_thing",
            "my-server",
            ToolPrefix::Server,
            Some(&json!(["my_server_do_thing"])),
            Some(&empty)
        ));
        assert!(!is_tool_allowed(
            "do_thing",
            "my-server",
            ToolPrefix::Server,
            None,
            Some(&json!(["my_server_do_thing"])),
            Some(&empty)
        ));
        assert!(is_tool_excluded(
            "do_thing",
            "my-server",
            ToolPrefix::Server,
            Some(&json!(["my_2d_server_do_thing"])),
            Some(&empty)
        ));
        assert!(is_tool_excluded(
            "do_thing",
            "my-server",
            ToolPrefix::Server,
            Some(&json!(["my-server_do_thing"])),
            Some(&empty)
        ));
        assert!(is_tool_excluded(
            "do-thing",
            "my-server",
            ToolPrefix::Server,
            Some(&json!(["my_server_do_thing"])),
            Some(&empty)
        ));
        assert!(!is_tool_excluded(
            "do-thing",
            "my-server",
            ToolPrefix::Server,
            Some(&json!(["my_server_do_thing"])),
            Some(&collision)
        ));
    }

    /// #346 (tool-metadata.ts:23-76 @ 10a45367): the cross-server selector
    /// candidate index suppresses a legacy-only include selector that would
    /// sweep another server's current name.
    #[test]
    fn build_tool_metadata_cross_server_selector_index() {
        let tools = vec![McpTool {
            name: "do_thing".to_string(),
            ..Default::default()
        }];
        let definition = entry(Map::from_iter([(
            "includeTools".to_string(),
            json!(["my_server_do_thing"]),
        )]));
        let mut configured: IndexMap<String, ServerEntry> = IndexMap::new();
        configured.insert("my-server".to_string(), definition.clone());
        configured.insert("my_server".to_string(), entry(Map::new()));

        // Known metadata for the other server → its current name collides
        // with the legacy selector → selector suppressed.
        let mut known: IndexMap<String, Vec<ToolMetadata>> = IndexMap::new();
        known.insert(
            "my_server".to_string(),
            vec![ToolMetadata {
                name: "my_server_do_thing".to_string(),
                original_name: "do_thing".to_string(),
                ..Default::default()
            }],
        );
        let result = build_tool_metadata(
            &tools,
            &[],
            &definition,
            "my-server",
            ToolPrefix::Server,
            Some(&configured),
            Some(&known),
        );
        assert!(result.metadata.is_empty());

        // No known map → upstream derives the other server's candidates per
        // evaluated tool name (types.ts:912-953) → same collision.
        let result = build_tool_metadata(
            &tools,
            &[],
            &definition,
            "my-server",
            ToolPrefix::Server,
            Some(&configured),
            None,
        );
        assert!(result.metadata.is_empty());

        // Other server renamed → no collision → legacy selector applies.
        let mut known: IndexMap<String, Vec<ToolMetadata>> = IndexMap::new();
        known.insert(
            "my_server".to_string(),
            vec![ToolMetadata {
                name: "my_server_do_other".to_string(),
                original_name: "do_other".to_string(),
                ..Default::default()
            }],
        );
        let result = build_tool_metadata(
            &tools,
            &[],
            &definition,
            "my-server",
            ToolPrefix::Server,
            Some(&configured),
            Some(&known),
        );
        assert_eq!(result.metadata.len(), 1);

        // No filters → no index → every tool included.
        let result = build_tool_metadata(
            &tools,
            &[],
            &entry(Map::new()),
            "my-server",
            ToolPrefix::Server,
            Some(&configured),
            Some(&known),
        );
        assert_eq!(result.metadata.len(), 1);
    }

    #[test]
    fn exclude_wins_over_include() {
        assert!(is_tool_allowed(
            "search",
            "demo",
            ToolPrefix::Server,
            None,
            None,
            None
        ));
        assert!(is_tool_allowed(
            "search",
            "demo",
            ToolPrefix::Server,
            Some(&json!(["search"])),
            None,
            None
        ));
        assert!(!is_tool_allowed(
            "search",
            "demo",
            ToolPrefix::Server,
            Some(&json!(["other"])),
            None,
            None
        ));
        assert!(!is_tool_allowed(
            "search",
            "demo",
            ToolPrefix::Server,
            Some(&json!(["*"])),
            Some(&json!(["search"])),
            None
        ));
    }

    #[test]
    fn resource_names_become_tool_names() {
        assert_eq!(
            resource_name_to_tool_name("My Config File"),
            "my_config_file"
        );
        assert_eq!(resource_name_to_tool_name("__"), "resource");
        assert_eq!(
            resource_name_to_tool_name("42 answer"),
            "resource_42_answer"
        );
    }

    #[test]
    fn build_tool_metadata_prefixes_dedupes_and_exposes_resources() {
        let tools = vec![
            McpTool {
                name: "list.sims".to_string(),
                description: Some("List simulators".to_string()),
                input_schema: Some(json!({"type": "object"})),
            },
            // Duplicate after sanitization: dropped.
            McpTool {
                name: "list_sims".to_string(),
                description: None,
                input_schema: None,
            },
        ];
        let resources = vec![McpResource {
            uri: "file:///tmp/x".to_string(),
            name: "Config".to_string(),
            description: None,
        }];
        let definition = entry(Map::new());
        let result = build_tool_metadata(
            &tools,
            &resources,
            &definition,
            "xcodebuild",
            ToolPrefix::Server,
            None,
            None,
        );
        let names: Vec<&str> = result.metadata.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["xcodebuild_list_sims", "xcodebuild_read_config"]);
        assert_eq!(
            result.metadata[1].description,
            "Read resource: file:///tmp/x"
        );
        assert_eq!(
            result.metadata[1].resource_uri.as_deref(),
            Some("file:///tmp/x")
        );

        let no_resources = entry(Map::from_iter([(
            "exposeResources".to_string(),
            json!(false),
        )]));
        let result = build_tool_metadata(
            &tools,
            &resources,
            &no_resources,
            "xcodebuild",
            ToolPrefix::Server,
            None,
            None,
        );
        assert_eq!(result.metadata.len(), 1);
    }

    #[test]
    fn find_tool_by_name_normalizes_hyphens() {
        let metadata = vec![ToolMetadata {
            name: "demo_search_records".to_string(),
            ..Default::default()
        }];
        assert!(find_tool_by_name(&metadata, "demo_search_records").is_some());
        assert!(find_tool_by_name(&metadata, "demo-search-records").is_some());
        assert!(find_tool_by_name(&metadata, "demo_other").is_none());
    }

    // Intent ports of `__tests__/tool-metadata.test.ts` @ 3d953f90
    // (formatSchema golden strings).

    #[test]
    fn format_schema_keeps_simple_object_schemas_compact() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search term", "default": "all" },
                "limit": { "type": ["number", "null"] },
                "mode": { "enum": ["fast", "safe"] },
            },
            "required": ["query"],
        });
        assert_eq!(
            format_schema(&schema, "  "),
            [
                "  query (string) *required* - Search term [default: \"all\"]",
                "  limit (number | null)",
                "  mode (enum: \"fast\", \"safe\")",
            ]
            .join("\n")
        );
    }

    #[test]
    fn format_schema_expands_union_branches_with_const_discriminators() {
        let schema = json!({
            "type": "object",
            "properties": {
                "document": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "text" },
                                "content": { "type": "string", "minLength": 1 },
                            },
                            "required": ["type", "content"],
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "file" },
                                "path": { "type": "string", "minLength": 1 },
                            },
                            "required": ["type", "path"],
                        },
                    ],
                },
            },
            "required": ["document"],
        });
        assert_eq!(
            format_schema(&schema, "  "),
            [
                "  document *required*",
                "    anyOf:",
                "      - object",
                "        type (const \"text\") *required*",
                "        content (string) *required* [minLength: 1]",
                "      - object",
                "        type (const \"file\") *required*",
                "        path (string) *required* [minLength: 1]",
            ]
            .join("\n")
        );
    }

    #[test]
    fn format_schema_formats_one_of_branches() {
        let schema = json!({
            "type": "object",
            "properties": {
                "target": { "oneOf": [{ "const": "draft" }, { "const": "published" }] },
            },
        });
        assert_eq!(
            format_schema(&schema, "  "),
            [
                "  target",
                "    oneOf:",
                "      - const \"draft\"",
                "      - const \"published\"",
            ]
            .join("\n")
        );
    }

    #[test]
    fn format_schema_formats_nested_objects_and_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "tags": {
                            "type": "array",
                            "items": { "enum": ["alpha", "beta"] },
                            "minItems": 1,
                        },
                    },
                    "required": ["enabled"],
                },
            },
            "required": ["config"],
        });
        assert_eq!(
            format_schema(&schema, "  "),
            [
                "  config (object) *required*",
                "    enabled (boolean) *required*",
                "    tags (array) [minItems: 1]",
                "      items (enum: \"alpha\", \"beta\")",
            ]
            .join("\n")
        );
    }
}
