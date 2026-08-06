//! Port of `session-selector-search.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The upstream `SortMode` / `NameFilter` string unions become enums;
//!   `ParsedSearchQuery` mirrors the upstream interface with a Rust
//!   [`regex::Regex`] instead of a JS `RegExp`.
//! - Match scores use char indices where upstream uses JS UTF-16 code-unit
//!   indices (`text.search(...)` / `indexOf(...)`); scores are relative
//!   ordering hints only, so both spaces rank identically.
//! - `filterAndSortSessions` takes `Vec<SessionInfo>` by value (the caller
//!   owns the list); upstream sorts/filters in place. The `cwd` search
//!   dimension enters through `SessionInfo.cwd`, which is part of the search
//!   text (session-selector-search.ts:26-28) — there is no separate `cwd`
//!   parameter upstream.

use pir_tui::fuzzy::fuzzy_match;
use regex::{Regex, RegexBuilder};

use crate::core::session_manager::SessionInfo;

/// `SortMode` (session-selector-search.ts:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Threaded,
    Recent,
    Relevance,
}

/// `NameFilter` (session-selector-search.ts:6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFilter {
    All,
    Named,
}

/// `ParsedSearchQuery["mode"]` (session-selector-search.ts:8-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Tokens,
    Regex,
}

/// `{ kind: "fuzzy" | "phrase"; value: string }` token
/// (session-selector-search.ts:10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchToken {
    Fuzzy(String),
    Phrase(String),
}

/// `ParsedSearchQuery` (session-selector-search.ts:8-14).
#[derive(Debug, Clone)]
pub struct ParsedSearchQuery {
    pub mode: QueryMode,
    pub tokens: Vec<SearchToken>,
    pub regex: Option<Regex>,
    /// If set, parsing failed and the query should be treated as
    /// non-matching.
    pub error: Option<String>,
}

/// `MatchResult` (session-selector-search.ts:16-20).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchResult {
    pub matches: bool,
    /// Lower is better; only meaningful when `matches` is true.
    pub score: f64,
}

/// `normalizeWhitespaceLower` (session-selector-search.ts:22-24).
fn normalize_whitespace_lower(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `getSessionSearchText` (session-selector-search.ts:26-28).
fn get_session_search_text(session: &SessionInfo) -> String {
    format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or(""),
        session.all_messages_text,
        session.cwd
    )
}

/// `hasSessionName` (session-selector-search.ts:30-32).
pub fn has_session_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

/// `parseSearchQuery` (session-selector-search.ts:39-114): `re:<pattern>`
/// regex mode, otherwise whitespace-separated tokens with `"phrase"`
/// quoting. Unbalanced quotes fall back to plain whitespace tokenization.
pub fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }

    // Regex mode: re:<pattern> (session-selector-search.ts:46-57).
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_string()),
            };
        }
        return match RegexBuilder::new(pattern).case_insensitive(true).build() {
            Ok(regex) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: Some(regex),
                error: None,
            },
            Err(err) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some(err.to_string()),
            },
        };
    }

    // Token mode with quote support (session-selector-search.ts:59-113).
    // Example: foo "node cve" bar
    let mut tokens: Vec<SearchToken> = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut had_unclosed_quote = false;

    // `flush` (session-selector-search.ts:66-71).
    fn flush(buf: &mut String, tokens: &mut Vec<SearchToken>, in_quote: bool) {
        let value = buf.trim().to_string();
        buf.clear();
        if value.is_empty() {
            return;
        }
        tokens.push(if in_quote {
            SearchToken::Phrase(value)
        } else {
            SearchToken::Fuzzy(value)
        });
    }

    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                flush(&mut buf, &mut tokens, true);
                in_quote = false;
            } else {
                flush(&mut buf, &mut tokens, false);
                in_quote = true;
            }
            continue;
        }
        if !in_quote && ch.is_whitespace() {
            flush(&mut buf, &mut tokens, false);
            continue;
        }
        buf.push(ch);
    }

    if in_quote {
        had_unclosed_quote = true;
    }

    // If quotes were unbalanced, fall back to plain whitespace tokenization
    // (session-selector-search.ts:98-109).
    if had_unclosed_quote {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: trimmed
                .split_whitespace()
                .map(|t| SearchToken::Fuzzy(t.to_string()))
                .collect(),
            regex: None,
            error: None,
        };
    }

    flush(&mut buf, &mut tokens, in_quote);

    ParsedSearchQuery {
        mode: QueryMode::Tokens,
        tokens,
        regex: None,
        error: None,
    }
}

/// Char index of a byte index (JS `indexOf`/`search` return UTF-16 code-unit
/// indices; scores are ordering hints, char indices rank identically).
fn char_index_at(text: &str, byte_index: usize) -> usize {
    text[..byte_index].chars().count()
}

/// `matchSession` (session-selector-search.ts:116-154).
pub fn match_session(session: &SessionInfo, parsed: &ParsedSearchQuery) -> MatchResult {
    let text = get_session_search_text(session);

    if parsed.mode == QueryMode::Regex {
        let Some(regex) = &parsed.regex else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        let Some(found) = regex.find(&text) else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        return MatchResult {
            matches: true,
            score: char_index_at(&text, found.start()) as f64 * 0.1,
        };
    }

    if parsed.tokens.is_empty() {
        return MatchResult {
            matches: true,
            score: 0.0,
        };
    }

    let mut total_score = 0.0;
    let mut normalized_text: Option<String> = None;

    for token in &parsed.tokens {
        match token {
            SearchToken::Phrase(phrase) => {
                if normalized_text.is_none() {
                    normalized_text = Some(normalize_whitespace_lower(&text));
                }
                let phrase = normalize_whitespace_lower(phrase);
                if phrase.is_empty() {
                    continue;
                }
                let normalized = normalized_text.as_ref().expect("set above");
                let Some(index) = normalized.find(&phrase) else {
                    return MatchResult {
                        matches: false,
                        score: 0.0,
                    };
                };
                total_score += char_index_at(normalized, index) as f64 * 0.1;
            }
            SearchToken::Fuzzy(value) => {
                let matched = fuzzy_match(value, &text);
                if !matched.matches {
                    return MatchResult {
                        matches: false,
                        score: 0.0,
                    };
                }
                total_score += matched.score;
            }
        }
    }

    MatchResult {
        matches: true,
        score: total_score,
    }
}

/// `filterAndSortSessions` (session-selector-search.ts:156-194). Recent mode
/// filters only, keeping the incoming order (the session manager already
/// returns sessions sorted by `modified` descending); other modes sort by
/// match score, tie-broken by `modified` descending.
pub fn filter_and_sort_sessions(
    sessions: Vec<SessionInfo>,
    query: &str,
    sort_mode: SortMode,
    name_filter: NameFilter,
) -> Vec<SessionInfo> {
    let name_filtered: Vec<SessionInfo> = if name_filter == NameFilter::All {
        sessions
    } else {
        sessions.into_iter().filter(has_session_name).collect()
    };
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return name_filtered;
    }

    let parsed = parse_search_query(query);
    if parsed.error.is_some() {
        return Vec::new();
    }

    // Recent mode: filter only, keep incoming order (session-selector-search.ts:171-177).
    if sort_mode == SortMode::Recent {
        return name_filtered
            .into_iter()
            .filter(|s| match_session(s, &parsed).matches)
            .collect();
    }

    // Relevance mode: sort by score, tie-break by modified desc
    // (session-selector-search.ts:180-193).
    let mut scored: Vec<(SessionInfo, f64)> = name_filtered
        .into_iter()
        .filter_map(|s| {
            let result = match_session(&s, &parsed);
            if result.matches {
                Some((s, result.score))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.modified_ms.cmp(&a.0.modified_ms))
    });

    scored.into_iter().map(|(s, _)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn session(
        id: &str,
        name: Option<&str>,
        cwd: &str,
        text: &str,
        modified_ms: i64,
    ) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            id: id.to_string(),
            cwd: cwd.to_string(),
            name: name.map(str::to_string),
            parent_session_path: None,
            created_ms: modified_ms - 1000,
            modified_ms,
            message_count: 1,
            first_message: text.to_string(),
            all_messages_text: text.to_string(),
        }
    }

    fn sessions() -> Vec<SessionInfo> {
        vec![
            session("s1", None, "/work/a", "node cve analysis", 300),
            session("s2", Some("deploy"), "/work/b", "deploy to prod", 200),
            session("s3", None, "/work/c", "refactor pipeline", 100),
        ]
    }

    #[test]
    fn parses_empty_query() {
        let parsed = parse_search_query("");
        assert_eq!(parsed.mode, QueryMode::Tokens);
        assert!(parsed.tokens.is_empty());
        assert!(parsed.error.is_none());
        assert_eq!(parse_search_query("   ").tokens.len(), 0);
    }

    #[test]
    fn parses_regex_query() {
        let parsed = parse_search_query("re: node.cve ");
        assert_eq!(parsed.mode, QueryMode::Regex);
        assert!(parsed.regex.is_some());
        assert!(parsed.error.is_none());

        // Case-insensitive flag ("i").
        let matched = match_session(&session("s1", None, "", "NODE CVE", 1), &parsed);
        assert!(matched.matches);

        // Empty pattern.
        let parsed = parse_search_query("re:  ");
        assert_eq!(parsed.error.as_deref(), Some("Empty regex"));

        // Invalid pattern.
        let parsed = parse_search_query("re:[");
        assert!(parsed.error.is_some());
        assert!(parsed.regex.is_none());
    }

    #[test]
    fn parses_tokens_with_phrase_quoting() {
        let parsed = parse_search_query(r#"foo "node cve" bar"#);
        assert_eq!(parsed.mode, QueryMode::Tokens);
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken::Fuzzy("foo".to_string()),
                SearchToken::Phrase("node cve".to_string()),
                SearchToken::Fuzzy("bar".to_string()),
            ]
        );
    }

    #[test]
    fn unbalanced_quotes_fall_back_to_whitespace_tokens() {
        // `foo "bar` keeps the quote character inside the token, exactly like
        // the upstream split(/\s+/) fallback (session-selector-search.ts:102-107).
        let parsed = parse_search_query(r#"foo "bar"#);
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken::Fuzzy("foo".to_string()),
                SearchToken::Fuzzy("\"bar".to_string()),
            ]
        );
        assert!(parsed.error.is_none());
    }

    #[test]
    fn phrase_matching_normalizes_whitespace_and_case() {
        let parsed = parse_search_query(r#""node   cve""#);
        assert_eq!(parsed.tokens.len(), 1);
        // "node cve" phrase must match "NODE   CVE" after normalization.
        let matched = match_session(&session("s1", None, "", "X NODE   CVE in deps", 1), &parsed);
        assert!(matched.matches);
        assert!(matched.score > 0.0);
        let missed = match_session(&session("s2", None, "", "nodes cve", 1), &parsed);
        assert!(!missed.matches);
    }

    #[test]
    fn fuzzy_token_must_match() {
        let parsed = parse_search_query("deploy prod");
        let matched = match_session(&session("s2", None, "", "deploy to prod", 1), &parsed);
        assert!(matched.matches);
        let missed = match_session(&session("s1", None, "", "node cve", 1), &parsed);
        assert!(!missed.matches);
    }

    #[test]
    fn empty_tokens_match_everything_with_zero_score() {
        let parsed = parse_search_query("");
        assert_eq!(
            match_session(&session("s1", None, "", "anything", 1), &parsed),
            MatchResult {
                matches: true,
                score: 0.0
            }
        );
    }

    #[test]
    fn regex_score_is_first_match_index() {
        let parsed = parse_search_query("re:cve");
        // Search text: "s1  node cve analysis /work/a".
        let matched = match_session(&session("s1", None, "/work/a", "node cve", 1), &parsed);
        assert!(matched.matches);
        assert!(matched.score > 0.0);
    }

    #[test]
    fn has_session_name_trims() {
        assert!(has_session_name(&session("s1", Some("x"), "", "", 1)));
        assert!(!has_session_name(&session("s1", Some("  "), "", "", 1)));
        assert!(!has_session_name(&session("s1", None, "", "", 1)));
    }

    #[test]
    fn recent_mode_filters_keeping_order() {
        let filtered =
            filter_and_sort_sessions(sessions(), "deploy", SortMode::Recent, NameFilter::All);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s2"]
        );
        // Empty query keeps everything in incoming order.
        let filtered = filter_and_sort_sessions(sessions(), "", SortMode::Recent, NameFilter::All);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s2", "s3"]
        );
    }

    #[test]
    fn relevance_mode_sorts_by_score_then_modified_desc() {
        // "alpha" matches both sessions fuzzily; "alpha beta" scores strictly
        // better than "beta alpha" (match at position 4 with a word-boundary
        // bonus vs. a mid-word match at position 7). s2 is newer, so the
        // score ordering (s1 first) can only come from the score, not the
        // modified tie-break.
        let sessions = vec![
            session("s2", None, "/w", "beta alpha", 500),
            session("s1", None, "/w", "alpha beta", 300),
        ];
        let filtered =
            filter_and_sort_sessions(sessions, "alpha", SortMode::Relevance, NameFilter::All);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );

        // Tie-break: two sessions matching with equal score sort by modified
        // descending (s4 newer than s5).
        let tie = vec![
            session("s5", None, "/x", "abc", 100),
            session("s4", None, "/x", "abc", 300),
        ];
        let filtered = filter_and_sort_sessions(tie, "abc", SortMode::Relevance, NameFilter::All);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s4", "s5"]
        );
    }

    #[test]
    fn named_filter_keeps_only_named_sessions() {
        let filtered =
            filter_and_sort_sessions(sessions(), "", SortMode::Recent, NameFilter::Named);
        assert_eq!(
            filtered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s2"]
        );
    }

    #[test]
    fn invalid_regex_returns_empty_list() {
        let filtered =
            filter_and_sort_sessions(sessions(), "re:[", SortMode::Relevance, NameFilter::All);
        assert!(filtered.is_empty());
    }
}
