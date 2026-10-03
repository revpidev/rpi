//! G3 built-in MCP parity driver (V16-08 §6): drives the shared fixture MCP
//! server (`scripts/mcp-parity/fixture-server.mjs`) through this crate's
//! `McpClient` + `StdioTransport` and prints one normalized JSON result
//! document to stdout.
//!
//! The upstream reference is `external/pi/packages/mcp` @ a13d35a74, driven
//! by `scripts/mcp-parity/builtin-upstream-runner.mjs` with the exact same
//! step sequence; the orchestrator
//! `scripts/mcp-parity/run-builtin-mcp-parity.mjs` diffs the two documents.
//!
//! Steps (identical on both sides): connect → `tools/list` →
//! `tools/call echo` → `tools/call fail` → `resources/read`. Frames are
//! recorded server-side by the fixture (`RPI_MCP_FIXTURE_LOG_FRAMES=1`) and
//! normalized here (`id` → `$id`, `clientInfo.name` → `parity-client`), so a
//! diff isolates client-implementation differences.
//!
//! Env:
//!   RPI_MCP_FIXTURE_SERVER   path to fixture-server.mjs (required)
//!   RPI_MCP_FIXTURE_LOG      frame transcript path (required)
//!   RPI_MCP_PARITY_NODE_PATH node binary (default: `node`)

use std::sync::Arc;
use std::time::Duration;

use rpi_mcp::transport::stdio::StdioTransportOptions;
use rpi_mcp::{CallToolResult, McpClient, McpClientOptions, McpRequestOptions, StdioTransport};
use serde_json::{Map, Value, json};

/// Recursive frame normalization, mirroring `upstream-runner.mjs`'s
/// `normalizeValue`: JSON-RPC ids collapse to `$id`, the `clientInfo` brand
/// (O1 exemption) collapses to `parity-client`.
fn normalize_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalize_value).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if key == "id" && (item.is_number() || item.is_string()) {
                    out.insert(key.clone(), json!("$id"));
                } else if key == "clientInfo" && item.get("name").and_then(Value::as_str).is_some()
                {
                    let mut info = item.as_object().cloned().unwrap_or_default();
                    info.insert("name".to_owned(), json!("parity-client"));
                    out.insert(key.clone(), Value::Object(info));
                } else {
                    out.insert(key.clone(), normalize_value(item));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// The wire shape of one `tools/list` entry: `Tool` keeps unknown fields in
/// `extra`, so rebuilding the JSON here reproduces what the upstream client
/// returns verbatim (the fixture uses only the modeled fields).
fn tool_json(tool: &rpi_mcp::Tool) -> Value {
    let mut map = tool.extra.clone();
    map.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(title) = &tool.title {
        map.insert("title".to_owned(), Value::String(title.clone()));
    }
    if let Some(description) = &tool.description {
        map.insert("description".to_owned(), Value::String(description.clone()));
    }
    map.insert("inputSchema".to_owned(), tool.input_schema.clone());
    if let Some(output_schema) = &tool.output_schema {
        map.insert("outputSchema".to_owned(), output_schema.clone());
    }
    if let Some(annotations) = &tool.annotations
        && let Ok(value) = serde_json::to_value(annotations)
    {
        map.insert("annotations".to_owned(), value);
    }
    if let Some(execution) = &tool.execution {
        map.insert("execution".to_owned(), execution.clone());
    }
    if let Some(meta) = &tool.meta {
        map.insert("_meta".to_owned(), meta.clone());
    }
    Value::Object(map)
}

/// The LLM-facing subset of `CallToolResult`, in the same JSON shape the
/// upstream client returns (`content` always present).
fn call_result_json(result: &CallToolResult) -> Value {
    let mut map = Map::new();
    map.insert("content".to_owned(), Value::Array(result.content.clone()));
    if let Some(structured) = &result.structured_content {
        map.insert("structuredContent".to_owned(), structured.clone());
    }
    if let Some(is_error) = result.is_error {
        map.insert("isError".to_owned(), Value::Bool(is_error));
    }
    Value::Object(map)
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let fixture = std::env::var("RPI_MCP_FIXTURE_SERVER")
        .map_err(|_| "RPI_MCP_FIXTURE_SERVER is required".to_owned())?;
    let log_path = std::env::var("RPI_MCP_FIXTURE_LOG")
        .map_err(|_| "RPI_MCP_FIXTURE_LOG is required".to_owned())?;
    let node = std::env::var("RPI_MCP_PARITY_NODE_PATH").unwrap_or_else(|_| "node".to_owned());

    let mut options = StdioTransportOptions::new(node);
    options.args = vec![fixture];
    options
        .env
        .insert("RPI_MCP_FIXTURE_LOG".to_owned(), log_path.clone());
    options
        .env
        .insert("RPI_MCP_FIXTURE_LOG_FRAMES".to_owned(), "1".to_owned());

    let transport = Arc::new(StdioTransport::new(options));
    let client = McpClient::new(McpClientOptions::new("rpi-mcp-parity", "1.0.0"));

    let mut output = json!({
        "side": "rpi",
        "transport": "stdio",
        "frames": [],
        "results": {},
        "status": "",
    });

    let outcome: Result<(), String> = async {
        client
            .connect(transport.clone())
            .await
            .map_err(|error| format!("connect failed: {error}"))?;
        output["status"] = json!("connected");

        let tools = client
            .list_tools(McpRequestOptions {
                timeout_ms: Some(10_000),
                ..McpRequestOptions::default()
            })
            .await
            .map_err(|error| format!("tools/list failed: {error}"))?;
        output["results"]["tools"] = Value::Array(tools.iter().map(tool_json).collect());

        let echo = client
            .call_tool(
                "echo",
                Some(json!({ "query": "hello" })),
                McpRequestOptions {
                    timeout_ms: Some(10_000),
                    ..McpRequestOptions::default()
                },
            )
            .await
            .map_err(|error| format!("echo call failed: {error}"))?;
        output["results"]["echo"] = call_result_json(&echo);

        let fail_call = match client
            .call_tool(
                "fail",
                Some(json!({})),
                McpRequestOptions {
                    timeout_ms: Some(10_000),
                    ..McpRequestOptions::default()
                },
            )
            .await
        {
            Ok(result) => json!({ "threw": false, "result": call_result_json(&result) }),
            Err(error) => json!({ "threw": true, "name": error_name(&error) }),
        };
        output["results"]["failCall"] = fail_call;

        let resource = client
            .read_resource(
                "fixture://config",
                McpRequestOptions {
                    timeout_ms: Some(10_000),
                    ..McpRequestOptions::default()
                },
            )
            .await
            .map_err(|error| format!("resources/read failed: {error}"))?;
        output["results"]["readResource"] = normalize_value(&resource);

        client
            .close()
            .await
            .map_err(|error| format!("close failed: {error}"))?;
        Ok(())
    }
    .await;

    // The fixture records the frame transcript server-side; read it back
    // after the connection drained (retry briefly: the child may still be
    // flushing its last line).
    let mut raw = String::new();
    for _ in 0..50 {
        raw = std::fs::read_to_string(&log_path).unwrap_or_default();
        if !raw.trim().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let frames: Vec<Value> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|frame| normalize_value(&frame))
        .collect();
    output["frames"] = Value::Array(frames);

    if let Err(message) = outcome {
        output["status"] = json!("error");
        output["error"] = json!(message);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&output)
            .map_err(|error| format!("output serialization failed: {error}"))?
    );
    Ok(())
}

/// The upstream runner reports `error.constructor.name` for a thrown call;
/// rpi uses the typed error's stable name.
fn error_name(error: &rpi_mcp::McpError) -> String {
    match error {
        rpi_mcp::McpError::ConnectionClosed(_) => "McpConnectionClosedError",
        rpi_mcp::McpError::Timeout { .. } => "McpTimeoutError",
        rpi_mcp::McpError::Transport(_) => "McpTransportError",
        _ => "McpError",
    }
    .to_owned()
}
