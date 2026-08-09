//! Fuzzy matching utilities.
//!
//! Port of `packages/tui/src/fuzzy.ts` @ pi 0.82.1 (2efa728).
//!
//! Matches if all query characters appear in order (not necessarily
//! consecutive). Lower score = better match.

/// `FuzzyMatch` (fuzzy.ts:7-10).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

fn is_word_boundary_char(c: Option<char>) -> bool {
    matches!(c, None | Some(' ' | '\t' | '-' | '_' | '.' | '/' | ':'))
}

/// `fuzzyMatch` (fuzzy.ts:12-96).
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let match_query = |normalized_query: &[char]| -> FuzzyMatch {
        if normalized_query.is_empty() {
            return FuzzyMatch {
                matches: true,
                score: 0.0,
            };
        }
        if normalized_query.len() > text_lower.len() {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        }

        let mut query_index = 0usize;
        let mut score = 0.0f64;
        let mut last_match_index: i64 = -1;
        let mut consecutive_matches = 0i64;

        for (i, &ch) in text_lower.iter().enumerate() {
            if query_index >= normalized_query.len() {
                break;
            }
            if ch == normalized_query[query_index] {
                let is_word_boundary =
                    is_word_boundary_char(i.checked_sub(1).map(|j| text_lower[j]));

                // Reward consecutive matches.
                if last_match_index == i as i64 - 1 {
                    consecutive_matches += 1;
                    score -= (consecutive_matches * 5) as f64;
                } else {
                    consecutive_matches = 0;
                    // Penalize gaps.
                    if last_match_index >= 0 {
                        score += ((i as i64 - last_match_index - 1) * 2) as f64;
                    }
                }

                // Reward word boundary matches.
                if is_word_boundary {
                    score -= 10.0;
                }

                // Slight penalty for later matches.
                score += i as f64 * 0.1;

                last_match_index = i as i64;
                query_index += 1;
            }
        }

        if query_index < normalized_query.len() {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        }

        if normalized_query == text_lower.as_slice() {
            score -= 100.0;
        }

        FuzzyMatch {
            matches: true,
            score,
        }
    };

    let primary_match = match_query(&query_lower);
    if primary_match.matches {
        return primary_match;
    }

    // Letters/digits transposition fallback (fuzzy.ts:65-86).
    let query_str: String = query_lower.iter().collect();
    let swapped_query = swap_letters_digits(&query_str);
    let Some(swapped) = swapped_query else {
        return primary_match;
    };
    let swapped_chars: Vec<char> = swapped.chars().collect();
    let swapped_match = match_query(&swapped_chars);
    if !swapped_match.matches {
        return primary_match;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// `abc123` → `123abc` and vice versa (fuzzy.ts:65-76).
fn swap_letters_digits(query: &str) -> Option<String> {
    let bytes = query.as_bytes();
    let is_alpha = |b: u8| b.is_ascii_lowercase();
    let is_digit = |b: u8| b.is_ascii_digit();

    // ^([a-z]+)([0-9]+)$
    if let Some(split) = bytes.iter().position(|&b| is_digit(b)) {
        let (letters, digits) = ((&query[..split]), (&query[split..]));
        if !letters.is_empty()
            && !digits.is_empty()
            && letters.bytes().all(is_alpha)
            && digits.bytes().all(is_digit)
        {
            return Some(format!("{digits}{letters}"));
        }
    }
    // ^([0-9]+)([a-z]+)$
    if let Some(split) = bytes.iter().position(|&b| is_alpha(b)) {
        let (digits, letters) = ((&query[..split]), (&query[split..]));
        if !digits.is_empty()
            && !letters.is_empty()
            && digits.bytes().all(is_digit)
            && letters.bytes().all(is_alpha)
        {
            return Some(format!("{letters}{digits}"));
        }
    }
    None
}

/// `fuzzyFilter` (fuzzy.ts:99-140): all whitespace/slash-separated tokens
/// must match; results are sorted best-first.
pub fn fuzzy_filter<T>(items: Vec<T>, query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    if query.trim().is_empty() {
        return items;
    }
    let tokens: Vec<&str> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return items;
    }

    let mut results: Vec<(T, f64)> = Vec::new();
    for item in items {
        let text = get_text(&item);
        let mut total_score = 0.0;
        let mut all_match = true;
        for token in &tokens {
            let m = fuzzy_match(token, &text);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item, total_score));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subsequence_matching() {
        assert!(fuzzy_match("snnt", "claude-sonnet-4-5").matches);
        assert!(!fuzzy_match("xyz", "claude-sonnet-4-5").matches);
        assert!(fuzzy_match("", "anything").matches);
    }

    #[test]
    fn test_exact_match_scores_best() {
        let exact = fuzzy_match("gpt-4o", "gpt-4o");
        let partial = fuzzy_match("gpt-4o", "gpt-4o-mini");
        assert!(exact.score < partial.score);
    }

    #[test]
    fn test_fuzzy_filter_tokens_all_must_match() {
        let items = vec![
            "anthropic claude-sonnet",
            "anthropic claude-haiku",
            "openai gpt-4o",
        ];
        let filtered = fuzzy_filter(items, "anthropic sonnet", |s| s.to_string());
        assert_eq!(filtered, vec!["anthropic claude-sonnet"]);
    }
}
