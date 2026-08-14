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

/// `scoreToolMatch` (search-ranking.ts:67-157). `None` means "no match".
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

    let fields: [(i64, String); 4] = [
        (WEIGHT_NAME, normalize_search_text(&tool.name)),
        (
            WEIGHT_ORIGINAL_NAME,
            normalize_search_text(&tool.original_name),
        ),
        (WEIGHT_SERVER, normalize_search_text(server)),
        (WEIGHT_DESCRIPTION, normalize_search_text(&tool.description)),
    ];
    let mut score: i64 = 0;
    let mut phrase_matched = false;
    let mut whole_field_exact = false;
    let mut matched_tokens: Vec<&String> = Vec::new();

    for (weight, value) in &fields {
        let field_tokens = tokenize(value);
        if *value == normalized_query {
            score += weight * 14;
            phrase_matched = true;
            whole_field_exact = true;
        } else if starts_with(value, &normalized_query) {
            score += weight * 9;
            phrase_matched = true;
        } else if value.contains(&normalized_query) {
            score += weight * 6;
            phrase_matched = true;
        }

        for token in &query_tokens {
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
    if let Some(keywords) = keywords.filter(|k| !k.is_empty()) {
        let weight = WEIGHT_KEYWORDS;
        let phrases: Vec<String> = keywords
            .iter()
            .map(|k| normalize_search_text(k).trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        let mut phrase_score = 0;
        for phrase in &phrases {
            if *phrase == normalized_query {
                phrase_score = phrase_score.max(weight * 14);
                phrase_matched = true;
                whole_field_exact = true;
            } else if starts_with(phrase, &normalized_query) {
                phrase_score = phrase_score.max(weight * 9);
                phrase_matched = true;
            } else if phrase.contains(&normalized_query) {
                phrase_score = phrase_score.max(weight * 6);
                phrase_matched = true;
            }
        }
        score += phrase_score;

        let keyword_tokens: Vec<String> = phrases.iter().flat_map(|p| tokenize(p)).collect();
        for token in &query_tokens {
            if keyword_tokens.contains(token) {
                score += weight * 4;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if keyword_tokens.iter().any(|kt| {
                starts_with(kt, token) || (kt.len() >= MIN_STEM_LENGTH && starts_with(token, kt))
            }) {
                score += weight * 2;
                if !matched_tokens.contains(&token) {
                    matched_tokens.push(token);
                }
            } else if phrases.iter().any(|p| p.contains(token)) {
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
        if tokenize(&tool.name).contains(first) {
            score += 8;
        }
    }
    if whole_field_exact {
        score += 20;
    }
    Some(score)
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

/// `rankToolMatches` (search-ranking.ts:159-181).
pub fn rank_tool_matches(
    state: &SearchState,
    query: &str,
    server: Option<&str>,
    include_keywords: bool,
) -> Vec<RankedToolMatch> {
    let mut matches = Vec::new();
    let global_prefix = state.config.global_tool_prefix();
    for (server_name, metadata) in state.tool_metadata {
        if let Some(server) = server {
            if server_name != server {
                continue;
            }
        }
        let definition = state.config.mcp_servers.get(server_name);
        if definition.is_some_and(ServerEntry::is_disabled) {
            continue;
        }
        let has_keywords =
            include_keywords && definition.and_then(ServerEntry::search_keywords).is_some();
        for tool in metadata {
            let keywords = if has_keywords {
                resolve_search_keywords(definition, &tool.original_name, server_name, global_prefix)
            } else {
                Vec::new()
            };
            let keywords_ref = if has_keywords {
                Some(keywords.as_slice())
            } else {
                None
            };
            if let Some(score) = score_tool_match(tool, server_name, query, keywords_ref) {
                matches.push(RankedToolMatch {
                    server: server_name.clone(),
                    tool: tool.clone(),
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

/// `rankSuggestions` (search-ranking.ts:197-206): strip the longest matching
/// server prefix (any of server/short/mcp forms) from the requested name,
/// then rank the remainder without keyword boosts.
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
    rank_tool_matches(state, query, None, false)
        .into_iter()
        .take(limit)
        .map(|m| m.tool.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
