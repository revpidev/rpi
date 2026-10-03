//! MCP tool adaptation (port of
//! `packages/coding-agent/src/extensions/mcp/tools.ts` @ a13d35a74).
//!
//! Results map onto the model-facing content (text and images). Text over
//! 20 KB keeps its start and end with the middle cut out, and the full text
//! is saved to a mode-0600 temp file. Codemode scripts receive the whole
//! `CallToolResult` without `_meta`, never truncated. MCP errors
//! (`isError`) are error results for the model but still resolve for
//! scripts.

use std::io::Write;
use std::sync::Arc;

use rpi_agent::types::AgentToolResult;
use rpi_ai::types::{ImageContent, TextContent, ToolResultContent};
use rpi_ext_host::types::{
    ComponentTree, ToolAnnotations, ToolDefinition, ToolExecuteRequest, ToolExposure,
    ToolNamespace, ToolRenderContext, ToolRenderResultOptions,
};
use rpi_mcp::content::{LlmContent, block_to_llm_content};
use rpi_mcp::protocol::McpError;
use rpi_mcp::types::{CallToolResult, ProgressNotification, Tool as McpTool};
use serde_json::{Map, Value, json};

use super::config::McpExposure;

/// `MAX_TOOL_NAME_LENGTH` (tools.ts:49): provider tool names are limited to
/// 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_LENGTH: usize = 64;
/// `MCP_OUTPUT_MAX_BYTES` (tools.ts:46).
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;
/// `OUTPUT_PREVIEW_LINES` (tools.ts:51).
const OUTPUT_PREVIEW_LINES: usize = 5;
/// Tool that reads the resources named by resource links (tools.ts:53).
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";

/// `McpToolDetails` (tools.ts:61).
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDetails {
    pub server: String,
    pub tool: String,
    /// Temp file with the full text output, when the text was truncated.
    pub full_output_path: Option<String>,
}

impl McpToolDetails {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("server".to_owned(), Value::String(self.server.clone()));
        map.insert("tool".to_owned(), Value::String(self.tool.clone()));
        if let Some(path) = &self.full_output_path {
            map.insert("fullOutputPath".to_owned(), Value::String(path.clone()));
        }
        Value::Object(map)
    }
}

/// `McpOutputSaver` (tools.ts:71): data plus a dotted extension → path.
#[derive(Debug, Clone)]
pub enum McpOutputData {
    Text(String),
    Binary(Vec<u8>),
}

pub type McpOutputSaver = Arc<dyn Fn(&McpOutputData, &str) -> Result<String, String> + Send + Sync>;

/// `saveToTempFile` (tools.ts:75): results can carry private data, so only
/// the user may read the file.
pub fn save_to_temp_file(data: &McpOutputData, extension: &str) -> Result<String, String> {
    let directory = std::env::temp_dir();
    for attempt in 0..16u64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let name = format!("rpi-mcp-{nanos:016x}{attempt:02x}{extension}");
        let path = directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let bytes = match data {
                    McpOutputData::Text(text) => text.as_bytes(),
                    McpOutputData::Binary(bytes) => bytes,
                };
                file.write_all(bytes).map_err(|error| error.to_string())?;
                return Ok(path.display().to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not create a unique temp file".to_owned())
}

/// `createMcpToolName` (tools.ts:86): `mcp__<server>__<tool>`, everything
/// but `[A-Za-z0-9_]` replaced by `_`, hash-suffixed when too long or taken.
pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let hash = format!("{digest:x}");
    let hash = &hash[..8];
    let keep = (MAX_TOOL_NAME_LENGTH - hash.len() - 1).min(name.len());
    format!("{}_{hash}", &name[..keep])
}

fn text_of(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `createMcpResultSchema` (tools.ts:106): the `CallToolResult` scripts
/// receive, with the tool's own output schema as `structuredContent`.
pub fn create_mcp_result_schema(structured_content_schema: Option<&Value>) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "content".to_owned(),
        json!({"type": "array", "items": {"type": "object"}}),
    );
    if let Some(schema) = structured_content_schema {
        properties.insert("structuredContent".to_owned(), schema.clone());
    }
    properties.insert("isError".to_owned(), json!({"type": "boolean"}));
    properties.insert("_meta".to_owned(), json!({"type": "object"}));
    json!({"type": "object", "properties": properties, "required": ["content"]})
}

/// `limitMcpContent` (tools.ts:127): keep model-facing text within
/// [`MCP_OUTPUT_MAX_BYTES`]; longer text becomes one text block in Codex's
/// truncation format plus the path of the full output; images follow it.
pub fn limit_mcp_content(
    content: Vec<ToolResultContent>,
    save_output: &McpOutputSaver,
) -> (Vec<ToolResultContent>, Option<String>) {
    let combined = text_of(&content);
    let truncation = crate::tools::truncate::truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES);
    if !truncation.truncated {
        return (content, None);
    }
    let (full_output_path, where_line) = match save_output(&McpOutputData::Text(combined), ".txt") {
        Ok(path) => {
            let where_line = format!("[Full output: {path} (read it with offset/limit)]");
            (Some(path), where_line)
        }
        Err(error) => (None, format!("[Could not save the full output: {error}]")),
    };
    let tokens = truncation.total_bytes.div_ceil(4);
    let text = format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}\n\n{}",
        truncation.total_lines, truncation.content, where_line
    );
    let mut limited = vec![ToolResultContent::Text(TextContent {
        text,
        text_signature: None,
    })];
    limited.extend(
        content
            .into_iter()
            .filter(|block| matches!(block, ToolResultContent::Image(_))),
    );
    (limited, full_output_path)
}

/// `extensionOf` (tools.ts:158): the URI's dotted extension, else `.bin`.
fn extension_of(uri: &str) -> String {
    let path = url::Url::parse(uri)
        .map(|url| url.path().to_owned())
        .unwrap_or_else(|_| uri.to_owned());
    match path.rsplit_once('.') {
        Some((_, extension))
            if !extension.is_empty()
                && extension.len() <= 8
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()) =>
        {
            format!(".{extension}")
        }
        _ => ".bin".to_owned(),
    }
}

/// `isTextMimeType` (tools.ts:165).
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let mime_type = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime_type.starts_with("text/")
        || mime_type == "application/json"
        || mime_type.ends_with("+json")
        || mime_type.ends_with("+xml")
}

/// `ConvertMcpResultOptions` (tools.ts:150).
#[derive(Clone, Default)]
pub struct ConvertMcpResultOptions {
    /// Saves truncated text and binary resources (default: a temp file).
    pub save_output: Option<McpOutputSaver>,
    /// Whether resource links can name `read_mcp_resource`.
    pub readable_resources: Option<bool>,
}

impl ConvertMcpResultOptions {
    fn saver(&self) -> McpOutputSaver {
        self.save_output
            .clone()
            .unwrap_or_else(|| Arc::new(save_to_temp_file))
    }
}

/// `blockToContent` (tools.ts:178): model-facing content of one block.
fn block_to_content(
    server: &str,
    block: &Value,
    options: &ConvertMcpResultOptions,
) -> Vec<ToolResultContent> {
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if block_type == "resource_link" {
        let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
        let title = block
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| block.get("name").and_then(Value::as_str))
            .unwrap_or_default();
        let mut details: Vec<String> = Vec::new();
        if let Some(mime) = block.get("mimeType").and_then(Value::as_str) {
            details.push(mime.to_owned());
        }
        if let Some(size) = block.get("size").and_then(Value::as_u64) {
            details.push(crate::tools::truncate::format_size(size as usize));
        }
        let read = if options.readable_resources.unwrap_or(false) {
            format!(". Read it with {READ_MCP_RESOURCE_TOOL} (server \"{server}\")")
        } else {
            String::new()
        };
        let description = block
            .get("description")
            .and_then(Value::as_str)
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        let details = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join(", "))
        };
        return vec![ToolResultContent::Text(TextContent {
            text: format!("[Resource {uri} \"{title}\"{details}{description}{read}]"),
            text_signature: None,
        })];
    }
    if block_type == "resource"
        && let Some(resource) = block.get("resource")
        && let Some(blob) = resource.get("blob").and_then(Value::as_str)
        && !resource
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/"))
    {
        use base64::Engine;
        let uri = resource
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mime_type = resource.get("mimeType").and_then(Value::as_str);
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(blob) else {
            return Vec::new();
        };
        if is_text_mime_type(mime_type) {
            return vec![ToolResultContent::Text(TextContent {
                text: String::from_utf8_lossy(&data).into_owned(),
                text_signature: None,
            })];
        }
        let kind = format!(
            "{}, {}",
            mime_type.unwrap_or("unknown type"),
            crate::tools::truncate::format_size(data.len())
        );
        return match (options.saver())(&McpOutputData::Binary(data), &extension_of(uri)) {
            Ok(path) => vec![ToolResultContent::Text(TextContent {
                text: format!("[Binary resource {uri} ({kind}) saved to {path}]"),
                text_signature: None,
            })],
            Err(error) => vec![ToolResultContent::Text(TextContent {
                text: format!("[Binary resource {uri} ({kind}) could not be saved: {error}]"),
                text_signature: None,
            })],
        };
    }
    match block_to_llm_content(block) {
        LlmContent::Text { text } => vec![ToolResultContent::Text(TextContent {
            text,
            text_signature: None,
        })],
        LlmContent::Image { data, mime_type } => {
            vec![ToolResultContent::Image(ImageContent { data, mime_type })]
        }
    }
}

/// `toModelContent` (tools.ts:216): model-facing content of `server`'s
/// blocks, before the output limit.
pub fn to_model_content(
    server: &str,
    blocks: &[Value],
    options: &ConvertMcpResultOptions,
) -> Vec<ToolResultContent> {
    blocks
        .iter()
        .flat_map(|block| block_to_content(server, block, options))
        .collect()
}

/// `convertMcpResult` (tools.ts:224): `isError` results become error
/// results that keep the structured result.
pub fn convert_mcp_result(
    server: &str,
    tool: &str,
    result: &CallToolResult,
    options: &ConvertMcpResultOptions,
) -> AgentToolResult {
    let mut converted: Vec<ToolResultContent> = if !result.content.is_empty() {
        to_model_content(server, &result.content, options)
    } else {
        rpi_mcp::to_llm_content(result)
            .into_iter()
            .map(|block| match block {
                LlmContent::Text { text } => ToolResultContent::Text(TextContent {
                    text,
                    text_signature: None,
                }),
                LlmContent::Image { data, mime_type } => {
                    ToolResultContent::Image(ImageContent { data, mime_type })
                }
            })
            .collect()
    };
    if result.is_error == Some(true) && text_of(&converted).is_empty() {
        converted.push(ToolResultContent::Text(TextContent {
            text: format!("MCP tool {server}/{tool} returned an error"),
            text_signature: None,
        }));
    }
    let (content, full_output_path) = limit_mcp_content(converted, &options.saver());
    let script_result = json!({
        "content": result.content,
        "structuredContent": result.structured_content,
        "isError": result.is_error,
    });
    AgentToolResult {
        content,
        details: McpToolDetails {
            server: server.to_owned(),
            tool: tool.to_owned(),
            full_output_path,
        }
        .to_json(),
        structured_content: Some(script_result),
        usage: None,
        is_error: result.is_error,
        terminate: None,
    }
}

/// `toParameters` (tools.ts:263): MCP servers may omit `type`, and some
/// providers reject object schemas without `properties`.
fn to_parameters(schema: &Value) -> Value {
    let mut schema = schema.as_object().cloned().unwrap_or_default();
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    schema.entry("properties").or_insert_with(|| json!({}));
    Value::Object(schema)
}

/// `toToolAnnotations` (tools.ts:272).
fn to_tool_annotations(tool: &McpTool) -> Option<ToolAnnotations> {
    let source = tool.annotations.as_ref()?;
    let annotations = ToolAnnotations {
        read_only_hint: source.read_only_hint,
        destructive_hint: source.destructive_hint,
        idempotent_hint: source.idempotent_hint,
        open_world_hint: source.open_world_hint,
    };
    if annotations.read_only_hint.is_none()
        && annotations.destructive_hint.is_none()
        && annotations.idempotent_hint.is_none()
        && annotations.open_world_hint.is_none()
    {
        return None;
    }
    Some(annotations)
}

/// `McpToolCaller` (tools.ts:80): the connection seam a tool definition
/// calls through.
#[async_trait::async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        options: McpCallOptions,
    ) -> Result<CallToolResult, McpError>;
}

/// Looks up the connection a tool definition calls through.
pub type McpClientLookup = Arc<
    dyn Fn() -> futures::future::BoxFuture<'static, Result<Arc<dyn McpToolCaller>, String>>
        + Send
        + Sync,
>;

/// Progress callback a tool call forwards to the UI.
pub type ProgressCallback = Arc<dyn Fn(&ProgressNotification) + Send + Sync>;

/// `McpRequestOptions` subset the tool passes through.
#[derive(Clone, Default)]
pub struct McpCallOptions {
    pub signal: Option<tokio_util::sync::CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub on_progress: Option<ProgressCallback>,
}

/// `createMcpToolDefinition` (tools.ts:279).
pub struct CreateMcpToolOptions {
    pub server: String,
    pub tool: McpTool,
    pub name: String,
    pub exposure: McpExposure,
    pub namespace: ToolNamespace,
    pub timeout_ms: u64,
    pub get_client: McpClientLookup,
    pub readable_resources: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

fn format_tool_call(label: &str, args: &Value, expanded: bool) -> String {
    if expanded {
        let mut lines = vec![label.to_owned()];
        if let Some(map) = args.as_object()
            && !map.is_empty()
        {
            for (key, value) in map {
                lines.push(format!("  {key}: {value}"));
            }
        }
        return lines.join("\n");
    }
    match args.as_object() {
        Some(map) if !map.is_empty() => {
            let pairs: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            format!("{label} ({})", pairs.join(", "))
        }
        _ => label.to_owned(),
    }
}

fn text_component(text: String) -> ComponentTree {
    json!({"type": "text", "props": {"text": text}})
}

/// `createMcpToolDefinition`: the host [`ToolDefinition`] for one MCP tool.
pub fn create_mcp_tool_definition(options: CreateMcpToolOptions) -> ToolDefinition {
    let server = options.server.clone();
    let tool_name = options.tool.name.clone();
    let label = format!("{server}/{tool_name}");
    let title = options.tool.title.clone().or_else(|| {
        options
            .tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.title.clone())
    });
    let description = options
        .tool
        .description
        .clone()
        .filter(|description| !description.trim().is_empty())
        .map(|description| description.trim().to_owned())
        .or_else(|| title.clone())
        .unwrap_or_else(|| format!("MCP tool {tool_name} from server {server}"));
    let annotations = to_tool_annotations(&options.tool);
    let parameters = to_parameters(&options.tool.input_schema);
    let output_schema = create_mcp_result_schema(options.tool.output_schema.as_ref());
    let exposure = options.exposure;
    let namespace = options.namespace.clone();
    let timeout_ms = options.timeout_ms;
    let get_client = options.get_client.clone();
    let readable_resources = options.readable_resources.clone();
    let tool_id = tool_name.clone();
    let render_label = label.clone();

    ToolDefinition {
        name: options.name,
        label: label.clone(),
        description,
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters,
        constrained_sampling: None,
        output_schema: Some(output_schema),
        exposure: exposure.to_tool_exposure(),
        namespace: Some(namespace),
        annotations,
        default_active: None,
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |mut request: ToolExecuteRequest, _context| {
            let get_client = get_client.clone();
            let tool_id = tool_id.clone();
            let server = server.clone();
            let signal = request.signal.clone();
            let params = request.params.clone();
            let on_update = request.on_update.take();
            let readable_resources = readable_resources.clone();
            Box::pin(async move {
                let connection = get_client().await?;
                let on_progress: Option<ProgressCallback> = on_update.map(|callback| {
                    let server = server.clone();
                    let tool_id = tool_id.clone();
                    Arc::new(move |progress: &ProgressNotification| {
                        let total = progress
                            .total
                            .map(|total| format!("/{total}"))
                            .unwrap_or_default();
                        let text = progress
                            .message
                            .clone()
                            .unwrap_or_else(|| format!("Progress {}{total}", progress.progress));
                        callback(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text,
                                text_signature: None,
                            })],
                            details: McpToolDetails {
                                server: server.clone(),
                                tool: tool_id.clone(),
                                full_output_path: None,
                            }
                            .to_json(),
                            structured_content: None,
                            usage: None,
                            is_error: None,
                            terminate: None,
                        });
                    }) as Arc<dyn Fn(&ProgressNotification) + Send + Sync>
                });
                let result = connection
                    .call_tool(
                        &tool_id,
                        if params.is_null() { json!({}) } else { params },
                        McpCallOptions {
                            signal: Some(signal),
                            timeout_ms: Some(timeout_ms),
                            on_progress,
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let readable = readable_resources
                    .as_ref()
                    .map(|readable| readable())
                    .unwrap_or(false);
                Ok(convert_mcp_result(
                    &server,
                    &tool_id,
                    &result,
                    &ConvertMcpResultOptions {
                        save_output: None,
                        readable_resources: Some(readable),
                    },
                ))
            })
        }),
        render_call: Some(Arc::new(move |context: ToolRenderContext| {
            Ok(text_component(format_tool_call(
                &render_label,
                &context.args,
                context.expanded,
            )))
        })),
        render_result: Some(Arc::new(
            move |result: AgentToolResult,
                  render: ToolRenderResultOptions,
                  _context: ToolRenderContext| {
                let output = text_of(&result.content);
                if output.trim().is_empty() {
                    return Ok(json!({"type": "column", "children": []}));
                }
                let mut children: Vec<ComponentTree> = Vec::new();
                let lines: Vec<&str> = output.lines().collect();
                if render.expanded || lines.len() <= OUTPUT_PREVIEW_LINES {
                    for line in lines {
                        children.push(text_component(line.replace('\t', "    ")));
                    }
                } else {
                    for line in lines.iter().take(OUTPUT_PREVIEW_LINES) {
                        children.push(text_component(line.replace('\t', "    ")));
                    }
                    let hidden = lines.len() - OUTPUT_PREVIEW_LINES;
                    children.push(json!({
                        "type": "text",
                        "props": {"text": format!("... ({hidden} more lines)"), "fg": "muted"},
                    }));
                }
                if let Some(path) = result.details.get("fullOutputPath").and_then(Value::as_str) {
                    children.push(json!({
                        "type": "text",
                        "props": {"text": format!("Full output: {path}"), "fg": "muted"},
                    }));
                }
                let style = if result.is_error == Some(true) {
                    "error"
                } else {
                    "toolOutput"
                };
                Ok(json!({"type": "column", "children": children, "props": {"fg": style}}))
            },
        )),
    }
}

/// Whether a registered tool definition has the given MCP namespace.
pub fn definition_namespace(definition: &ToolDefinition) -> Option<&str> {
    definition
        .namespace
        .as_ref()
        .map(|namespace| namespace.name.as_str())
}

/// Exposure a tool definition carries.
pub fn definition_exposure(definition: &ToolDefinition) -> ToolExposure {
    definition.exposure
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_sanitize_and_hash() {
        assert_eq!(
            create_mcp_tool_name("dev-radius", "read", |_| false),
            "mcp__dev_radius__read"
        );
        assert_eq!(
            create_mcp_tool_name("s", "a-b.c", |_| false),
            "mcp__s__a_b_c"
        );
        let taken = create_mcp_tool_name("s", "read", |_| true);
        assert!(taken.starts_with("mcp__s__read_"));
        assert_eq!(taken.len(), "mcp__s__read".len() + 1 + 8);
        let long = create_mcp_tool_name("s", &"x".repeat(100), |_| false);
        assert_eq!(long.len(), MAX_TOOL_NAME_LENGTH);
    }

    #[test]
    fn truncation_keeps_head_and_tail_and_saves_the_full_text() {
        let saved: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saver: McpOutputSaver = {
            let saved = saved.clone();
            Arc::new(move |data, extension| {
                let McpOutputData::Text(text) = data else {
                    return Err("expected text".to_owned());
                };
                saved.lock().unwrap().push(format!("{text}::{extension}"));
                Ok("/tmp/full.txt".to_owned())
            })
        };
        let big = "a".repeat(MCP_OUTPUT_MAX_BYTES + 100);
        let (content, path) = limit_mcp_content(
            vec![ToolResultContent::Text(TextContent {
                text: big.clone(),
                text_signature: None,
            })],
            &saver,
        );
        assert_eq!(path.as_deref(), Some("/tmp/full.txt"));
        let text = text_of(&content);
        assert!(text.contains("chars truncated"), "{text}");
        assert!(text.contains("[Full output: /tmp/full.txt"), "{text}");
        assert_eq!(saved.lock().unwrap().len(), 1);

        let short = "hello".to_owned();
        let (content, path) = limit_mcp_content(
            vec![ToolResultContent::Text(TextContent {
                text: short.clone(),
                text_signature: None,
            })],
            &saver,
        );
        assert!(path.is_none());
        assert_eq!(text_of(&content), short);
    }

    #[test]
    fn converts_results_and_keeps_structured_content() {
        let result = CallToolResult {
            content: vec![json!({"type": "text", "text": "ok"})],
            structured_content: Some(json!({"count": 3})),
            is_error: Some(true),
            meta: Some(json!({"ignored": true})),
        };
        let converted = convert_mcp_result("s", "t", &result, &ConvertMcpResultOptions::default());
        assert_eq!(converted.is_error, Some(true));
        let structured = converted.structured_content.unwrap();
        assert_eq!(structured["structuredContent"], json!({"count": 3}));
        assert!(structured.get("_meta").is_none());
        assert!(text_of(&converted.content).contains("ok"));
        assert_eq!(
            converted.details.get("server").and_then(Value::as_str),
            Some("s")
        );
    }

    #[test]
    fn resource_links_name_the_read_tool() {
        let blocks = vec![json!({
            "type": "resource_link",
            "uri": "file:///x",
            "name": "x",
            "size": 2048,
        })];
        let content = to_model_content(
            "docs",
            &blocks,
            &ConvertMcpResultOptions {
                save_output: None,
                readable_resources: Some(true),
            },
        );
        let text = text_of(&content);
        assert!(
            text.contains("Read it with read_mcp_resource (server \"docs\")"),
            "{text}"
        );
        assert!(text.contains("2.0KB"), "{text}");
        let content = to_model_content(
            "docs",
            &blocks,
            &ConvertMcpResultOptions {
                save_output: None,
                readable_resources: Some(false),
            },
        );
        assert!(!text_of(&content).contains("read_mcp_resource"));
    }

    #[test]
    fn parameters_default_to_object_with_properties() {
        let parameters = to_parameters(&json!({"properties": {"a": {"type": "string"}}}));
        assert_eq!(parameters["type"], "object");
        let parameters = to_parameters(&json!({}));
        assert_eq!(parameters["properties"], json!({}));
    }
}
