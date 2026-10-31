//! The `tool_search` tool and extension (port of
//! `packages/durable/src/tool-search/tool-search.ts` @ `4a080252f`).
//!
//! The model searches the deferred tools it is not offered yet; the matches are
//! loaded with `control.addTools`, so the next request offers them. Loading
//! writes the conversation's `modelTools` (its `add`), so loaded tools stay
//! offered on that branch, also after a compaction.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::types::Tool;
use serde_json::{Value, json};

use crate::chord::context::Context;
use crate::chord::json::JsonObject;
use crate::harness::define::define_extension;
use crate::harness::types::{
    Extension, ToolCaller, ToolControl, ToolExecute, ToolExecuteError, ToolExecutionApi,
    ToolExecutionResult, ToolRegistration, ToolReplay,
};
use crate::search::{SearchDocument, rank_documents};
use crate::tools::tool_error;

/// Name of the discovery tool.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
/// Default maximum number of matches.
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

/// Fixed, so it does not change while deferred tools are registered.
const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

/// `tool_search` result details: the tools it loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchToolDetails {
    /// Loaded tool names, best first.
    pub loaded: Vec<String>,
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query for deferred tools." },
            "limit": {
                "type": "number",
                "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}.")
            }
        },
        "required": ["query"]
    })
}

/// Schema descriptions and property names, recursively.
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

/// Search text of a tool: its name, the name with `_` as spaces, its
/// description, and its parameters' text.
fn search_text(tool: &ToolRegistration) -> String {
    let mut parts: Vec<String> = vec![
        tool.name().to_string(),
        tool.name().replace('_', " "),
        tool.tool.description.clone(),
    ];
    schema_text(&tool.tool.parameters, &mut parts);
    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

struct ToolSearchTool;

#[async_trait]
impl ToolExecute for ToolSearchTool {
    async fn execute(
        &self,
        args: JsonObject,
        api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if query.trim().is_empty() {
            return Err(tool_error("query must not be empty"));
        }
        let limit = match args.get("limit").and_then(Value::as_f64) {
            None => DEFAULT_TOOL_SEARCH_LIMIT,
            Some(limit) if limit.is_finite() && limit.fract() == 0.0 && limit > 0.0 => {
                limit as usize
            }
            Some(_) => return Err(tool_error("limit must be a positive integer")),
        };
        let candidates = api.agent(context).await?.deferred;
        let documents: Vec<SearchDocument> = candidates
            .iter()
            .map(|tool| SearchDocument {
                name: tool.name().to_string(),
                text: search_text(tool),
            })
            .collect();
        let loaded = rank_documents(&query, &documents, limit);
        let text = if loaded.is_empty() {
            "No matching tools found.".to_string()
        } else {
            let lines = loaded
                .iter()
                .map(|name| {
                    let description = candidates
                        .iter()
                        .find(|tool| tool.name() == name)
                        .map(|tool| tool.tool.description.as_str())
                        .unwrap_or_default();
                    // The first non-empty line (upstream `.trim().split(/\r?\n/)[0]`).
                    let first = description
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default();
                    format!("- {name}: {}", first.trim())
                })
                .collect::<Vec<_>>()
                .join("\n");
            let plural = if loaded.len() == 1 { "tool" } else { "tools" };
            format!(
                "Loaded {} {plural}. They are available from your next call:\n{lines}",
                loaded.len()
            )
        };
        let details = serde_json::to_value(ToolSearchToolDetails {
            loaded: loaded.clone(),
        })
        .unwrap_or(Value::Null);
        let control = if loaded.is_empty() {
            None
        } else {
            Some(ToolControl {
                add_tools: Some(loaded),
                ..Default::default()
            })
        };
        Ok(ToolExecutionResult {
            output: Some(vec![rpi_ai::types::ToolResultContent::Text(
                rpi_ai::types::TextContent {
                    text,
                    text_signature: None,
                },
            )]),
            details: Some(details),
            control,
            ..Default::default()
        })
    }
}

/// The `tool_search` tool.
pub fn create_tool_search_tool() -> ToolRegistration {
    ToolRegistration {
        tool: Tool {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            description: TOOL_SEARCH_DESCRIPTION.to_string(),
            parameters: parameters(),
            constrained_sampling: None,
        },
        replay: ToolReplay::Safe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: None,
        // Searching changes what the model is offered; scripts have their own
        // `searchTools()`.
        callers: Some(vec![ToolCaller::Model]),
        deferred: false,
        execute: Arc::new(ToolSearchTool),
    }
}

/// The `tool_search` extension: install it and select it where the model should
/// find deferred tools.
pub fn create_tool_search_extension() -> Extension {
    define_extension(Extension {
        name: "tool-search".to_string(),
        tools: vec![create_tool_search_tool()],
        ..Extension::default()
    })
}
