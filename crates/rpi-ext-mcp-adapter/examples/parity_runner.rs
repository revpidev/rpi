//! Cross-implementation parity: rpi (Rust) side runner (design §5.2).
//!
//! Drives this crate's `McpServerManager` through the same step sequence as
//! `scripts/mcp-parity/upstream-runner.mjs` against the shared fixture
//! server and prints one normalized JSON document to stdout:
//!
//!   { side, transport, frames, results, status }
//!
//! Frame transcripts are recorded server-side by the fixture server
//! (`RPI_MCP_FIXTURE_LOG_FRAMES=1`); the runner normalizes volatile
//! JSON-RPC ids to `$id` the same way the Node side does.
//!
//! Scenarios (`RPI_MCP_PARITY_SCENARIO`): `stdio` (Node fixture server as
//! child process) and `http` (fixture server in HTTP mode; the profile —
//! streamable-json / fallback-404|405|406|415 / auth-401 — is chosen by the
//! orchestrator via `RPI_MCP_FIXTURE_HTTP_PROFILE`).
//!
//! Spawned only by `scripts/mcp-parity/run-mcp-parity.mjs`; not part of
//! `cargo test`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rpi_ext_mcp_adapter::manager::{ConnectionStatus, McpServerManager};
use rpi_ext_mcp_adapter::metadata::ServerEntry;
use serde_json::{json, Value};

fn here(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("rpi-mcp-parity-rust-{tag}-{nanos}"))
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn normalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                let normalized = if key == "id" {
                    match item {
                        Value::Number(_) | Value::String(_) => json!("$id"),
                        other => normalize(other),
                    }
                } else if key == "clientInfo" {
                    // O1 brand exemption: upstream `pi-mcp-<server>` vs rpi
                    // `rpi-mcp-<server>` (protocol.rs client_info).
                    match normalize(item) {
                        Value::Object(mut info) => {
                            info.insert("name".into(), json!("parity-client"));
                            Value::Object(info)
                        }
                        other => other,
                    }
                } else {
                    normalize(item)
                };
                out.insert(key.clone(), normalized);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn stdio_entry(script: &Path, log: &Path) -> ServerEntry {
    ServerEntry(
        json!({
            "command": "node",
            "args": [script.to_string_lossy()],
            "env": {
                "RPI_MCP_FIXTURE_LOG": log.to_string_lossy(),
                "RPI_MCP_FIXTURE_LOG_FRAMES": "1",
            },
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    )
}

#[tokio::main]
async fn main() {
    let script = PathBuf::from(env("RPI_MCP_PARITY_FIXTURE_SERVER"));
    let scenario = env("RPI_MCP_PARITY_SCENARIO");
    let workdir = here(&scenario);
    std::fs::create_dir_all(&workdir).expect("workdir");
    // stdio: the fixture child writes into our workdir; http: the
    // orchestrator-launched fixture already logs to RPI_MCP_FIXTURE_LOG.
    let log = if scenario == "stdio" {
        workdir.join("frames.log")
    } else {
        PathBuf::from(env("RPI_MCP_FIXTURE_LOG"))
    };

    let definition = if scenario == "stdio" {
        stdio_entry(&script, &log)
    } else {
        let url = env("RPI_MCP_PARITY_SERVER_URL");
        ServerEntry(
            json!({ "url": url })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )
    };

    let manager = McpServerManager::new(Some(workdir.to_string_lossy().into_owned()));
    let mut document = json!({
        "side": "rpi",
        "transport": scenario,
        "results": {},
        "status": "",
    });

    let run = async {
        let connection = manager.connect("fixture", &definition).await?;
        document["status"] = match connection.status() {
            ConnectionStatus::Connected => json!("connected"),
            ConnectionStatus::Closed => json!("closed"),
            ConnectionStatus::NeedsAuth => json!("needs-auth"),
        };
        if connection.status() != ConnectionStatus::Connected {
            return Ok(());
        }
        let client = connection.client.as_ref().expect("connected client");

        let echo = client
            .call_tool("echo", json!({ "query": "hello" }), Duration::from_secs(10))
            .await?;
        document["results"]["echo"] = normalize(&echo);

        let fail = match client
            .call_tool("fail", json!({}), Duration::from_secs(10))
            .await
        {
            Ok(result) => json!({ "threw": false, "result": normalize(&result) }),
            Err(error) => json!({ "threw": true, "name": error_kind(&error) }),
        };
        document["results"]["failCall"] = fail;

        let resource = client
            .read_resource("fixture://config", Duration::from_secs(10))
            .await?;
        document["results"]["readResource"] = normalize(&resource);
        Ok::<(), rpi_ext_mcp_adapter::protocol::ProtocolError>(())
    };

    match run.await {
        Ok(()) => {}
        Err(error) => {
            document["status"] = json!("error");
            document["error"] = json!(format!("{}: {}", error_kind(&error), error));
        }
    }
    manager.close_all().await;

    let frames = std::fs::read_to_string(&log).unwrap_or_default();
    let frames: Vec<Value> = frames
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    document["frames"] = normalize(&Value::Array(frames));

    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
    let _ = std::fs::remove_dir_all(&workdir);
}

fn error_kind(error: &rpi_ext_mcp_adapter::protocol::ProtocolError) -> &'static str {
    use rpi_ext_mcp_adapter::protocol::ProtocolError as E;
    match error {
        E::Transport(_) => "TransportError",
        E::Http { .. } => "SdkHttpError",
        E::Unauthorized => "UnauthorizedError",
        E::Rpc { .. } => "RpcError",
        E::Timeout => "TimeoutError",
        E::Closed => "ClosedError",
        E::Protocol(_) => "ProtocolError",
    }
}
