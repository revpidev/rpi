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
    // 1-based tools/list counter (SLOW_TOOLS_LIST_FROM knob).
    let tools_list_count = std::cell::Cell::new(0u64);
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
    // Server-initiated notifications queued while handling a line (the
    // listen acknowledgement / proactive list_changed), flushed after the
    // response (or immediately for notification-only handling).
    let mut notifications: Vec<serde_json::Value> = Vec::new();
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
        let flush_notifications = |queue: &mut Vec<serde_json::Value>| {
            let mut out = stdout.lock();
            for notification in queue.drain(..) {
                let _ = serde_json::to_writer(&mut out, &notification);
                let _ = writeln!(out);
            }
            let _ = out.flush();
        };
        let Some(id) = id else {
            // Notifications from the client (no response expected); the
            // match arms may queue server-initiated notifications.
            handle_notification(&message, &mut notifications);
            flush_notifications(&mut notifications);
            continue;
        };
        let result = match method {
            "initialize" => {
                // TE24 listen knobs: MODERN_2026 negotiates the 2026-07-28
                // era with listChanged capabilities (drives
                // subscriptions/listen); NOTIFY_TOOLS_CHANGED proactively
                // sends notifications/tools/list_changed after the
                // initialized notification.
                let modern = std::env::var("RPI_MCP_FIXTURE_MODERN_2026").is_ok();
                let capabilities = if modern {
                    serde_json::json!({
                        "tools": { "listChanged": true },
                        "resources": { "listChanged": true },
                        "prompts": { "listChanged": true },
                    })
                } else {
                    serde_json::json!({ "tools": {}, "resources": {}, "prompts": {} })
                };
                serde_json::json!({
                    "protocolVersion": if modern { "2026-07-28" } else { "2025-03-26" },
                    "capabilities": capabilities,
                    "serverInfo": { "name": "fixture", "version": "0.1" },
                    "instructions": "fixture instructions",
                })
            }
            "notifications/initialized" => {
                // `initialized` normally arrives as a NOTIFICATION (no id);
                // a request-shaped variant (some SDKs) answers empty and
                // the proactive list_changed rides the shared handler.
                serde_json::Value::Null
            }
            "subscriptions/listen" => {
                // #468: acknowledge with the subscription id mirror, then
                // hold the stream open (graceful close = a later RESULT for
                // the same string id; CANCEL_LISTEN_AFTER_MS models a
                // server-side cancel so the manager's re-establish path is
                // exercisable).
                let listen_id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                notifications.push(serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/subscriptions/acknowledged",
                    "params": {
                        "_meta": { "io.modelcontextprotocol/subscriptionId": listen_id.clone() },
                        "notifications": message
                            .get("params")
                            .and_then(|p| p.get("notifications"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    },
                }));
                let cancel_ms: u64 = std::env::var("RPI_MCP_FIXTURE_CANCEL_LISTEN_AFTER_MS")
                    .ok()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or(0);
                if cancel_ms > 0 {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(cancel_ms));
                        let stdout = std::io::stdout();
                        let mut out = stdout.lock();
                        let cancel = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/cancelled",
                            "params": { "requestId": listen_id },
                        });
                        let _ = serde_json::to_writer(&mut out, &cancel);
                        let _ = writeln!(out);
                        let _ = out.flush();
                    });
                }
                serde_json::Value::Null
            }
            "ping" => serde_json::json!({}),
            "tools/list" => {
                // TE24 keep-alive test knobs (defaults keep the historical
                // shape): EXTRA_TOOL grows the catalog (refresh detection),
                // SLOW_TOOLS_LIST_MS delays the response (bounded refresh
                // timeout).
                let mut tools = vec![
                    serde_json::json!({
                        "name": "echo",
                        "description": "Echo the query back",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "query": { "type": "string" } },
                            "required": ["query"],
                        },
                    }),
                    serde_json::json!({
                        "name": "fail",
                        "description": "Always fails",
                        "inputSchema": { "type": "object", "properties": {} },
                    }),
                ];
                if std::env::var("RPI_MCP_FIXTURE_EXTRA_TOOL").is_ok() {
                    tools.push(serde_json::json!({
                        "name": "extra",
                        "description": "Appears when RPI_MCP_FIXTURE_EXTRA_TOOL is set",
                        "inputSchema": { "type": "object", "properties": {} },
                    }));
                }
                // Delay from the Nth tools/list onward (1-based): the
                // initialize-time listing stays fast so `requestTimeoutMs`
                // can stay realistic for the handshake.
                let slow_ms: u64 = std::env::var("RPI_MCP_FIXTURE_SLOW_TOOLS_LIST_MS")
                    .ok()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or(0);
                let slow_from: u64 = std::env::var("RPI_MCP_FIXTURE_SLOW_TOOLS_LIST_FROM")
                    .ok()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or(1);
                let seen = tools_list_count.get();
                tools_list_count.set(seen + 1);
                if slow_ms > 0 && seen + 1 >= slow_from {
                    std::thread::sleep(std::time::Duration::from_millis(slow_ms));
                }
                serde_json::json!({ "tools": tools })
            }
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
        // The listen acknowledgment rides BEFORE the (empty) response on
        // the wire — both are written together here; the SUBSCRIPTIONS arm
        // returns Null (rendered as an empty result below).
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        {
            let mut out = stdout.lock();
            for notification in notifications.drain(..) {
                let _ = serde_json::to_writer(&mut out, &notification);
                let _ = writeln!(out);
            }
            let _ = serde_json::to_writer(&mut out, &response);
            let _ = writeln!(out);
            let _ = out.flush();
        }
    }
}

/// Notification-only handling (no id on the wire): with
/// NOTIFY_TOOLS_CHANGED_DELAY_MS set, schedule the proactive
/// notifications/tools/list_changed on a writer thread — the delay models
/// a real server changing its catalog WELL after the handshake completes
/// (an immediate push would race the client's connection publication and
/// be dropped, exactly like upstream's handler guard).
fn handle_notification(message: &serde_json::Value, _notifications: &mut Vec<serde_json::Value>) {
    if message.get("method").and_then(|m| m.as_str()) == Some("notifications/initialized") {
        let delay_ms: u64 = std::env::var("RPI_MCP_FIXTURE_NOTIFY_TOOLS_CHANGED_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(0);
        if delay_ms > 0 {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                });
                let _ = serde_json::to_writer(&mut out, &notification);
                let _ = writeln!(out);
                let _ = out.flush();
            });
        }
    }
}
