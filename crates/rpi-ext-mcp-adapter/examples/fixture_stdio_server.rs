//! Fixture MCP stdio server for `rpi-ext-mcp-adapter` integration tests
//! (design §5.2). Speaks JSON-RPC over stdin/stdout (LF-delimited), logs the
//! received frame methods to `RPI_MCP_FIXTURE_LOG` (one per line), writes
//! its pid to `RPI_MCP_FIXTURE_PID` (for no-leftover-process assertions),
//! and answers:
//!
//! - `initialize` → protocolVersion 2025-03-26, tools+resources+prompts
//!   capabilities, serverInfo `fixture/0.1`, instructions "fixture
//!   instructions"
//! - `tools/list` → `echo` (schema with `query`), `fail` (isError result),
//!   `read_config` (resource tool handled via resources/read separately)
//! - `tools/call` → echo returns the `query` arg; fail returns isError
//! - `resources/list` → one `config` resource; `resources/read` → text
//!   contents; `prompts/list` → one prompt; `ping` → {}
//! - anything else → -32601
//!
//! Built by `cargo test` as an example; never invoked directly by users.

use std::io::{BufRead, Write};

fn main() {
    if let Ok(path) = std::env::var("RPI_MCP_FIXTURE_PID") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    let log_path = std::env::var("RPI_MCP_FIXTURE_LOG").ok();
    let log = |frame: &str| {
        if let Some(path) = &log_path {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{frame}");
            }
        }
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        log(method);
        let id = message.get("id").cloned();
        let Some(id) = id else { continue }; // notifications: no response
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                "serverInfo": { "name": "fixture", "version": "0.1" },
                "instructions": "fixture instructions",
            }),
            "ping" => serde_json::json!({}),
            "tools/list" => serde_json::json!({
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo the query back",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "query": { "type": "string" } },
                            "required": ["query"],
                        },
                    },
                    {
                        "name": "fail",
                        "description": "Always fails",
                        "inputSchema": { "type": "object", "properties": {} },
                    },
                ],
            }),
            "tools/call" => {
                let name = message
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                if name == "fail" {
                    serde_json::json!({
                        "isError": true,
                        "content": [{ "type": "text", "text": "boom" }],
                    })
                } else {
                    let query = message
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("query"))
                        .and_then(|q| q.as_str())
                        .unwrap_or("");
                    serde_json::json!({
                        "content": [{ "type": "text", "text": query }],
                    })
                }
            }
            "resources/list" => serde_json::json!({
                "resources": [{ "uri": "fixture://config", "name": "Config" }],
            }),
            "resources/read" => serde_json::json!({
                "contents": [{ "uri": "fixture://config", "text": "resource-body" }],
            }),
            "prompts/list" => serde_json::json!({
                "prompts": [{ "name": "standup", "description": "Standup notes" }],
            }),
            _ => {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" },
                });
                let mut out = stdout.lock();
                let _ = serde_json::to_writer(&mut out, &response);
                let _ = writeln!(out);
                let _ = out.flush();
                continue;
            }
        };
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let mut out = stdout.lock();
        let _ = serde_json::to_writer(&mut out, &response);
        let _ = writeln!(out);
        let _ = out.flush();
    }
}
