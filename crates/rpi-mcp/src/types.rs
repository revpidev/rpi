//! MCP protocol types and result validation (port of
//! `packages/mcp/src/protocol/types.ts` @ a13d35a74).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::protocol::{McpError, is_object};

/// `LATEST_PROTOCOL_VERSION` (types.ts:4).
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// `SUPPORTED_PROTOCOL_VERSIONS` (types.ts:9): versions the client accepts
/// from a server. Older versions stay accepted for servers built on older
/// SDKs.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// `Implementation` (types.ts:14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl Implementation {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
        }
    }
}

/// `Root` (types.ts:20).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Root {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `ServerCapabilities` (types.ts:33). Unknown/extra keys are kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `InitializeResult` (types.ts:48).
#[derive(Debug, Clone, PartialEq)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: Implementation,
    pub instructions: Option<String>,
}

/// `validateInitializeResult` (client.ts:60).
pub fn validate_initialize_result(value: &Value) -> Result<InitializeResult, McpError> {
    let Some(map) = value.as_object() else {
        return Err(invalid("Invalid MCP initialize result"));
    };
    let protocol_version = map.get("protocolVersion").and_then(Value::as_str);
    let capabilities = map.get("capabilities");
    let server_info = map.get("serverInfo").and_then(Value::as_object);
    let name = server_info
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str);
    let version = server_info
        .and_then(|info| info.get("version"))
        .and_then(Value::as_str);
    let instructions = match map.get("instructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid("Invalid MCP initialize result")),
    };
    let (Some(protocol_version), Some(capabilities), Some(server_info), Some(name), Some(version)) =
        (protocol_version, capabilities, server_info, name, version)
    else {
        return Err(invalid("Invalid MCP initialize result"));
    };
    if !is_object(capabilities) {
        return Err(invalid("Invalid MCP initialize result"));
    }
    let capabilities: ServerCapabilities = serde_json::from_value(capabilities.clone())
        .map_err(|_| invalid("Invalid MCP initialize result"))?;
    Ok(InitializeResult {
        protocol_version: protocol_version.to_owned(),
        capabilities,
        server_info: Implementation {
            name: name.to_owned(),
            version: version.to_owned(),
            title: server_info
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        instructions,
    })
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::Invalid(message.into())
}

/// `ProgressNotification` (types.ts:56).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressNotification {
    #[serde(rename = "progressToken")]
    pub progress_token: Value,
    pub progress: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `ToolAnnotations` (types.ts:64).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint", skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// `Tool` (types.ts:78).
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: Option<ToolAnnotations>,
    pub execution: Option<Value>,
    pub meta: Option<Value>,
    /// Fields not modeled above, kept for round-tripping.
    pub extra: Map<String, Value>,
}

impl Tool {
    /// `isTool` (client.ts:105): `name` is a string + `inputSchema` is an object.
    pub fn is_tool(value: &Value) -> bool {
        value.get("name").is_some_and(Value::is_string)
            && value.get("inputSchema").is_some_and(is_object)
    }

    pub fn parse(value: &Value) -> Result<Self, McpError> {
        if !Self::is_tool(value) {
            return Err(invalid("Invalid entry in MCP tools/list result"));
        }
        let map = value.as_object().expect("checked object");
        let mut extra = map.clone();
        for key in [
            "name",
            "title",
            "description",
            "inputSchema",
            "outputSchema",
            "annotations",
            "execution",
            "_meta",
        ] {
            extra.remove(key);
        }
        let annotations = match map.get("annotations") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value(value.clone())
                    .map_err(|_| invalid("Invalid entry in MCP tools/list result"))?,
            ),
        };
        Ok(Tool {
            name: map
                .get("name")
                .and_then(Value::as_str)
                .expect("checked")
                .to_owned(),
            title: optional_string(map.get("title")),
            description: optional_string(map.get("description")),
            input_schema: map.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
            output_schema: map
                .get("outputSchema")
                .cloned()
                .filter(|value| !value.is_null()),
            annotations,
            execution: map
                .get("execution")
                .cloned()
                .filter(|value| !value.is_null()),
            meta: map.get("_meta").cloned().filter(|value| !value.is_null()),
            extra,
        })
    }
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

/// `Resource` (types.ts:97).
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<f64>,
    pub annotations: Option<Value>,
    pub meta: Option<Value>,
    pub extra: Map<String, Value>,
}

impl Resource {
    /// `isResource` (client.ts:108): `uri` is a string; some servers omit
    /// `name`, in which case the URI stands in.
    pub fn is_resource(value: &Value) -> bool {
        value.get("uri").is_some_and(Value::is_string)
            && value
                .get("name")
                .is_none_or(|name| name.is_null() || name.is_string())
    }

    pub fn parse(value: &Value) -> Result<Self, McpError> {
        if !Self::is_resource(value) {
            return Err(invalid("Invalid entry in MCP resources/list result"));
        }
        let map = value.as_object().expect("checked object");
        let uri = map
            .get("uri")
            .and_then(Value::as_str)
            .expect("checked")
            .to_owned();
        let mut extra = map.clone();
        for key in [
            "uri",
            "name",
            "title",
            "description",
            "mimeType",
            "size",
            "annotations",
            "_meta",
        ] {
            extra.remove(key);
        }
        Ok(Resource {
            name: map
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| uri.clone()),
            uri,
            title: optional_string(map.get("title")),
            description: optional_string(map.get("description")),
            mime_type: optional_string(map.get("mimeType")),
            size: map.get("size").and_then(Value::as_f64),
            annotations: map
                .get("annotations")
                .cloned()
                .filter(|value| !value.is_null()),
            meta: map.get("_meta").cloned().filter(|value| !value.is_null()),
            extra,
        })
    }
}

/// `ResourceTemplate` (types.ts:110).
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub annotations: Option<Value>,
    pub meta: Option<Value>,
    pub extra: Map<String, Value>,
}

impl ResourceTemplate {
    /// `isResourceTemplate` (client.ts:112).
    pub fn is_resource_template(value: &Value) -> bool {
        value.get("uriTemplate").is_some_and(Value::is_string)
            && value
                .get("name")
                .is_none_or(|name| name.is_null() || name.is_string())
    }

    pub fn parse(value: &Value) -> Result<Self, McpError> {
        if !Self::is_resource_template(value) {
            return Err(invalid(
                "Invalid entry in MCP resources/templates/list result",
            ));
        }
        let map = value.as_object().expect("checked object");
        let uri_template = map
            .get("uriTemplate")
            .and_then(Value::as_str)
            .expect("checked")
            .to_owned();
        let mut extra = map.clone();
        for key in [
            "uriTemplate",
            "name",
            "title",
            "description",
            "mimeType",
            "annotations",
            "_meta",
        ] {
            extra.remove(key);
        }
        Ok(ResourceTemplate {
            name: map
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| uri_template.clone()),
            uri_template,
            title: optional_string(map.get("title")),
            description: optional_string(map.get("description")),
            mime_type: optional_string(map.get("mimeType")),
            annotations: map
                .get("annotations")
                .cloned()
                .filter(|value| !value.is_null()),
            meta: map.get("_meta").cloned().filter(|value| !value.is_null()),
            extra,
        })
    }
}

/// One validated page of a paginated list (`validateListPage`, client.ts:77).
#[derive(Debug, Clone, PartialEq)]
pub struct ListPage {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

/// Validate a paginated list result. Some servers end pagination with `null`
/// or `""` instead of omitting the cursor.
pub fn validate_list_page(
    method: &str,
    key: &str,
    value: &Value,
    is_item: impl Fn(&Value) -> bool,
) -> Result<ListPage, McpError> {
    let items = value.get(key).and_then(Value::as_array);
    let (Some(items), true) = (items, is_object(value)) else {
        return Err(invalid(format!("Invalid MCP {method} result")));
    };
    if !items.iter().all(|item| is_object(item) && is_item(item)) {
        return Err(invalid(format!("Invalid entry in MCP {method} result")));
    }
    let next_cursor = match value.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid(format!("Invalid MCP {method} cursor"))),
    };
    Ok(ListPage {
        items: items.clone(),
        next_cursor,
    })
}

/// `pageCursor` (client.ts:120).
pub fn page_cursor(page: &ListPage) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(cursor) = &page.next_cursor {
        map.insert("nextCursor".to_owned(), Value::String(cursor.clone()));
    }
    map
}

/// `ReadResourceResult` (types.ts:126).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadResourceResult {
    pub contents: Vec<Value>,
    pub meta: Option<Value>,
}

/// `validateReadResourceResult` (client.ts:127).
pub fn validate_read_resource_result(value: &Value) -> Result<ReadResourceResult, McpError> {
    let Some(map) = value.as_object() else {
        return Err(invalid("Invalid MCP resources/read result"));
    };
    let Some(contents) = map.get("contents").and_then(Value::as_array) else {
        return Err(invalid("Invalid MCP resources/read result"));
    };
    for entry in contents {
        let valid = is_object(entry)
            && entry.get("uri").is_some_and(Value::is_string)
            && (entry.get("text").is_some_and(Value::is_string)
                || entry.get("blob").is_some_and(Value::is_string));
        if !valid {
            return Err(invalid("Invalid contents in MCP resources/read result"));
        }
    }
    Ok(ReadResourceResult {
        contents: contents.clone(),
        meta: map.get("_meta").cloned().filter(|value| !value.is_null()),
    })
}

/// The `CallToolResult` shape, validated and normalized (`content` defaults
/// to `[]`; client.ts:145).
#[derive(Debug, Clone, PartialEq)]
pub struct CallToolResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub meta: Option<Value>,
}

/// `validateCallToolResult` (client.ts:145).
pub fn validate_call_tool_result(value: &Value) -> Result<CallToolResult, McpError> {
    let Some(map) = value.as_object() else {
        return Err(invalid("Invalid MCP tools/call result"));
    };
    if let Some(content) = map.get("content")
        && !content.is_null()
        && !content.is_array()
    {
        return Err(invalid("Invalid MCP tools/call result"));
    }
    if let Some(structured) = map.get("structuredContent")
        && !structured.is_null()
        && !is_object(structured)
    {
        return Err(invalid("Invalid MCP tools/call structured content"));
    }
    Ok(CallToolResult {
        content: map
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        structured_content: map
            .get("structuredContent")
            .cloned()
            .filter(|value| !value.is_null()),
        is_error: map.get("isError").and_then(Value::as_bool),
        meta: map.get("_meta").cloned().filter(|value| !value.is_null()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_result_requires_core_fields() {
        let good = json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo", "version": "1.0.0"},
            "instructions": "hi",
        });
        let parsed = validate_initialize_result(&good).unwrap();
        assert_eq!(parsed.protocol_version, "2025-11-25");
        assert!(parsed.capabilities.tools.is_some());
        assert_eq!(parsed.server_info.name, "demo");
        assert_eq!(parsed.instructions.as_deref(), Some("hi"));

        for bad in [
            json!({}),
            json!({"protocolVersion": "x", "capabilities": {}, "serverInfo": {"name": "a"}}),
            json!({"protocolVersion": "x", "capabilities": {}, "serverInfo": {"name": "a", "version": "1"}, "instructions": 5}),
        ] {
            assert!(validate_initialize_result(&bad).is_err());
        }
    }

    #[test]
    fn list_pages_accept_null_and_empty_cursors() {
        let page = validate_list_page(
            "tools/list",
            "tools",
            &json!({"tools": [{"name": "t", "inputSchema": {}}], "nextCursor": null}),
            Tool::is_tool,
        )
        .unwrap();
        assert_eq!(page.next_cursor, None);
        let page = validate_list_page(
            "tools/list",
            "tools",
            &json!({"tools": [], "nextCursor": ""}),
            Tool::is_tool,
        )
        .unwrap();
        assert_eq!(page.next_cursor, None);
        let page = validate_list_page(
            "tools/list",
            "tools",
            &json!({"tools": [], "nextCursor": "abc"}),
            Tool::is_tool,
        )
        .unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("abc"));
        assert!(
            validate_list_page(
                "tools/list",
                "tools",
                &json!({"tools": [], "nextCursor": 1}),
                Tool::is_tool
            )
            .is_err()
        );
        assert!(validate_list_page("tools/list", "tools", &json!({}), Tool::is_tool).is_err());
    }

    #[test]
    fn call_tool_result_defaults_content() {
        let result = validate_call_tool_result(&json!({"structuredContent": {"a": 1}})).unwrap();
        assert!(result.content.is_empty());
        assert_eq!(result.structured_content, Some(json!({"a": 1})));
        assert!(validate_call_tool_result(&json!({"content": "x"})).is_err());
        assert!(validate_call_tool_result(&json!({"structuredContent": []})).is_err());
    }

    #[test]
    fn resource_name_falls_back_to_uri() {
        let resource = Resource::parse(&json!({"uri": "file:///x"})).unwrap();
        assert_eq!(resource.name, "file:///x");
        let template = ResourceTemplate::parse(&json!({"uriTemplate": "x://{id}"})).unwrap();
        assert_eq!(template.name, "x://{id}");
    }
}
