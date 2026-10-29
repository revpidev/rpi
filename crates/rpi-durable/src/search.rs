//! BM25 tool search for `tool_search` (port of
//! `packages/durable/src/search.ts` @ `4a080252f`).
//!
//! Durable keeps its own copy of the ranking the coding agent uses; the same
//! ranker backs `searchTools()` in code mode.

use std::collections::{HashMap, HashSet};

/// A searchable document: a name and the text built from its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDocument {
    /// Document name returned by the ranking.
    pub name: String,
    /// Searchable text.
    pub text: String,
}

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Naive singular form, so `issues` matches `issue` and `searches` matches
/// `search`.
fn stem(term: &str) -> String {
    if term.len() > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if term.len() > 4
        && (term.ends_with("ches")
            || term.ends_with("shes")
            || term.ends_with("sses")
            || term.ends_with("xes")
            || term.ends_with("zes"))
    {
        return term[..term.len() - 2].to_owned();
    }
    if term.len() > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_owned();
    }
    term.to_owned()
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics,
/// without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(text.len() + 8);
    let characters: Vec<char> = text.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        if index > 0 {
            let previous = characters[index - 1];
            let next = characters.get(index + 1).copied();
            let split_lower_upper = (previous.is_ascii_lowercase() || previous.is_ascii_digit())
                && character.is_ascii_uppercase();
            let split_acronym = previous.is_ascii_uppercase()
                && character.is_ascii_uppercase()
                && next.is_some_and(|next| next.is_ascii_lowercase());
            if split_lower_upper || split_acronym {
                spaced.push(' ');
            }
        }
        spaced.push(character.to_ascii_lowercase());
    }
    spaced
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// Okapi BM25 (k1 1.2, b 0.75): the names of the best matches, best first;
/// ties keep document order.
pub fn rank_documents(query: &str, documents: &[SearchDocument], limit: usize) -> Vec<String> {
    let k1 = 1.2_f64;
    let b = 0.75_f64;
    let query_terms: Vec<String> = {
        let mut seen = HashSet::new();
        tokenize(query)
            .into_iter()
            .filter(|term| seen.insert(term.clone()))
            .collect()
    };
    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }
    let term_counts: Vec<HashMap<String, usize>> = documents
        .iter()
        .map(|document| {
            let mut counts = HashMap::new();
            for term in tokenize(&document.text) {
                *counts.entry(term).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let lengths: Vec<usize> = term_counts
        .iter()
        .map(|counts| counts.values().sum())
        .collect();
    let total: usize = lengths.iter().sum();
    let average_length = if total == 0 {
        1.0
    } else {
        total as f64 / documents.len() as f64
    };
    let idf: HashMap<&String, f64> = query_terms
        .iter()
        .map(|term| {
            let frequency = term_counts
                .iter()
                .filter(|counts| counts.contains_key(term))
                .count();
            (
                term,
                (1.0 + (documents.len() as f64 - frequency as f64 + 0.5)
                    / (frequency as f64 + 0.5))
                    .ln(),
            )
        })
        .collect();
    let mut matches: Vec<(usize, String, f64)> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let mut score = 0.0;
        for term in &query_terms {
            let Some(count) = term_counts[index].get(term).copied() else {
                continue;
            };
            let count = count as f64;
            let norm = k1 * (1.0 - b + (b * lengths[index] as f64) / average_length);
            score +=
                idf.get(term).copied().unwrap_or(0.0) * ((count * (k1 + 1.0)) / (count + norm));
        }
        if score > 0.0 {
            matches.push((index, document.name.clone(), score));
        }
    }
    matches.sort_by(|left, right| {
        right
            .2
            .partial_cmp(&left.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(_, name, _)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(name: &str, text: &str) -> SearchDocument {
        SearchDocument {
            name: name.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn tokenize_splits_camel_case_and_drops_stop_words() {
        assert_eq!(
            tokenize("readFile and create_issue"),
            vec!["read", "file", "create", "issue"]
        );
    }

    #[test]
    fn rank_prefers_the_matching_document_and_keeps_ties_in_order() {
        let documents = vec![
            document("alpha", "unrelated text"),
            document("beta", "searches issues quickly"),
        ];
        assert_eq!(rank_documents("issue search", &documents, 8), vec!["beta"]);
        let tied = vec![
            document("first", "same words"),
            document("second", "same words"),
        ];
        assert_eq!(rank_documents("same", &tied, 8), vec!["first", "second"]);
    }
}
