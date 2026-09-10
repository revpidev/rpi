//! Search scoring, ranking, pagination and suggestions (FR-P0-04 pure part).
//!
//! Port of `search-ranking.ts` @ pi-mcp-adapter v2.24.0 (3d953f90):
//! `normalizeSearchText` / `tokenize` / `scoreToolMatch` / `rankToolMatches`
//! / `paginate` / `rankSuggestions` / `resolveSearchKeywords`. The scoring
//! weights are a behavioral parity surface — do not "improve" them.
//!
//! Intentional differences:
//! - Tie-break ordering uses Rust byte-wise string compare where upstream
//!   uses `localeCompare`; for sanitized tool names (ASCII) the two agree.
//! - The regex search mode itself lives in `proxy.rs` (upstream
//!   `executeSearch`); this module is the pure scoring/pagination layer.

use std::collections::HashSet;

use crate::metadata::{
    get_server_prefix, get_tool_name_candidates, matches_tool_pattern, resolve_tool_prefix,
    McpConfig, ServerEntry, ToolMetadata, ToolPrefix,
};
use serde_json::Value;

/// Shortest field token allowed to stem-match a longer query token
/// (search-ranking.ts:6-10).
const MIN_STEM_LENGTH: usize = 4;

const WEIGHT_NAME: i64 = 12;
const WEIGHT_ORIGINAL_NAME: i64 = 10;
const WEIGHT_SERVER: i64 = 8;
const WEIGHT_DESCRIPTION: i64 = 5;
const WEIGHT_KEYWORDS: i64 = 5;

/// A scored match (search-ranking.ts:20-24).
#[derive(Debug, Clone, PartialEq)]
pub struct RankedToolMatch {
    pub server: String,
    pub tool: ToolMetadata,
    pub score: i64,
}

/// The slice of adapter state the pure ranking functions need
/// (`McpExtensionState.config` + `toolMetadata`, search-ranking.ts:159-167).
/// `tool_metadata` iterates in insertion order like the upstream `Map`.
pub struct SearchState<'a> {
    pub config: &'a McpConfig,
    pub tool_metadata: &'a [(String, Vec<ToolMetadata>)],
    /// Servers in active failure backoff (failure-backoff.ts:18-23): their
    /// tools are skipped by ranking so a failed server is never advertised
    /// (search-ranking.ts:246 @ 10a45367, R7.2.3.1/#434).
    pub unavailable_servers: &'a HashSet<String>,
}

/// `normalizeSearchText` (search-ranking.ts:56-60): split camelCase
/// boundaries, collapse `[_./:-]` runs to one space, lowercase.
pub fn normalize_search_text(value: &str) -> String {
    let mut spaced = String::with_capacity(value.len() + 8);
    let mut prev: Option<char> = None;
    for ch in value.chars() {
        if let Some(p) = prev {
            // /([a-z0-9])([A-Z])/g -> "$1 $2"
            if (p.is_ascii_lowercase() || p.is_ascii_digit()) && ch.is_ascii_uppercase() {
                spaced.push(' ');
            }
        }
        spaced.push(ch);
        prev = Some(ch);
    }
    // /[_./:-]+/g -> " "
    let mut collapsed = String::with_capacity(spaced.len());
    let mut in_run = false;
    for ch in spaced.chars() {
        if matches!(ch, '_' | '.' | '/' | ':' | '-') {
            if !in_run {
                collapsed.push(' ');
            }
            in_run = true;
        } else {
            collapsed.push(ch);
            in_run = false;
        }
    }
    collapsed.to_lowercase()
}

/// `tokenize` (search-ranking.ts:63-65): split on non-`[a-z0-9]` runs.
pub fn tokenize(value: &str) -> Vec<String> {
    normalize_search_text(value)
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn starts_with(haystack: &str, needle: &str) -> bool {
    haystack.starts_with(needle)
}

// Test-only preparation counter (TE25 FR-A R3/A2: "no repeated
// normalization" is asserted by call counts). Thread-local so parallel
// unit tests stay isolated; compiled out of production builds.
#[cfg(test)]
thread_local! {
    static PREPARED_TOOL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_prepared_tool() {
    PREPARED_TOOL_CALLS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn prepared_tool_calls() -> usize {
    PREPARED_TOOL_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_prepared_tool_calls() {
    PREPARED_TOOL_CALLS.with(|count| count.set(0));
}

/// Normalized name/description/... fields plus keyword tokens, prepared once
/// per tool and reused across every query in a catalog
/// (search-ranking.ts:26-36 @ 10a45367 `PreparedToolSearch`).
struct PreparedToolSearch<'a> {
    tool: &'a ToolMetadata,
    /// `[(weight, normalized_value, tokens)]` for name / originalName /
    /// server / description, in upstream field order.
    fields: [(i64, String, Vec<String>); 4],
    keyword_phrases: Vec<String>,
    keyword_tokens: Vec<String>,
}

/// Per-server prepared slice (search-ranking.ts:43-51 @ 10a45367
/// `CachedServerSearch`).
struct PreparedServerSearch<'a> {
    server_name: &'a str,
    tools: Vec<PreparedToolSearch<'a>>,
}

/// A prepared tool catalog (the rpi equivalent of upstream's per-state
/// `WeakMap` cache): built from one `SearchState` snapshot and reused across
/// every ranking call that runs against it — most notably the repeated
/// rankings inside [`rank_suggestions`]. A snapshot is cloned per call, so
/// the reuse boundary is the snapshot, exactly like upstream's cache keys
/// on the live state object.
pub(crate) struct PreparedSearchCatalog<'a> {
    servers: Vec<PreparedServerSearch<'a>>,
}

impl<'a> PreparedSearchCatalog<'a> {
    /// Prepare every tool of the snapshot once (`getPreparedTools`,
    /// search-ranking.ts:199-229 @ 10a45367). Keywords are resolved only
    /// when the server declares `searchKeywords` and the caller wants the
    /// keyword boost.
    fn prepare(state: &SearchState<'a>, include_keywords: bool) -> Self {
        let global_prefix = state.config.global_tool_prefix();
        let servers = state
            .tool_metadata
            .iter()
            .map(|(server_name, metadata)| {
                let definition = state.config.mcp_servers.get(server_name);
                let keywords_enabled =
                    include_keywords && definition.and_then(ServerEntry::search_keywords).is_some();
                let tools = metadata
                    .iter()
                    .map(|tool| {
                        let keywords = if keywords_enabled {
                            resolve_search_keywords(
                                definition,
                                &tool.original_name,
                                server_name,
                                global_prefix,
                            )
                        } else {
                            Vec::new()
                        };
                        let keywords_ref = if keywords_enabled {
                            Some(keywords.as_slice())
                        } else {
                            None
                        };
                        prepare_tool_search(tool, server_name, keywords_ref)
                    })
                    .collect();
                PreparedServerSearch {
                    server_name: server_name.as_str(),
                    tools,
                }
            })
            .collect();
        Self { servers }
    }
}

/// `prepareToolSearch` (search-ranking.ts:87-105 @ 10a45367).
fn prepare_tool_search<'a>(
    tool: &'a ToolMetadata,
    server: &str,
    keywords: Option<&[String]>,
) -> PreparedToolSearch<'a> {
    #[cfg(test)]
    note_prepared_tool();
    let name = normalize_search_text(&tool.name);
    let original_name = normalize_search_text(&tool.original_name);
    let server = normalize_search_text(server);
    let description = normalize_search_text(&tool.description);
    let keyword_phrases: Vec<String> = keywords
        .unwrap_or(&[])
        .iter()
        .map(|keyword| normalize_search_text(keyword).trim().to_string())
        .filter(|phrase| !phrase.is_empty())
        .collect();
    let keyword_tokens: Vec<String> = keyword_phrases.iter().flat_map(|p| tokenize(p)).collect();
    let fields = [
        (WEIGHT_NAME, name.clone(), tokenize(&name)),
        (
            WEIGHT_ORIGINAL_NAME,
            original_name.clone(),
            tokenize(&original_name),
        ),
        (WEIGHT_SERVER, server.clone(), tokenize(&server)),
        (
            WEIGHT_DESCRIPTION,
            description.clone(),
            tokenize(&description),
        ),
    ];
    PreparedToolSearch {
        tool,
        fields,
        keyword_phrases,
        keyword_tokens,
    }
}

/// `scorePreparedToolMatch` (search-ranking.ts:107-197 @ 10a45367).
fn score_prepared_tool_match(
    prepared: &PreparedToolSearch<'_>,
    normalized_query: &str,
    query_tokens: &[String],
) -> Option<i64> {
    let mut score: i64 = 0;
    let mut phrase_matched = false;
    let mut whole_field_exact = false;
    let mut matched_tokens: Vec<&String> = Vec::new();

    for (weight, value, field_tokens) in &prepared.fields {
        if value == normalized_query {
            score += weight * 14;
            phrase_matched = true;
            whole_field_exact = true;
        } else if starts_with(value, normalized_query) {
            score += weight * 9;
            phrase_matched = true;
        } else if value.contains(normalized_query) {
            score += weight * 6;
            phrase_matched = true;
        }

        for token in query_tokens {
            if field_tokens.contains(token) {
                score += weight * 4;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if field_tokens.iter().any(|ft| {
                starts_with(ft, token) || (ft.len() >= MIN_STEM_LENGTH && starts_with(token, ft))
            }) {
                score += weight * 2;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if value.contains(token) {
                score += weight;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            }
        }
    }

    // Configured keywords are discrete phrases: the phrase-level bonus takes
    // the best single phrase (search-ranking.ts:111-114).
    if !prepared.keyword_phrases.is_empty() {
        let weight = WEIGHT_KEYWORDS;
        let mut phrase_score = 0;
        for phrase in &prepared.keyword_phrases {
            if phrase == normalized_query {
                phrase_score = phrase_score.max(weight * 14);
                phrase_matched = true;
                whole_field_exact = true;
            } else if starts_with(phrase, normalized_query) {
                phrase_score = phrase_score.max(weight * 9);
                phrase_matched = true;
            } else if phrase.contains(normalized_query) {
                phrase_score = phrase_score.max(weight * 6);
                phrase_matched = true;
            }
        }
        score += phrase_score;

        for token in query_tokens {
            if prepared.keyword_tokens.contains(token) {
                score += weight * 4;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if prepared.keyword_tokens.iter().any(|kt| {
                starts_with(kt, token) || (kt.len() >= MIN_STEM_LENGTH && starts_with(token, kt))
            }) {
                score += weight * 2;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if prepared.keyword_phrases.iter().any(|p| p.contains(token)) {
                score += weight;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            }
        }
    }

    let coverage = matched_tokens.len() as f64 / query_tokens.len() as f64;
    let coverage_gate_failed = if query_tokens.len() <= 2 {
        (coverage - 1.0).abs() > f64::EPSILON
    } else {
        coverage < 0.6
    };
    if !phrase_matched && coverage_gate_failed {
        return None;
    }

    score += if (coverage - 1.0).abs() <= f64::EPSILON {
        25
    } else {
        // JS Math.round for non-negative values is floor(x + 0.5).
        (coverage * 10.0 + 0.5).floor() as i64
    };
    if let Some(first) = query_tokens.first() {
        // Upstream `prepared.nameTokens` is `preparedFields[0].tokens`
        // (search-ranking.ts:102/188); `normalizeSearchText` is idempotent,
        // so the prepared name tokens equal `tokenize(tool.name)`.
        if prepared.fields[0].2.contains(first) {
            score += 8;
        }
    }
    if whole_field_exact {
        score += 20;
    }
    Some(score)
}

/// `scoreToolMatch` (search-ranking.ts:67-157): single-tool wrapper over
/// [`prepare_tool_search`] + [`score_prepared_tool_match`].
pub fn score_tool_match(
    tool: &ToolMetadata,
    server: &str,
    query: &str,
    keywords: Option<&[String]>,
) -> Option<i64> {
    let normalized_query = normalize_search_text(query).trim().to_string();
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return None;
    }
    score_prepared_tool_match(
        &prepare_tool_search(tool, server, keywords),
        &normalized_query,
        &query_tokens,
    )
}

/// `resolveSearchKeywords` (search-ranking.ts:31-54): keys match by original
/// name, prefixed name or glob; all matching entries union, first-wins
/// dedupe of trimmed values.
pub fn resolve_search_keywords(
    definition: Option<&ServerEntry>,
    tool_original_name: &str,
    server_name: &str,
    global_prefix: ToolPrefix,
) -> Vec<String> {
    let Some(map) = definition.and_then(ServerEntry::search_keywords) else {
        return Vec::new();
    };
    let effective_prefix = resolve_tool_prefix(definition, global_prefix);
    let candidates = get_tool_name_candidates(tool_original_name, server_name, effective_prefix);
    let mut keywords: Vec<String> = Vec::new();
    for (pattern, values) in map {
        let Value::Array(values) = values else {
            continue;
        };
        if !matches_tool_pattern(
            &candidates,
            Some(&Value::Array(vec![Value::String(pattern.clone())])),
        ) {
            continue;
        }
        for value in values {
            let Some(trimmed) = value.as_str().map(str::trim) else {
                continue;
            };
            if trimmed.is_empty() || keywords.iter().any(|k| k == trimmed) {
                continue;
            }
            keywords.push(trimmed.to_string());
        }
    }
    keywords
}

/// `rankToolMatches` (search-ranking.ts:231-260 @ 10a45367): prepare the
/// snapshot's tool fields once, then score every prepared tool.
pub fn rank_tool_matches(
    state: &SearchState,
    query: &str,
    server: Option<&str>,
    include_keywords: bool,
) -> Vec<RankedToolMatch> {
    let catalog = PreparedSearchCatalog::prepare(state, include_keywords);
    rank_prepared_matches(&catalog, state, query, server)
}

/// Ranking over an already prepared catalog: reused by [`rank_suggestions`]
/// so the tool fields are normalized/tokenized once for every suggestion
/// query (#392).
fn rank_prepared_matches(
    catalog: &PreparedSearchCatalog<'_>,
    state: &SearchState<'_>,
    query: &str,
    server: Option<&str>,
) -> Vec<RankedToolMatch> {
    let mut matches = Vec::new();
    let normalized_query = normalize_search_text(query).trim().to_string();
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return matches;
    }
    for prepared_server in &catalog.servers {
        if let Some(server) = server {
            if prepared_server.server_name != server {
                continue;
            }
        }
        // search-ranking.ts:246: skip servers in active failure backoff.
        if state
            .unavailable_servers
            .contains(prepared_server.server_name)
        {
            continue;
        }
        if state
            .config
            .mcp_servers
            .get(prepared_server.server_name)
            .is_some_and(ServerEntry::is_disabled)
        {
            continue;
        }
        for prepared in &prepared_server.tools {
            if let Some(score) =
                score_prepared_tool_match(prepared, &normalized_query, &query_tokens)
            {
                matches.push(RankedToolMatch {
                    server: prepared_server.server_name.to_string(),
                    tool: prepared.tool.clone(),
                    score,
                });
            }
        }
    }
    // Stable sort (JS sort is stable): score desc, then name ascending.
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.tool.name.cmp(&b.tool.name))
    });
    matches
}

/// `paginate` result (search-ranking.ts:183-195).
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

/// `paginate` (search-ranking.ts:183-195). Callers pass already-validated
/// numbers; negative/overflowing inputs clamp like `Math.max(0, trunc(...))`
/// / `Math.max(1, trunc(...))`.
pub fn paginate<T: Clone>(items: &[T], offset: i64, limit: i64) -> Page<T> {
    let safe_offset = usize::try_from(offset).unwrap_or(0);
    let safe_limit = usize::try_from(limit).unwrap_or(1).max(1);
    let total = items.len();
    let page: Vec<T> = items
        .iter()
        .skip(safe_offset)
        .take(safe_limit)
        .cloned()
        .collect();
    let next_offset = safe_offset + page.len();
    Page {
        items: page,
        total,
        has_more: next_offset < total,
        next_offset: if next_offset < total {
            Some(next_offset)
        } else {
            None
        },
    }
}

/// `rankSuggestions` (search-ranking.ts:279-288 @ 10a45367): strip the
/// longest matching server prefix (any of server/short/mcp forms) from the
/// requested name, then rank the remainder without keyword boosts. The
/// prepared catalog is built once and reused by every candidate ranking
/// (#392).
pub fn rank_suggestions(state: &SearchState, name: &str, limit: usize) -> Vec<String> {
    let mut stripped: Vec<String> = Vec::new();
    for server in state.config.mcp_servers.keys() {
        for prefix in [ToolPrefix::Server, ToolPrefix::Short, ToolPrefix::Mcp] {
            let candidate = get_server_prefix(server, prefix);
            if !candidate.is_empty() && name.starts_with(&format!("{candidate}_")) {
                stripped.push(candidate);
            }
        }
    }
    // Stable, longest-prefix-first (search-ranking.ts:201).
    stripped.sort_by_key(|candidate| std::cmp::Reverse(candidate.len()));
    let query = match stripped.first() {
        Some(candidate) => &name[candidate.len() + 1..],
        None => name,
    };
    let catalog = PreparedSearchCatalog::prepare(state, false);
    rank_prepared_matches(&catalog, state, query, None)
        .into_iter()
        .take(limit)
        .map(|m| m.tool.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    // Intent ports of `__tests__/search-ranking.test.ts` @ 3d953f90
    // (coding-standards §12.2).

    fn tool(name: &str, description: &str) -> ToolMetadata {
        ToolMetadata {
            name: name.to_string(),
            original_name: name.to_string(),
            description: description.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn ranks_exact_name_above_description_match() {
        let exact = score_tool_match(
            &tool("search_records", "Find records"),
            "demo",
            "search",
            None,
        );
        let description = score_tool_match(
            &tool("find_records", "Search records"),
            "demo",
            "search",
            None,
        );
        assert!(exact > description);
    }

    #[test]
    fn drops_partial_two_token_matches() {
        assert_eq!(
            score_tool_match(
                &tool("search_records", "Find records"),
                "demo",
                "search missing",
                None
            ),
            None
        );
    }

    #[test]
    fn ignores_single_letter_possessive_tokens_instead_of_stem_matching() {
        assert_eq!(
            score_tool_match(
                &tool("sync_icon", "Add an icon to your project's icons file."),
                "better-icons",
                "simulator",
                None
            ),
            None
        );
        assert!(score_tool_match(
            &tool("sync_icon", "Sync an icon."),
            "better-icons",
            "synchronize",
            None
        )
        .is_some());
    }

    #[test]
    fn matches_through_configured_keywords_where_query_would_miss() {
        let advanced = tool(
            "search_records_advanced",
            "Advanced record search with filters",
        );
        assert_eq!(
            score_tool_match(&advanced, "demo", "fuzzy lookup", None),
            None
        );
        assert!(score_tool_match(
            &advanced,
            "demo",
            "fuzzy lookup",
            Some(&["fuzzy lookup".to_string(), "legacy".to_string()])
        )
        .is_some());
        assert_eq!(score_tool_match(&advanced, "demo", "fuzzy", None), None);
        assert!(score_tool_match(
            &advanced,
            "demo",
            "fuzzy",
            Some(&["fuzzy lookup".to_string()])
        )
        .is_some());
    }

    #[test]
    fn ranks_exact_keyword_alias_above_description_phrase_match() {
        let aliased = score_tool_match(
            &tool(
                "search_records_advanced",
                "Advanced record search with filters",
            ),
            "demo",
            "fuzzy lookup",
            Some(&["fuzzy lookup".to_string()]),
        );
        let description = score_tool_match(
            &tool("record_search", "Fuzzy lookup across records"),
            "demo",
            "fuzzy lookup",
            None,
        );
        assert!(aliased > description);
    }

    #[test]
    fn scores_exact_alias_above_cross_phrase_token_matches() {
        let advanced = tool(
            "search_records_advanced",
            "Advanced record search with filters",
        );
        let keywords = vec!["fuzzy lookup".to_string(), "legacy".to_string()];
        let exact = score_tool_match(&advanced, "demo", "fuzzy lookup", Some(&keywords));
        let cross_phrase = score_tool_match(&advanced, "demo", "lookup legacy", Some(&keywords));
        assert!(exact > cross_phrase);
    }

    #[test]
    fn empty_keyword_list_does_not_change_scoring() {
        let advanced = tool("search_records_advanced", "Advanced record search");
        assert_eq!(
            score_tool_match(&advanced, "demo", "advanced", Some(&[])),
            score_tool_match(&advanced, "demo", "advanced", None)
        );
    }

    #[test]
    fn paginates_including_offsets_beyond_the_result_set() {
        let items = vec!["a", "b", "c"];
        let page = paginate(&items, 1, 1);
        assert_eq!(page.items, vec!["b"]);
        assert_eq!(page.total, 3);
        assert!(page.has_more);
        assert_eq!(page.next_offset, Some(2));

        let page = paginate(&items, 5, 1);
        assert!(page.items.is_empty());
        assert_eq!(page.total, 3);
        assert!(!page.has_more);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn normalize_and_tokenize_split_camel_and_separators() {
        assert_eq!(
            normalize_search_text("searchRecords.advanced-mode"),
            "search records advanced mode"
        );
        assert_eq!(tokenize("list_sims"), vec!["list", "sims"]);
        assert_eq!(tokenize("a:b/c"), vec!["a", "b", "c"]);
    }

    /// TE25 FR-A R2 (#392 5d9e382): normalized fields and keyword tokens
    /// are prepared once per tool per ranking call (counted — not timed:
    /// see `prepared_tool_calls`).
    #[test]
    fn prepared_catalog_prepares_each_tool_once_per_ranking_call() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({
                    "command": "demo",
                    "searchKeywords": { "search_*": ["fuzzy lookup"] },
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
        );
        let metadata = vec![(
            "demo".to_string(),
            vec![
                tool("search_records", "Find records"),
                tool("create_record", "Create a record"),
                tool("list_sims", "List sims"),
            ],
        )];
        let unavailable = HashSet::new();
        let state = SearchState {
            config: &config,
            tool_metadata: &metadata,
            unavailable_servers: &unavailable,
        };

        reset_prepared_tool_calls();
        let matches = rank_tool_matches(&state, "record", None, true);
        assert!(!matches.is_empty());
        assert_eq!(prepared_tool_calls(), 3);

        // Suggestions build one prepared catalog for the single stripped
        // query (no per-suggestion re-preparation).
        reset_prepared_tool_calls();
        let suggestions = rank_suggestions(&state, "demo_search_records", 5);
        assert!(suggestions.contains(&"search_records".to_string()));
        assert_eq!(prepared_tool_calls(), 3);
    }

    /// TE25 FR-A R2 (#392 upstream test "refreshes prepared fields when
    /// catalog or keyword references change"): prepared data never outlives
    /// its snapshot, so keyword/catalog edits are visible on the next call.
    #[test]
    fn prepared_fields_refresh_when_keywords_or_catalog_change() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({
                    "command": "demo",
                    "searchKeywords": { "search_records": ["fuzzy"] },
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
        );
        let metadata = vec![(
            "demo".to_string(),
            vec![tool("search_records", "Find records")],
        )];
        let unavailable = HashSet::new();
        let state = SearchState {
            config: &config,
            tool_metadata: &metadata,
            unavailable_servers: &unavailable,
        };
        assert!(rank_tool_matches(&state, "fuzzy", None, true).len() == 1);

        config.mcp_servers.get_mut("demo").map(|definition| {
            definition.as_map_mut().insert(
                "searchKeywords".to_string(),
                json!({ "search_records": ["semantic"] }),
            )
        });
        let state = SearchState {
            config: &config,
            tool_metadata: &metadata,
            unavailable_servers: &unavailable,
        };
        assert_eq!(rank_tool_matches(&state, "fuzzy", None, true).len(), 0);
        assert_eq!(rank_tool_matches(&state, "semantic", None, true).len(), 1);

        // Catalog swap refreshes the prepared fields too.
        let swapped = vec![(
            "demo".to_string(),
            vec![tool("create_record", "Create a record")],
        )];
        let state = SearchState {
            config: &config,
            tool_metadata: &swapped,
            unavailable_servers: &unavailable,
        };
        assert_eq!(rank_tool_matches(&state, "search", None, true).len(), 0);
        assert_eq!(rank_tool_matches(&state, "create", None, true).len(), 1);
    }
}
