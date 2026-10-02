//! `tool_search`: BM25 tool discovery over the tools that are not declared to
//! the model (`codemode`/`deferred` exposure). Port of
//! `packages/coding-agent/src/extensions/tool-search/tool.ts` @ a13d35a74.
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
}

/// `ToolSearchMatch` (tool.ts:29-32).
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
/// `search` (tool.ts:63-69).
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
/// without stop words (tool.ts:71-80).
pub fn tokenize(text: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static SPLIT_LOWER_UPPER: OnceLock<regex::Regex> = OnceLock::new();
    static SPLIT_ACRONYM: OnceLock<regex::Regex> = OnceLock::new();
    static NON_ALNUM: OnceLock<regex::Regex> = OnceLock::new();
    let lower_upper = SPLIT_LOWER_UPPER
        .get_or_init(|| regex::Regex::new(r"([a-z0-9])([A-Z])").expect("valid regex"));
    let acronym = SPLIT_ACRONYM
        .get_or_init(|| regex::Regex::new(r"([A-Z]+)([A-Z][a-z])").expect("valid regex"));
    let non_alnum =
        NON_ALNUM.get_or_init(|| regex::Regex::new(r"[^a-z0-9]+").expect("valid regex"));
    let split = lower_upper.replace_all(text, "$1 $2");
    let split = acronym.replace_all(&split, "$1 $2");
    let lowered = split.to_lowercase();
    non_alnum
        .split(&lowered)
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// Schema descriptions and property names, recursively (tool.ts:86-101).
fn schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(description) = object.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(Value::as_array) {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}

/// Search text of a tool: the name, the name with `_` as spaces, the
/// description, schema descriptions and property names, and the namespace
/// with its description and instructions (tool.ts:103-116).
pub fn create_tool_search_document(
    name: &str,
    description: &str,
    parameters: &Value,
    namespace: Option<&ToolNamespace>,
) -> ToolSearchDocument {
    let mut parts: Vec<String> = vec![
        name.to_owned(),
        name.replace('_', " "),
        description.to_owned(),
    ];
    schema_text(parameters, &mut parts);
    if let Some(namespace) = namespace {
        parts.push(namespace.name.clone());
        parts.push(namespace.description.clone().unwrap_or_default());
        parts.push(namespace.instructions.clone().unwrap_or_default());
    }
    ToolSearchDocument {
        name: name.to_owned(),
        text: parts
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Okapi BM25 with the usual parameters. Ties keep document order
/// (tool.ts:118-157).
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
        let average_length = {
            let total: usize = lengths.iter().sum();
            if documents.is_empty() || total == 0 {
                1.0
            } else {
                total as f64 / documents.len() as f64
            }
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
        let mut matches: Vec<ToolSearchMatch> = Vec::new();
        for (index, document) in documents.iter().enumerate() {
            let mut score = 0.0;
            for term in &query_terms {
                let count = term_counts[index].get(term).copied().unwrap_or(0);
                if count == 0 {
                    continue;
                }
                let norm =
                    self.k1 * (1.0 - self.b + (self.b * lengths[index] as f64) / average_length);
                score += idf.get(term).copied().unwrap_or(0.0)
                    * ((count as f64 * (self.k1 + 1.0)) / (count as f64 + norm));
            }
            if score > 0.0 {
                matches.push(ToolSearchMatch {
                    name: document.name.clone(),
                    score,
                });
            }
        }
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(limit);
        matches
    }
}

/// `toolSearchSchema` (tool.ts:159-167).
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

/// `TOOL_SEARCH_DESCRIPTION` (tool.ts:220).
pub const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

/// `isSearchable` (tool.ts:192-194).
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
/// matches, so the next model call declares them (tool.ts:196-214).
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

/// `createToolSearchToolDefinition` (tool.ts:222-247).
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
        // sees (tool.ts:233).
        exposure: ToolExposure::ModelOnly,
        namespace: None,
        annotations: None,
        default_active: Some(false),
        prepare_loadout: None,
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
    //! Ports `test/tool-search.test.ts` @ a13d35a74 (ranker cases).

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
    fn bm25_ranks_matching_tools_and_keeps_order_on_ties() {
        let documents = vec![
            ToolSearchDocument {
                name: "web_search".to_owned(),
                text: "web search the internet".to_owned(),
            },
            ToolSearchDocument {
                name: "read_file".to_owned(),
                text: "read a file from disk".to_owned(),
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
    fn search_document_collects_schema_and_namespace_text() {
        let parameters = json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "file path" } },
        });
        let namespace = ToolNamespace {
            name: "mcp__docs".to_owned(),
            description: Some("Documentation tools".to_owned()),
            instructions: Some("Prefer search before fetch".to_owned()),
        };
        let document =
            create_tool_search_document("web_search", "Search it", &parameters, Some(&namespace));
        for needle in [
            "web_search",
            "web search",
            "Search it",
            "file path",
            "mcp__docs",
            "Documentation tools",
            "Prefer search before fetch",
        ] {
            assert!(
                document.text.contains(needle),
                "{needle}: {}",
                document.text
            );
        }
    }
}
