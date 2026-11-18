//! `tool_search`: BM25 tool discovery over the tools that are not declared to
//! the model (`codemode`/`deferred` exposure). Port of
//! `packages/coding-agent/src/extensions/tool-search/tool.ts` @ b4afdddf.
//!
//! The same ranker backs `searchTools()` in codemode scripts.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::loader::InlineExtension;
use rpi_ext_host::types::{ToolDefinition, ToolExposure, ToolNamespace};
use serde_json::{Value, json};

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

/// A tool as the ranker sees it: its name and the text built by
/// [`create_tool_search_document`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSearchDocument {
    pub name: String,
    pub text: String,
    pub namespace_text: Option<String>,
}

/// `ToolSearchMatch` (tool.ts:33-36).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchMatch {
    pub name: String,
    pub score: f64,
}

/// `STOP_WORDS` (tool.ts:39-61).
const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Naive singular form, so `issues` matches `issue` and `searches` matches
/// `search` (tool.ts:70-76).
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
/// and English stop words/singular forms (tool.ts:71-80).
pub fn tokenize(text: &str) -> Vec<String> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_segmentation::UnicodeSegmentation;

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
    matches!(character, '\u{0300}'..='\u{036f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}' | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}')
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

fn resolve_local_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
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
            Value::Object(object) => object.get(&key)?,
            Value::Array(array) => array.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(target)
}

/// Schema descriptions and property names, following referenced local schemas
/// once and never walking unreferenced definitions (tool.ts:104-126).
fn schema_text(schema: &Value, root: &Value, seen: &mut HashSet<usize>, parts: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_local_ref(reference, root)
    {
        let address = std::ptr::from_ref(target) as usize;
        if seen.insert(address) {
            schema_text(target, root, seen, parts);
        }
    }
    if let Some(description) = object.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, root, seen, parts);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, root, seen, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(Value::as_array) {
            for variant in variants {
                schema_text(variant, root, seen, parts);
            }
        }
    }
}

/// Description and schema text stay separate from the tool and namespace names
/// (tool.ts:131-141).
pub fn create_tool_search_document(
    name: &str,
    description: &str,
    parameters: &Value,
    namespace: Option<&ToolNamespace>,
) -> ToolSearchDocument {
    let mut parts: Vec<String> = vec![description.to_owned()];
    let mut seen = HashSet::from([std::ptr::from_ref(parameters) as usize]);
    schema_text(parameters, parameters, &mut seen, &mut parts);
    ToolSearchDocument {
        name: name.to_owned(),
        text: parts
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        namespace_text: namespace.map(|namespace| {
            [
                Some(namespace.name.clone()),
                namespace.description.clone(),
                namespace.instructions.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
        }),
    }
}

/// Okapi BM25 with the usual parameters. Ties keep document order
/// (tool.ts:213-261).
pub struct Bm25Ranker {
    k1: f64,
    b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25Ranker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rank(
        &self,
        query: &str,
        documents: &[ToolSearchDocument],
        limit: usize,
    ) -> Vec<ToolSearchMatch> {
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
                    (1.0 + (indexed.len() as f64 - frequency as f64 + 0.5)
                        / (frequency as f64 + 0.5))
                        .ln(),
                )
            })
            .collect();
        let full_name_bonus: f64 = idf.values().sum();
        let exact_query = normalize_name(query);
        let mut matches = Vec::new();
        for (index, document) in indexed.iter().enumerate() {
            let norm =
                self.k1 * (1.0 - self.b + (self.b * document.text_length as f64) / average_length);
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
                } + (count * (self.k1 + 1.0)) / (count + norm);
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
                ToolSearchMatch {
                    name: document.name.clone(),
                    score,
                },
                covered,
                exact_name_query(&exact_query, &document.name),
            ));
        }
        matches.sort_by(|left, right| {
            right
                .3
                .cmp(&left.3)
                .then(right.2.cmp(&left.2))
                .then(
                    right
                        .1
                        .score
                        .partial_cmp(&left.1.score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(left.0.cmp(&right.0))
        });
        matches
            .into_iter()
            .take(limit)
            .map(|(_, result, _, _)| result)
            .collect()
    }
}

struct IndexedDocument {
    name: String,
    name_terms: HashSet<String>,
    local_name_terms: Vec<String>,
    text_counts: HashMap<String, usize>,
    text_length: usize,
    namespace_terms: HashSet<String>,
}

fn index_document(document: &ToolSearchDocument) -> IndexedDocument {
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

/// `toolSearchSchema` (tool.ts:264-269).
pub fn tool_search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query for deferred tools." },
            "limit": { "type": "number", "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}.") },
        },
        "required": ["query"],
        "additionalProperties": false,
    })
}

/// `TOOL_SEARCH_DESCRIPTION` (tool.ts:320).
pub const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

/// `isSearchable` (tool.ts:292-294).
fn is_searchable(exposure: ToolExposure) -> bool {
    matches!(exposure, ToolExposure::Codemode | ToolExposure::Deferred)
}

struct SearchableTool {
    name: String,
    description: String,
    parameters: Value,
    exposure: ToolExposure,
    namespace: Option<ToolNamespace>,
}

fn parse_searchable_tools(api: &ExtensionApi) -> Vec<SearchableTool> {
    api.get_all_tools()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| {
            let object = tool.as_object()?;
            let name = object.get("name")?.as_str()?.to_owned();
            let description = object
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let parameters = object.get("parameters").cloned().unwrap_or(Value::Null);
            let exposure = object
                .get("exposure")
                .and_then(|value| serde_json::from_value::<ToolExposure>(value.clone()).ok())
                .unwrap_or_default();
            let namespace = object
                .get("namespace")
                .filter(|value| !value.is_null())
                .and_then(|value| serde_json::from_value::<ToolNamespace>(value.clone()).ok());
            Some(SearchableTool {
                name,
                description,
                parameters,
                exposure,
                namespace,
            })
        })
        .collect()
}

/// Rank the searchable tools that are not active yet and activate the
/// matches, so the next model call declares them (tool.ts:300-315).
pub fn search_and_load(api: &ExtensionApi, query: &str, limit: usize) -> Vec<(String, String)> {
    let active = api.get_active_tools().unwrap_or_default();
    let active_set: HashSet<&str> = active.iter().map(String::as_str).collect();
    let candidates: Vec<SearchableTool> = parse_searchable_tools(api)
        .into_iter()
        .filter(|tool| is_searchable(tool.exposure) && !active_set.contains(tool.name.as_str()))
        .collect();
    let documents: Vec<ToolSearchDocument> = candidates
        .iter()
        .map(|tool| {
            create_tool_search_document(
                &tool.name,
                &tool.description,
                &tool.parameters,
                tool.namespace.as_ref(),
            )
        })
        .collect();
    let matches = Bm25Ranker::new().rank(query, &documents, limit);
    if !matches.is_empty() {
        let mut names: Vec<String> = active.clone();
        names.extend(matches.iter().map(|matched| matched.name.clone()));
        let _ = api.set_active_tools(names);
    }
    matches
        .into_iter()
        .map(|matched| {
            let description = candidates
                .iter()
                .find(|tool| tool.name == matched.name)
                .map(|tool| tool.description.clone())
                .unwrap_or_default();
            (matched.name, description)
        })
        .collect()
}

/// `createToolSearchToolDefinition` (tool.ts:322-349).
pub fn create_tool_search_tool_definition(api: ExtensionApi) -> ToolDefinition {
    ToolDefinition {
        name: TOOL_SEARCH_TOOL_NAME.to_owned(),
        label: TOOL_SEARCH_TOOL_NAME.to_owned(),
        description: TOOL_SEARCH_DESCRIPTION.to_owned(),
        prompt_snippet: Some(
            "Search for tools that are not loaded yet and load the matches".to_owned(),
        ),
        prompt_guidelines: None,
        parameters: tool_search_schema(),
        constrained_sampling: None,
        output_schema: None,
        // Searching is not something scripts need; it changes what the model
        // sees (tool.ts:335).
        exposure: ToolExposure::ModelOnly,
        namespace: None,
        annotations: None,
        default_active: Some(false),
        prepare_loadout: None,
        reaches_tools: Some(crate::core::extensions::reaches_all_tools()),
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |request, _ctx| {
            let api = api.clone();
            Box::pin(async move {
                let params = request.params;
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if query.is_empty() {
                    return Err("query must not be empty".to_owned());
                }
                let limit = match params.get("limit") {
                    None | Some(Value::Null) => DEFAULT_TOOL_SEARCH_LIMIT,
                    Some(value) => {
                        let Some(number) = value.as_f64() else {
                            return Err("limit must be a positive integer".to_owned());
                        };
                        if number.fract() != 0.0 || number <= 0.0 {
                            return Err("limit must be a positive integer".to_owned());
                        }
                        number as usize
                    }
                };
                let tools = search_and_load(&api, &query, limit);
                let text = if tools.is_empty() {
                    "No matching tools found.".to_owned()
                } else {
                    let listing: Vec<String> = tools
                        .iter()
                        .map(|(name, description)| {
                            let first_line = description.lines().next().unwrap_or("");
                            format!("- {name}: {first_line}")
                        })
                        .collect();
                    format!(
                        "Loaded {} tool{}. They are available from your next call:\n{}",
                        tools.len(),
                        if tools.len() == 1 { "" } else { "s" },
                        listing.join("\n")
                    )
                };
                Ok(rpi_agent::types::AgentToolResult {
                    content: vec![rpi_ai::types::ToolResultContent::Text(
                        rpi_ai::types::TextContent {
                            text,
                            text_signature: None,
                        },
                    )],
                    details: json!({ "loaded": tools.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>() }),
                    ..Default::default()
                })
            })
        }),
        render_call: None,
        render_result: None,
    }
}

/// The built-in hidden `tool_search` extension.
pub fn inline_extension() -> InlineExtension {
    InlineExtension::Named {
        name: "tool-search".to_owned(),
        hidden: true,
        // `extensions/index.ts:12`: replaceable builtin.
        replaceable: true,
        builtin: true,
        factory: Arc::new(|api| {
            Box::pin(async move {
                api.register_tool(create_tool_search_tool_definition(api.clone()))
                    .map_err(|error| error.to_string())?;
                Ok(())
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    //! Ports `test/tool-search.test.ts` @ b4afdddf (ranker cases).

    use super::*;

    #[test]
    fn tokenize_splits_camel_case_and_drops_stop_words() {
        assert_eq!(tokenize("readFileFromDisk"), vec!["read", "file", "disk"]);
        assert_eq!(tokenize("web_search"), vec!["web", "search"]);
        assert_eq!(tokenize("getHTTPResponse"), vec!["get", "http", "response"]);
        assert_eq!(tokenize("a the of"), Vec::<String>::new());
    }

    #[test]
    fn stem_singularizes_naively() {
        assert_eq!(stem("issues"), "issue");
        assert_eq!(stem("searches"), "search");
        assert_eq!(stem("classes"), "class");
        assert_eq!(stem("ss"), "ss");
        assert_eq!(stem("files"), "file");
    }

    #[test]
    fn search_document_expands_only_referenced_local_definitions() {
        let schema = json!({
            "properties": {
                "first": {"$ref": "#/$defs/shared"},
                "cycle": {"$ref": "#/$defs/cycle"},
                "__proto__": {"description": "own property"}
            },
            "$defs": {
                "shared": {"description": "shared definition"},
                "cycle": {"$ref": "#/$defs/cycle"},
                "unused": {"description": "not searchable"}
            }
        });
        let document = create_tool_search_document("read", "Read", &schema, None);
        assert!(document.text.contains("shared definition"));
        assert!(document.text.contains("own property"));
        assert!(!document.text.contains("not searchable"));
        assert_eq!(document.text.matches("shared definition").count(), 1);
    }

    #[test]
    fn bm25_ranks_matching_tools_and_keeps_order_on_ties() {
        let documents = vec![
            ToolSearchDocument {
                name: "web_search".to_owned(),
                text: "web search the internet".to_owned(),
                namespace_text: None,
            },
            ToolSearchDocument {
                name: "read_file".to_owned(),
                text: "read a file from disk".to_owned(),
                namespace_text: None,
            },
        ];
        let ranker = Bm25Ranker::new();
        let matches = ranker.rank("search the web", &documents, 8);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "web_search");
        assert!(ranker.rank("", &documents, 8).is_empty());
        assert!(ranker.rank("file", &documents, 0).is_empty());
    }

    #[test]
    fn tokenize_handles_cjk_thai_and_nfc() {
        assert_eq!(tokenize("讀取"), vec!["讀", "取"]);
        assert!(!tokenize("อ่านไฟล์").is_empty());
        assert_eq!(tokenize("Cafe\u{301}"), tokenize("Café"));
    }

    #[test]
    fn search_document_collects_schema_and_namespace_text() {
        let parameters = json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "file path" } },
        });
        let namespace = ToolNamespace {
            name: "mcp__docs".to_owned(),
            aliases: Some(vec!["docs".to_owned()]),
            description: Some("Documentation tools".to_owned()),
            instructions: Some("Prefer search before fetch".to_owned()),
        };
        let document =
            create_tool_search_document("web_search", "Search it", &parameters, Some(&namespace));
        assert_eq!(document.name, "web_search");
        assert_eq!(document.text, "Search it path file path");
        assert_eq!(
            document.namespace_text.as_deref(),
            Some("mcp__docs Documentation tools Prefer search before fetch")
        );
    }
}
