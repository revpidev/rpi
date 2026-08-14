//! Conformance client driver for `@modelcontextprotocol/conformance`
//! (task TE02 / design §6 O2): same CLI contract as the upstream
//! `conformance/driver.sh` — the referee spawns this binary once per
//! scenario with
//!
//!   MCP_CONFORMANCE_SCENARIO=<scenario> conformance-driver <server-url>
//!
//! and grades the wire traffic server-side. The MCP client under test is
//! this crate's own stack (`McpServerManager` + thin JSON-RPC client).
//!
//! Core scenarios (the auth/* matrix is TE03 scope):
//! - `initialize`: connect + discovery
//! - `tools_call`: callTool add_numbers {a:5,b:3}
//! - `sse-retry`: callTool test_reconnection — the server drops the SSE
//!   stream mid-call; the transport's reconnection scheduler reopens the
//!   GET stream (retry: delay, Last-Event-ID) and re-maps the replayed
//!   response to the pending request id.
//! - `elicitation-sep1034-*`: needs a server→client request handler
//!   (P2 scope; P0 answers -32601 → the tool call fails, expected mismatch)
//!
//! Exit code: 0 = scenario steps completed, non-zero = client failure.
//! Spawned only by the conformance referee; not part of `cargo test`.

use std::sync::Arc;
use std::time::Duration;

use rpi_ext_mcp_adapter::manager::{ConnectionStatus, McpServerManager};
use rpi_ext_mcp_adapter::metadata::ServerEntry;
use serde_json::{json, Value};

fn temp_workdir() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-conformance-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

/// One connect attempt; mirrors driver.ts `connectWithAuth` minus OAuth
/// (the auth/* scenarios are TE03 scope — those servers challenge with 401,
/// which the P0 client maps to the needs-auth status and this driver
/// reports as a failure).
async fn connect(
    manager: &Arc<McpServerManager>,
    definition: &ServerEntry,
) -> Result<Arc<rpi_ext_mcp_adapter::manager::ServerConnection>, String> {
    let connection = manager
        .connect("conformance", definition)
        .await
        .map_err(|error| format!("connect failed: {error}"))?;
    match connection.status() {
        ConnectionStatus::Connected => Ok(connection),
        ConnectionStatus::NeedsAuth => Err("needs-auth (OAuth is TE03 scope)".to_string()),
        ConnectionStatus::Closed => Err("connection closed during handshake".to_string()),
    }
}

async fn call_tool(
    manager: &Arc<McpServerManager>,
    definition: &ServerEntry,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    // sse-retry: the server deliberately drops the stream mid-call; retry
    // once through the manager after tearing the stale connection down
    // (driver.ts retries through its auth round-trip loop; the transport
    // retry itself is owned by the referee's grading).
    for attempt in 0..2 {
        let connection = connect(manager, definition).await?;
        if let Some(client) = &connection.client {
            match client
                .call_tool(name, args.clone(), Duration::from_secs(60))
                .await
            {
                Ok(result) => {
                    if result.get("isError") == Some(&json!(true)) {
                        return Err(format!(
                            "tool {name} returned an error result: {}",
                            result["content"]
                        ));
                    }
                    return Ok(result);
                }
                Err(error) => {
                    let message = error.to_string();
                    let transport_failure = matches!(
                        error,
                        rpi_ext_mcp_adapter::protocol::ProtocolError::Transport(_)
                            | rpi_ext_mcp_adapter::protocol::ProtocolError::Closed
                    );
                    if attempt == 0 && transport_failure {
                        manager.close("conformance").await;
                        continue;
                    }
                    return Err(format!("callTool {name} failed: {message}"));
                }
            }
        }
    }
    Err("callTool retry loop exhausted".to_string())
}

#[tokio::main]
async fn main() {
    let scenario = std::env::var("MCP_CONFORMANCE_SCENARIO").unwrap_or_default();
    let server_url = std::env::args().nth(1).unwrap_or_default();
    if scenario.is_empty() || server_url.is_empty() {
        eprintln!("Usage: MCP_CONFORMANCE_SCENARIO=<scenario> conformance-driver <server-url>");
        std::process::exit(1);
    }

    let definition = ServerEntry(
        json!({ "url": server_url })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let manager = McpServerManager::new(Some(temp_workdir()));

    let outcome: Result<(), String> = match scenario.as_str() {
        "initialize" => connect(&manager, &definition).await.map(|_| ()),
        "tools_call" => call_tool(
            &manager,
            &definition,
            "add_numbers",
            json!({ "a": 5, "b": 3 }),
        )
        .await
        .map(|_| ()),
        "sse-retry" => call_tool(&manager, &definition, "test_reconnection", json!({}))
            .await
            .map(|_| ()),
        "elicitation-sep1034-client-defaults" => call_tool(
            &manager,
            &definition,
            "test_client_elicitation_defaults",
            json!({}),
        )
        .await
        .map(|_| ()),
        other => Err(format!("Unsupported MCP conformance scenario: {other}")),
    };

    manager.close_all().await;
    if let Err(message) = outcome {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
