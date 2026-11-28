//! BM25 tool search for `tool_search` (port of
//! `packages/durable/src/search.ts` @ `b4afdddf`).
//!
//! Durable keeps its own copy of the ranking the coding agent uses; the same
//! ranker backs `searchTools()` in code mode.

use std::collections::{HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// A searchable document split into the fields the ranker weighs differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDocument {
    pub name: String,
    /// Description and parameter names/descriptions; the name is separate.
    pub text: String,
    /// Namespace name, description, and instructions.
    pub namespace_text: Option<String>,
}

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

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

/// Unicode NFC normalization, identifier boundaries, UAX#29 word boundaries,
/// and English stop words/singular forms.
pub fn tokenize(text: &str) -> Vec<String> {
    let text = text.nfc().collect::<String>();
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
        spaced.extend(character.to_lowercase());
    }
    let normalized: String = spaced
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || (!character.is_whitespace() && is_mark(character)) {
                character
            } else {
                ' '
            }
        })
        .collect();
    normalized
        .split_word_bounds()
        .filter(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|character| character.is_alphanumeric() || is_mark(character))
                && !STOP_WORDS.contains(word)
        })
        .map(|word| {
            if word.is_ascii() {
                stem(word)
            } else {
                word.to_owned()
            }
        })
        .collect()
}

fn is_mark(character: char) -> bool {
    // Every Unicode combining mark (Mn/Mc/Me), not a hand-picked range list:
    // Thai/Devanagari/Arabic marks must stay attached to their base letter
    // (upstream `\p{M}`, search.ts:147).
    unicode_normalization::char::is_combining_mark(character)
}

fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '$' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn exact_name_query(query: &str, name: &str) -> bool {
    let normalized = normalize_name(name);
    normalized == query || normalized.ends_with(&format!("__{query}"))
}

struct IndexedDocument {
    name: String,
    name_terms: HashSet<String>,
    local_name_terms: Vec<String>,
    text_counts: HashMap<String, usize>,
    text_length: usize,
    namespace_terms: HashSet<String>,
}

fn index_document(document: &SearchDocument) -> IndexedDocument {
    let mut text_counts = HashMap::new();
    for term in tokenize(&document.text) {
        *text_counts.entry(term).or_insert(0) += 1;
    }
    IndexedDocument {
        name: document.name.clone(),
        name_terms: tokenize(&document.name).into_iter().collect(),
        local_name_terms: {
            let local = document.name.rsplit("__").next().unwrap_or(&document.name);
            let mut seen = HashSet::new();
            tokenize(local)
                .into_iter()
                .filter(|term| seen.insert(term.clone()))
                .collect()
        },
        text_length: text_counts.values().sum(),
        text_counts,
        namespace_terms: tokenize(document.namespace_text.as_deref().unwrap_or(""))
            .into_iter()
            .collect(),
    }
}

/// Okapi BM25 (k1 1.2, b 0.75): exact names first, then query-term coverage,
/// weighted score, and finally document order for ties.
pub fn rank_documents(query: &str, documents: &[SearchDocument], limit: usize) -> Vec<String> {
    let k1 = 1.2_f64;
    let b = 0.75_f64;
    let query_terms: HashSet<String> = tokenize(query).into_iter().collect();
    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }
    let indexed: Vec<IndexedDocument> = documents.iter().map(index_document).collect();
    let total: usize = indexed.iter().map(|document| document.text_length).sum();
    let average_length = if total == 0 {
        1.0
    } else {
        total as f64 / indexed.len() as f64
    };
    let idf: HashMap<&String, f64> = query_terms
        .iter()
        .map(|term| {
            let frequency = indexed
                .iter()
                .filter(|document| {
                    document.name_terms.contains(term)
                        || document.text_counts.contains_key(term)
                        || document.namespace_terms.contains(term)
                })
                .count();
            (
                term,
                (1.0 + (indexed.len() as f64 - frequency as f64 + 0.5) / (frequency as f64 + 0.5))
                    .ln(),
            )
        })
        .collect();
    let full_name_bonus: f64 = idf.values().sum();
    let exact_query = normalize_name(query);
    let mut matches = Vec::new();
    for (index, document) in indexed.iter().enumerate() {
        let norm = k1 * (1.0 - b + (b * document.text_length as f64) / average_length);
        let mut score = 0.0;
        let mut covered = 0;
        for term in &query_terms {
            let count = document.text_counts.get(term).copied().unwrap_or(0) as f64;
            let weight = if document.name_terms.contains(term) {
                3.0
            } else {
                0.0
            } + if document.namespace_terms.contains(term) {
                0.5
            } else {
                0.0
            } + (count * (k1 + 1.0)) / (count + norm);
            if weight > 0.0 {
                covered += 1;
            }
            score += idf.get(term).copied().unwrap_or(0.0) * weight;
        }
        if score == 0.0 {
            continue;
        }
        if !document.local_name_terms.is_empty()
            && document
                .local_name_terms
                .iter()
                .all(|term| query_terms.contains(term))
        {
            score += full_name_bonus;
        }
        matches.push((
            index,
            document.name.clone(),
            score,
            covered,
            exact_name_query(&exact_query, &document.name),
        ));
    }
    matches.sort_by(|left, right| {
        right
            .4
            .cmp(&left.4)
            .then(right.3.cmp(&left.3))
            .then(
                right
                    .2
                    .partial_cmp(&left.2)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(left.0.cmp(&right.0))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(_, name, _, _, _)| name)
        .collect()
}

fn decode_pointer_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    let mut output = String::with_capacity(decoded.len());
    let mut characters = decoded.chars();
    while let Some(character) = characters.next() {
        if character == '~' {
            match characters.next() {
                Some('0') => output.push('~'),
                Some('1') => output.push('/'),
                _ => return None,
            }
        } else {
            output.push(character);
        }
    }
    Some(output)
}

fn resolve_local_ref<'a>(
    reference: &str,
    root: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    if reference == "#" {
        return Some(root);
    }
    if !reference.starts_with("#/") {
        return None;
    }
    let mut target = root;
    for segment in reference[2..].split('/') {
        let key = decode_pointer_segment(segment)?;
        target = match target {
            serde_json::Value::Object(object) => object.get(&key)?,
            serde_json::Value::Array(array) => array.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(target)
}

/// Append description/property text, expanding only referenced local schemas.
pub(crate) fn append_schema_search_text(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    seen: &mut HashSet<usize>,
    parts: &mut Vec<String>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str)
        && let Some(target) = resolve_local_ref(reference, root)
    {
        let address = std::ptr::from_ref(target) as usize;
        if seen.insert(address) {
            append_schema_search_text(target, root, seen, parts);
        }
    }
    if let Some(description) = object
        .get("description")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(description.to_owned());
    }
    if let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        for (name, property) in properties {
            parts.push(name.clone());
            append_schema_search_text(property, root, seen, parts);
        }
    }
    if let Some(items) = object.get("items") {
        append_schema_search_text(items, root, seen, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(serde_json::Value::as_array) {
            for variant in variants {
                append_schema_search_text(variant, root, seen, parts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(name: &str, text: &str) -> SearchDocument {
        SearchDocument {
            name: name.to_string(),
            text: text.to_string(),
            namespace_text: None,
        }
    }

    #[test]
    fn tokenize_splits_camel_case_unicode_and_nfc() {
        assert_eq!(
            tokenize("readFile and create_issue"),
            vec!["read", "file", "create", "issue"]
        );
        assert_eq!(tokenize("讀取"), vec!["讀", "取"]);
        assert_eq!(tokenize("Cafe\u{301}"), tokenize("Café"));
        // Combining marks stay attached (upstream `\p{M}`); UAX #29 still
        // splits Thai into clusters without Intl.Segmenter's dictionary.
        assert_eq!(
            tokenize("อ่านไฟล์"),
            vec!["อ\u{e48}", "า", "น", "ไ", "ฟ", "ล\u{e4c}"]
        );
        assert_eq!(tokenize("नमस्ते"), vec!["नमस्ते"]);
    }

    #[test]
    fn rank_prefers_coverage_names_exact_matches_and_local_names() {
        let documents = vec![
            document("window", "unrelated"),
            document("capture", "screenshot window"),
            document("mcp__peekaboo__app", "unrelated"),
            document("other__app", "unrelated"),
        ];
        assert_eq!(
            rank_documents("screenshot window", &documents, 2),
            vec!["capture", "window"]
        );
        assert_eq!(
            rank_documents("peekaboo app", &documents, 2),
            vec!["mcp__peekaboo__app", "other__app"]
        );
        assert_eq!(
            rank_documents("other__app", &documents, 1),
            vec!["other__app"]
        );
    }

    #[test]
    fn rank_uses_namespace_at_lower_weight() {
        let documents = vec![
            SearchDocument {
                name: "read".to_owned(),
                text: "read a file".to_owned(),
                namespace_text: Some("filesystem tools".to_owned()),
            },
            document("write", "write a file"),
        ];
        assert_eq!(rank_documents("filesystem", &documents, 2), vec!["read"]);
    }

    #[test]
    fn schema_expands_referenced_definitions_once_and_ignores_cycles() {
        let schema = serde_json::json!({
            "description": "root",
            "properties": {
                "first": {"$ref": "#/$defs/shared"},
                "cycle": {"$ref": "#/$defs/cycle"},
                "bad": {"$ref": "#/$defs/missing"},
                "__proto__": {"description": "safe own property"}
            },
            "$defs": {
                "shared": {"description": "shared definition"},
                "cycle": {"$ref": "#/$defs/cycle"},
                "unused": {"description": "must not be searched"}
            }
        });
        let mut parts = Vec::new();
        let mut seen = HashSet::from([std::ptr::from_ref(&schema) as usize]);
        append_schema_search_text(&schema, &schema, &mut seen, &mut parts);
        let text = parts.join(" ");
        assert!(text.contains("shared definition"));
        assert!(text.contains("safe own property"));
        assert!(!text.contains("must not be searched"));
        assert_eq!(
            parts
                .iter()
                .filter(|part| *part == "shared definition")
                .count(),
            1
        );
    }
}
