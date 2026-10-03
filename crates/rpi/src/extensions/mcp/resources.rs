//! MCP resource tools (port of
//! `packages/coding-agent/src/extensions/mcp/resources.ts` @ a13d35a74):
//! `list_mcp_resources`, `list_mcp_resource_templates` and
//! `read_mcp_resource`, covering every connected server with resources.
//! MCP App resources (`ui://` or `profile=mcp-app`) and icons are left out.

use std::sync::Arc;

use rpi_agent::types::AgentToolResult;
use rpi_ai::types::{TextContent, ToolResultContent};
use rpi_ext_host::types::{
    ComponentTree, ToolAnnotations, ToolDefinition, ToolExecuteRequest, ToolRenderContext,
    ToolRenderResultOptions,
};
use rpi_mcp::McpRequestOptions;
use rpi_mcp::protocol::McpError;
use rpi_mcp::types::ListPage;
use serde_json::{Value, json};

use super::tools::{ConvertMcpResultOptions, McpToolDetails, limit_mcp_content, to_model_content};
use super::tools::{McpOutputSaver, save_to_temp_file};

pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
pub use super::tools::READ_MCP_RESOURCE_TOOL;

/// A connected server that offers resources (`McpResourceServer`,
/// resources.ts:39).
#[async_trait::async_trait]
pub trait McpResourceServer: Send + Sync {
    fn resource_server_name(&self) -> &str;
    fn resource_server_timeout_ms(&self) -> u64;
    async fn resources_page(
        &self,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<ListPage, McpError>;
    async fn resource_templates_page(
        &self,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<ListPage, McpError>;
    async fn all_resources(&self, options: McpRequestOptions) -> Result<Vec<Value>, McpError>;
    async fn all_resource_templates(
        &self,
        options: McpRequestOptions,
    ) -> Result<Vec<Value>, McpError>;
    async fn read_resource(&self, uri: &str, options: McpRequestOptions)
    -> Result<Value, McpError>;
}

/// `isMcpAppResource` (resources.ts:53): MCP App user interfaces, which only
/// hosts that render them can use.
pub fn is_mcp_app_uri(uri: &str, mime_type: Option<&str>) -> bool {
    if uri.starts_with("ui://") {
        return true;
    }
    let Some(mime_type) = mime_type else {
        return false;
    };
    // `/;\s*profile\s*=\s*"?mcp-app"?/i` (resources.ts:54).
    let lowered = mime_type.to_ascii_lowercase();
    for (index, _) in lowered.match_indices("profile") {
        if !lowered[..index].trim_end().ends_with(';') {
            continue;
        }
        let rest = lowered[index + "profile".len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        if rest.starts_with("mcp-app") {
            return true;
        }
    }
    false
}

/// `listed` (resources.ts:57): the item without `_meta`/icons, tagged with
/// its server.
fn listed(server: &str, item: &Value) -> Value {
    let mut object = item.as_object().cloned().unwrap_or_default();
    object.remove("_meta");
    object.remove("icons");
    object.insert("server".to_owned(), Value::String(server.to_owned()));
    Value::Object(object)
}

fn string_property(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn string_argument(params: &Value, key: &str) -> Result<Option<String>, String> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(text) = value.as_str() else {
        return Err(format!("{key} must be a string"));
    };
    let trimmed = text.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    })
}

fn json_result(
    tool: &str,
    server: Option<&str>,
    payload: Value,
) -> Result<AgentToolResult, String> {
    let (content, full_output_path) = limit_mcp_content(
        vec![ToolResultContent::Text(TextContent {
            text: serde_json::to_string(&payload).unwrap_or_default(),
            text_signature: None,
        })],
        &save_default(),
    );
    Ok(AgentToolResult {
        content,
        details: McpToolDetails {
            server: server.unwrap_or_default().to_owned(),
            tool: tool.to_owned(),
            full_output_path,
        }
        .to_json(),
        structured_content: Some(payload),
        usage: None,
        is_error: None,
        terminate: None,
    })
}

fn save_default() -> McpOutputSaver {
    Arc::new(save_to_temp_file)
}

/// Options for [`create_mcp_resource_tool_definitions`] (resources.ts:279).
pub struct CreateResourceToolOptions {
    /// Exposure of the resource tools: the widest exposure of the servers
    /// they reach.
    pub exposure: super::config::McpExposure,
    /// The servers whose resources they reach, at call time.
    pub servers: Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>,
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string_property("MCP server name. Omit to list every server with resources."),
            "cursor": string_property("Opaque cursor from a previous call with the same server; omit for the first page."),
        },
        "additionalProperties": false,
    })
}

fn read_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string_property("MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources."),
            "uri": string_property("Resource URI to read. Must be one of the URIs returned by list_mcp_resources."),
        },
        "required": ["server", "uri"],
        "additionalProperties": false,
    })
}

fn listing_errors_schema() -> Value {
    json!({
        "type": "array",
        "description": "Servers that could not be listed",
        "items": {
            "type": "object",
            "properties": {"server": {"type": "string"}, "error": {"type": "string"}},
            "required": ["server", "error"],
        },
    })
}

fn list_output_schema(templates: bool) -> Value {
    if templates {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "resourceTemplates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"},
                            "uriTemplate": {"type": "string", "description": "RFC 6570 URI template"},
                            "name": {"type": "string"},
                            "title": {"type": "string"},
                            "description": {"type": "string"},
                            "mimeType": {"type": "string"},
                        },
                        "required": ["server", "uriTemplate", "name"],
                    },
                },
                "nextCursor": {"type": "string"},
                "errors": listing_errors_schema(),
            },
            "required": ["resourceTemplates"],
        })
    } else {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "resources": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"},
                            "uri": {"type": "string"},
                            "name": {"type": "string"},
                            "title": {"type": "string"},
                            "description": {"type": "string"},
                            "mimeType": {"type": "string"},
                            "size": {"type": "number"},
                        },
                        "required": ["server", "uri", "name"],
                    },
                },
                "nextCursor": {"type": "string"},
                "errors": listing_errors_schema(),
            },
            "required": ["resources"],
        })
    }
}

fn read_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": {"type": "string"},
            "uri": {"type": "string"},
            "contents": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"uri": {"type": "string"}, "mimeType": {"type": "string"}, "text": {"type": "string"}},
                            "required": ["uri", "text"],
                        },
                        {
                            "type": "object",
                            "properties": {"uri": {"type": "string"}, "mimeType": {"type": "string"}, "blob": {"type": "string", "description": "base64"}},
                            "required": ["uri", "blob"],
                        },
                    ],
                },
            },
        },
        "required": ["server", "uri", "contents"],
    })
}

fn text_component(value: &str) -> ComponentTree {
    json!({"type": "text", "props": {"text": value}})
}

/// The three resource tools (resources.ts:298).
pub fn create_mcp_resource_tool_definitions(
    options: CreateResourceToolOptions,
) -> Vec<ToolDefinition> {
    let read_only = ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    };
    let list_tool = create_list_tool(
        LIST_MCP_RESOURCES_TOOL,
        "resources",
        "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.",
        options.exposure,
        options.servers.clone(),
        read_only.clone(),
        false,
    );
    let templates_tool = create_list_tool(
        LIST_MCP_RESOURCE_TEMPLATES_TOOL,
        "resourceTemplates",
        "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.",
        options.exposure,
        options.servers.clone(),
        read_only.clone(),
        true,
    );
    let read_tool = create_read_tool(options.exposure, options.servers, read_only);
    vec![list_tool, templates_tool, read_tool]
}

#[allow(clippy::too_many_arguments)]
fn create_list_tool(
    name: &'static str,
    key: &'static str,
    description: &'static str,
    exposure: super::config::McpExposure,
    servers: Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>,
    annotations: ToolAnnotations,
    templates: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: description.to_owned(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: parameters(),
        constrained_sampling: None,
        output_schema: Some(list_output_schema(templates)),
        exposure: exposure.to_tool_exposure(),
        namespace: None,
        annotations: Some(annotations),
        default_active: None,
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |request: ToolExecuteRequest, _context| {
            let servers = servers.clone();
            let params = request.params.clone();
            Box::pin(async move {
                let server_filter = string_argument(&params, "server")?;
                let cursor = string_argument(&params, "cursor")?;
                let mut payload = serde_json::Map::new();
                if let Some(server_name) = &server_filter {
                    let server = find_server(&servers, server_name)?;
                    let timeout = server.resource_server_timeout_ms();
                    let result = if templates {
                        server
                            .resource_templates_page(
                                cursor.clone(),
                                McpRequestOptions {
                                    timeout_ms: Some(timeout),
                                    ..Default::default()
                                },
                            )
                            .await
                    } else {
                        server
                            .resources_page(
                                cursor.clone(),
                                McpRequestOptions {
                                    timeout_ms: Some(timeout),
                                    ..Default::default()
                                },
                            )
                            .await
                    }
                    .map_err(|error| error.to_string())?;
                    payload.insert(
                        "server".to_owned(),
                        Value::String(server.resource_server_name().to_owned()),
                    );
                    payload.insert(
                        key.to_owned(),
                        Value::Array(
                            result
                                .items
                                .iter()
                                .filter(|item| {
                                    let uri = item
                                        .get("uri")
                                        .or_else(|| item.get("uriTemplate"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    !is_mcp_app_uri(
                                        uri,
                                        item.get("mimeType").and_then(Value::as_str),
                                    )
                                })
                                .map(|item| listed(server.resource_server_name(), item))
                                .collect(),
                        ),
                    );
                    if let Some(cursor) = result.next_cursor {
                        payload.insert("nextCursor".to_owned(), Value::String(cursor));
                    }
                } else {
                    if cursor.is_some() {
                        return Err("cursor can only be used when a server is specified".to_owned());
                    }
                    let mut all: Vec<Arc<dyn McpResourceServer>> = servers();
                    all.sort_by_key(|server| server.resource_server_name().to_owned());
                    let mut items: Vec<Value> = Vec::new();
                    let mut errors: Vec<Value> = Vec::new();
                    for server in all {
                        let timeout = server.resource_server_timeout_ms();
                        let result = if templates {
                            server
                                .all_resource_templates(McpRequestOptions {
                                    timeout_ms: Some(timeout),
                                    ..Default::default()
                                })
                                .await
                        } else {
                            server
                                .all_resources(McpRequestOptions {
                                    timeout_ms: Some(timeout),
                                    ..Default::default()
                                })
                                .await
                        };
                        match result {
                            Ok(values) => {
                                for item in values.iter().filter(|item| {
                                    let uri = item
                                        .get("uri")
                                        .or_else(|| item.get("uriTemplate"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    !is_mcp_app_uri(
                                        uri,
                                        item.get("mimeType").and_then(Value::as_str),
                                    )
                                }) {
                                    items.push(listed(server.resource_server_name(), item));
                                }
                            }
                            Err(error) => {
                                errors.push(json!({"server": server.resource_server_name(), "error": error.to_string()}));
                            }
                        }
                    }
                    payload.insert(key.to_owned(), Value::Array(items));
                    if !errors.is_empty() {
                        payload.insert("errors".to_owned(), Value::Array(errors));
                    }
                }
                json_result(name, server_filter.as_deref(), Value::Object(payload))
            })
        }),
        render_call: Some(Arc::new(|_context: ToolRenderContext| {
            Ok(text_component(name))
        })),
        render_result: Some(Arc::new(
            move |_result: AgentToolResult,
                  _options: ToolRenderResultOptions,
                  _context: ToolRenderContext| {
                Ok(json!({"type": "column", "children": []}))
            },
        )),
    }
}

fn create_read_tool(
    exposure: super::config::McpExposure,
    servers: Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>,
    annotations: ToolAnnotations,
) -> ToolDefinition {
    ToolDefinition {
        name: READ_MCP_RESOURCE_TOOL.to_owned(),
        label: READ_MCP_RESOURCE_TOOL.to_owned(),
        description:
            "Read a specific resource from an MCP server given the server name and resource URI."
                .to_owned(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: read_parameters(),
        constrained_sampling: None,
        output_schema: Some(read_output_schema()),
        exposure: exposure.to_tool_exposure(),
        namespace: None,
        annotations: Some(annotations),
        default_active: None,
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |request: ToolExecuteRequest, _context| {
            let servers = servers.clone();
            let params = request.params.clone();
            Box::pin(async move {
                let server_name = string_argument(&params, "server")?
                    .ok_or_else(|| "server must be provided".to_owned())?;
                let uri = string_argument(&params, "uri")?
                    .ok_or_else(|| "uri must be provided".to_owned())?;
                let server = find_server(&servers, &server_name)?;
                let timeout = server.resource_server_timeout_ms();
                let result = server
                    .read_resource(
                        &uri,
                        McpRequestOptions {
                            timeout_ms: Some(timeout),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let contents = result
                    .get("contents")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let multiple = contents.len() > 1;
                let blocks: Vec<Value> = contents
                    .iter()
                    .flat_map(|contents| {
                        let mut blocks: Vec<Value> = Vec::new();
                        if multiple {
                            blocks.push(json!({
                                "type": "text",
                                "text": format!("{}:", contents.get("uri").and_then(Value::as_str).unwrap_or_default()),
                            }));
                        } else {
                            // Several contents (for example a directory) are
                            // labeled with their URIs; the conversion below
                            // still needs a block with the resource.
                        }
                        blocks.push(json!({"type": "resource", "resource": contents}));
                        blocks
                    })
                    .collect();
                let converted =
                    to_model_content(&server_name, &blocks, &ConvertMcpResultOptions::default());
                let (content, full_output_path) = limit_mcp_content(
                    if converted.is_empty() {
                        vec![ToolResultContent::Text(TextContent {
                            text: format!("Resource {uri} is empty."),
                            text_signature: None,
                        })]
                    } else {
                        converted
                    },
                    &save_default(),
                );
                let script_contents: Vec<Value> = contents
                    .iter()
                    .map(|contents| {
                        let mut value = contents.clone();
                        if let Some(object) = value.as_object_mut() {
                            object.remove("_meta");
                        }
                        value
                    })
                    .collect();
                Ok(AgentToolResult {
                    content,
                    details: McpToolDetails {
                        server: server_name.clone(),
                        tool: READ_MCP_RESOURCE_TOOL.to_owned(),
                        full_output_path,
                    }
                    .to_json(),
                    structured_content: Some(json!({
                        "server": server_name,
                        "uri": uri,
                        "contents": script_contents,
                    })),
                    usage: None,
                    is_error: None,
                    terminate: None,
                })
            })
        }),
        render_call: Some(Arc::new(|_context: ToolRenderContext| {
            Ok(text_component(READ_MCP_RESOURCE_TOOL))
        })),
        render_result: Some(Arc::new(
            move |_result: AgentToolResult,
                  _options: ToolRenderResultOptions,
                  _context: ToolRenderContext| {
                Ok(json!({"type": "column", "children": []}))
            },
        )),
    }
}

fn find_server(
    servers: &Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>,
    name: &str,
) -> Result<Arc<dyn McpResourceServer>, String> {
    let all = servers();
    if let Some(server) = all
        .iter()
        .find(|server| server.resource_server_name() == name)
    {
        return Ok(server.clone());
    }
    let available: Vec<&str> = all
        .iter()
        .map(|server| server.resource_server_name())
        .collect();
    let suffix = if available.is_empty() {
        String::new()
    } else {
        format!(". Servers with resources: {}", available.join(", "))
    };
    Err(format!("MCP server \"{name}\" has no resources{suffix}"))
}

/// Convenience: whether any resource/namespace tool name is part of the
/// built-in resource trio.
pub fn is_resource_tool(name: &str) -> bool {
    matches!(
        name,
        LIST_MCP_RESOURCES_TOOL | LIST_MCP_RESOURCE_TEMPLATES_TOOL | READ_MCP_RESOURCE_TOOL
    )
}
