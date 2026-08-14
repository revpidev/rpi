//! Integration tests: fixture stdio server + stub HTTP server driving the
//! real protocol/manager/lifecycle/proxy stack (design §5.2/§5.4).
//!
//! The fixture server is `examples/fixture_stdio_server.rs` (built by
//! `cargo test`); the HTTP stub is a hand-rolled `tokio::net::TcpListener`
//! server below. Frame-sequence assertions read the fixture's
//! `RPI_MCP_FIXTURE_LOG` transcript.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rpi_ext_mcp_adapter::lifecycle::LifecycleManager;
use rpi_ext_mcp_adapter::manager::{ConnectionStatus, McpServerManager};
use rpi_ext_mcp_adapter::metadata::ServerEntry;
use rpi_ext_mcp_adapter::proxy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Locate the example binary built next to this test binary.
fn fixture_server_exe() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    // target/debug/deps/<test> -> target/debug/examples/fixture_stdio_server
    let debug_dir = exe.parent().and_then(Path::parent).expect("target dir");
    let candidate = debug_dir.join("examples").join("fixture_stdio_server");
    assert!(
        candidate.exists(),
        "fixture server missing: {}",
        candidate.display()
    );
    candidate
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-itest-{}-{}-{}",
        tag,
        std::process::id(),
        now_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn stdio_entry(log: &Path, pid: &Path) -> ServerEntry {
    ServerEntry(
        json!({
            "command": fixture_server_exe().to_string_lossy(),
            "env": {
                "RPI_MCP_FIXTURE_LOG": log.to_string_lossy(),
                "RPI_MCP_FIXTURE_PID": pid.to_string_lossy(),
            },
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    )
}

fn pid_alive(pid_file: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<i32>() else {
        return false;
    };
    Path::new(&format!("/proc/{pid}")).exists()
}

// ---------------------------------------------------------------- stdio flow

#[tokio::test]
async fn stdio_full_flow_frame_sequence_and_calls() {
    let dir = temp_dir("stdio-flow");
    let log = dir.join("frames.log");
    let pid = dir.join("server.pid");
    let entry = stdio_entry(&log, &pid);

    let manager = McpServerManager::new(Some(dir.to_string_lossy().into_owned()));
    let connection = manager.connect("fixture", &entry).await.expect("connect");

    assert_eq!(connection.status(), ConnectionStatus::Connected);
    assert_eq!(
        connection.instructions.as_deref(),
        Some("fixture instructions")
    );
    let tool_names: Vec<&str> = connection
        .tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(tool_names, ["echo", "fail"]);
    assert_eq!(connection.resources.len(), 1);
    assert_eq!(connection.prompts.len(), 1);

    let client = connection.client.clone().expect("client");
    let result = client
        .call_tool("echo", json!({ "query": "hello" }), Duration::from_secs(5))
        .await
        .expect("call");
    assert_eq!(result["content"][0]["text"], json!("hello"));

    let result = client
        .call_tool("fail", json!({}), Duration::from_secs(5))
        .await
        .expect("call");
    assert_eq!(result["isError"], json!(true));

    let result = client
        .read_resource("fixture://config", Duration::from_secs(5))
        .await
        .expect("read");
    assert_eq!(result["contents"][0]["text"], json!("resource-body"));

    // Frame sequence (design §5.2 parity anchor): initialize → initialized
    // → the three discovery lists, then the calls.
    let frames = std::fs::read_to_string(&log).expect("frame log");
    let frames: Vec<&str> = frames.lines().collect();
    assert_eq!(
        frames,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "resources/list",
            "prompts/list",
            "tools/call",
            "tools/call",
            "resources/read",
        ]
    );

    manager.close_all().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!pid_alive(&pid), "fixture child must be reaped (G4)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stdio_connection_failure_includes_stderr_tail() {
    let entry = ServerEntry(
        json!({
            "command": "sh",
            "args": ["-c", "echo fixture-boom-line >&2; exit 1"],
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    let manager = McpServerManager::new(None);
    let error = match manager.connect("broken", &entry).await {
        Ok(_) => panic!("connect must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("fixture-boom-line"),
        "error should carry the stderr tail: {error}"
    );
}

// ----------------------------------------------------------------- lifecycle

#[tokio::test]
async fn lifecycle_idle_shutdown_and_keep_alive_reconnect() {
    let dir = temp_dir("lifecycle");
    let log = dir.join("frames.log");
    let pid = dir.join("server.pid");
    let entry = stdio_entry(&log, &pid);

    let manager = McpServerManager::new(Some(dir.to_string_lossy().into_owned()));
    let lifecycle = LifecycleManager::new(manager.clone(), CancellationToken::new());
    lifecycle.set_health_interval(Duration::from_millis(150));
    lifecycle.register_server("fixture", &entry, None);
    lifecycle.set_global_idle_timeout_minutes(0); // idle check disabled at 0

    // keep-alive: killed child is reconnected on the next health tick.
    lifecycle.mark_keep_alive("fixture", &entry);
    lifecycle.start_health_checks();

    manager.connect("fixture", &entry).await.expect("connect");
    let first_pid = std::fs::read_to_string(&pid).expect("pid file");
    let first_pid: i32 = first_pid.trim().parse().expect("pid");
    #[cfg(unix)]
    unsafe {
        // Safety: kill(2) a pid we just spawned via the fixture server.
        libc::kill(first_pid, libc::SIGKILL);
    }
    let (reconnect_tx, reconnect_rx) = tokio::sync::oneshot::channel();
    let reconnect_tx = Arc::new(std::sync::Mutex::new(Some(reconnect_tx)));
    lifecycle.set_reconnect_callback(Arc::new(move |_name| {
        if let Some(tx) = reconnect_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(());
        }
    }));
    tokio::time::timeout(Duration::from_secs(10), reconnect_rx)
        .await
        .expect("keep-alive reconnect within 10s")
        .expect("channel");

    lifecycle.graceful_shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pid_alive(&pid),
        "reconnected fixture child must be reaped (G4)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn lifecycle_idle_close_after_timeout() {
    let dir = temp_dir("idle");
    let log = dir.join("frames.log");
    let pid = dir.join("server.pid");
    let entry = stdio_entry(&log, &pid);

    let manager = McpServerManager::new(Some(dir.to_string_lossy().into_owned()));
    let lifecycle = LifecycleManager::new(manager.clone(), CancellationToken::new());
    lifecycle.set_health_interval(Duration::from_millis(100));
    // idleTimeout is minutes upstream; a 0.001-minute (~60ms) timeout needs
    // fractional support — register with the smallest positive duration via
    // the global setting instead.
    lifecycle.register_server("fixture", &entry, None);
    manager.connect("fixture", &entry).await.expect("connect");
    assert!(manager.get_connection("fixture").is_some());

    // Force the connection to look idle: last_used_at in the past, global
    // idle timeout at its smallest (0 minutes disables the check upstream,
    // so use the manager's is_idle directly with a tiny duration to assert
    // semantics, then run one health pass with a 1-minute timeout against a
    // backdated connection).
    let connection = manager.get_connection("fixture").expect("connection");
    connection
        .last_used_at
        .store(0, std::sync::atomic::Ordering::SeqCst);
    assert!(manager.is_idle("fixture", Duration::from_millis(1)));
    lifecycle.set_global_idle_timeout_minutes(1);
    let (idle_tx, idle_rx) = tokio::sync::oneshot::channel();
    let idle_tx = Arc::new(std::sync::Mutex::new(Some(idle_tx)));
    lifecycle.set_idle_shutdown_callback(Arc::new(move |_name| {
        if let Some(tx) = idle_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
    }));
    lifecycle.start_health_checks();
    tokio::time::timeout(Duration::from_secs(10), idle_rx)
        .await
        .expect("idle shutdown within 10s")
        .expect("channel");
    assert!(manager.get_connection("fixture").is_none());

    lifecycle.graceful_shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!pid_alive(&pid), "idle-closed child must be reaped (G4)");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------ proxy end-to-end

/// Serializes tests that mutate process env (`HOME`, `RPI_CODING_AGENT_DIR`).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Env-mutating end-to-end over the proxy dispatcher.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env mutation; held across await is intentional
async fn proxy_end_to_end_five_modes() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("proxy-e2e");
    let home = dir.join("home");
    let project = dir.join("proj");
    let agent = dir.join("agent");
    for d in [&home, &project, &agent] {
        std::fs::create_dir_all(d).expect("dirs");
    }
    let log = dir.join("frames.log");
    let pid = dir.join("server.pid");
    let entry = stdio_entry(&log, &pid);
    std::fs::write(
        project.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": { "fixture": entry.as_map() }
        }))
        .expect("json"),
    )
    .expect("write config");

    let saved_home = std::env::var_os("HOME");
    let saved_agent = std::env::var_os("RPI_CODING_AGENT_DIR");
    std::env::set_var("HOME", &home);
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent);

    std::panic::AssertUnwindSafe(async {
        let dispatcher = Arc::new(proxy::ProxyDispatcher::new());
        dispatcher.start_init(project.clone(), None);

        // status: not connected yet (lazy default), cached after bootstrap
        // (no cache file existed → bootstrap connects once).
        let status = dispatcher.execute(&json!({}), &[]).await;
        let text = status["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(text.contains("MCP: 1/1 servers"), "status text: {text}");
        assert!(text.contains("✓ fixture"), "status connected: {text}");

        // search finds echo via cache/live metadata.
        let result = dispatcher.execute(&json!({ "search": "echo" }), &[]).await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(text.contains("fixture_echo"), "search text: {text}");
        assert!(result["details"]["count"].as_u64().unwrap_or(0) >= 1);

        // regex mode + error kinds.
        let result = dispatcher
            .execute(&json!({ "search": "fix.*_echo", "regex": true }), &[])
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(text.contains("fixture_echo"), "regex search: {text}");
        let result = dispatcher
            .execute(&json!({ "search": "([", "regex": true }), &[])
            .await;
        assert_eq!(result["details"]["error"], json!("invalid_pattern"));
        let long = "a".repeat(300);
        let result = dispatcher
            .execute(&json!({ "search": long, "regex": true }), &[])
            .await;
        assert_eq!(result["details"]["error"], json!("query_too_long"));

        // describe renders the ts-shape.
        let result = dispatcher
            .execute(&json!({ "describe": "fixture_echo" }), &[])
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            text.contains("fixture_echo\nServer: fixture"),
            "describe: {text}"
        );
        assert!(
            text.contains("Shape:\n{ query: string; }"),
            "describe shape: {text}"
        );

        // list + instructions.
        let result = dispatcher
            .execute(&json!({ "server": "fixture" }), &[])
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(text.contains("fixture (3 tools):"), "list: {text}");
        let result = dispatcher
            .execute(&json!({ "instructions": "fixture" }), &[])
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            text.contains("fixture instructions"),
            "instructions: {text}"
        );

        // call: echo content + isError re-flagging shapes.
        let result = dispatcher
            .execute(
                &json!({ "tool": "fixture_echo", "args": { "query": "pong" } }),
                &[],
            )
            .await;
        assert_eq!(result["content"][0]["text"], json!("pong"));
        assert_eq!(result["details"]["mode"], json!("call"));

        let result = dispatcher
            .execute(&json!({ "tool": "fixture_fail" }), &[])
            .await;
        assert_eq!(result["details"]["error"], json!("tool_error"));
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.starts_with("Error: "), "tool_error prefix: {text}");
        // error-signal.ts: the hook flips isError for these details.
        assert_eq!(
            proxy::tool_error_override(&result["details"]),
            Some(json!({ "isError": true }))
        );

        // connect mode returns the server list.
        let result = dispatcher
            .execute(&json!({ "connect": "fixture" }), &[])
            .await;
        assert_eq!(result["details"]["mode"], json!("list"));

        // invalid args shapes (TE-D04: throw → error result).
        let result = dispatcher
            .execute(&json!({ "tool": "fixture_echo", "args": "{not json" }), &[])
            .await;
        assert_eq!(result["details"]["error"], json!("invalid_args"));
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.starts_with("Invalid args JSON: "), "{text}");

        dispatcher.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!pid_alive(&pid), "proxy shutdown reaps children (G4)");
    })
    .await;

    match saved_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    match saved_agent {
        Some(agent) => std::env::set_var("RPI_CODING_AGENT_DIR", agent),
        None => std::env::remove_var("RPI_CODING_AGENT_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------- HTTP stubs

struct StubRequest {
    method: String,
    #[allow(dead_code)]
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Minimal HTTP/1.1 stub: one connection per request, programmatic handler.
type StubHandler = Arc<dyn Fn(&StubRequest) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

async fn run_stub(listener: TcpListener, handler: StubHandler, stop: CancellationToken) {
    loop {
        let (mut socket, _) = tokio::select! {
            _ = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(_) => break,
            },
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let header_end = loop {
                let n = match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos;
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default();
            let mut parts = request_line.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    let name = name.trim().to_lowercase();
                    let value = value.trim().to_string();
                    if name == "content-length" {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.push((name, value));
                }
            }
            let mut body = buf[header_end + 4..].to_vec();
            while body.len() < content_length {
                let n = match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                body.extend_from_slice(&tmp[..n]);
            }
            let request = StubRequest {
                method,
                path,
                headers,
                body: String::from_utf8_lossy(&body[..content_length.min(body.len())]).to_string(),
            };
            let (status, response_headers, response_body) = handler(&request);
            let mut response = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                response_body.len()
            );
            for (name, value) in response_headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&response_body);
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn header<'a>(request: &'a StubRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-03-26",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "http-stub", "version": "0.1" },
    })
}

#[tokio::test]
async fn http_streamable_json_flow_and_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<StubRequestLite>::new()));
    let seen_clone = seen.clone();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            seen_clone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(StubRequestLite {
                    http_method: request.method.clone(),
                    rpc_method: method.clone(),
                    session_header: header(request, "mcp-session-id").map(str::to_string),
                    version_header: header(request, "mcp-protocol-version").map(str::to_string),
                });
            match (request.method.as_str(), method.as_str()) {
                ("POST", "initialize") => (
                    200,
                    vec![
                        ("content-type".to_string(), "application/json".to_string()),
                        ("mcp-session-id".to_string(), "sess-1".to_string()),
                    ],
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": initialize_result() })
                        .to_string(),
                ),
                ("POST", "tools/list") => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    json!({
                        "jsonrpc": "2.0", "id": body["id"],
                        "result": { "tools": [{ "name": "web", "description": "HTTP tool" }] },
                    })
                    .to_string(),
                ),
                ("POST", _) => (202, vec![], String::new()),
                ("GET", _) => (405, vec![], String::new()),
                _ => (400, vec![], String::new()),
            }
        }),
        stop_clone,
    ));

    let entry = ServerEntry(
        json!({ "url": format!("http://127.0.0.1:{port}/mcp") })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let manager = McpServerManager::new(None);
    let connection = manager.connect("http", &entry).await.expect("connect");
    assert_eq!(connection.status(), ConnectionStatus::Connected);
    assert_eq!(connection.tools.len(), 1);
    manager.close_all().await;
    stop.cancel();

    let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    // Handshake has no session header; discovery carries it plus the
    // negotiated protocol version.
    assert_eq!(seen[0].rpc_method, "initialize");
    assert_eq!(seen[0].session_header, None);
    let tools_list = seen
        .iter()
        .find(|r| r.rpc_method == "tools/list")
        .expect("tools/list");
    assert_eq!(tools_list.session_header.as_deref(), Some("sess-1"));
    assert_eq!(tools_list.version_header.as_deref(), Some("2025-03-26"));
    assert!(seen
        .iter()
        .any(|r| r.rpc_method == "notifications/initialized"));
}

#[tokio::test]
async fn http_bearer_and_command_secret_headers() {
    // Static bearer via env var + a `!command`-resolved custom header.
    std::env::set_var("RPI_MCP_BEARER_TEST", "env-token-123");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen_auth = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let seen_custom = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let seen_auth2 = seen_auth.clone();
    let seen_custom2 = seen_custom.clone();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            seen_auth2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(header(request, "authorization").map(str::to_string));
            seen_custom2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(header(request, "x-secret").map(str::to_string));
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": initialize_result() })
                        .to_string(),
                ),
                "tools/list" => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "tools": [] } })
                        .to_string(),
                ),
                _ => (202, vec![], String::new()),
            }
        }),
        stop_clone,
    ));

    let entry = ServerEntry(
        json!({
            "url": format!("http://127.0.0.1:{port}/mcp"),
            "auth": "bearer",
            "bearerTokenEnv": "RPI_MCP_BEARER_TEST",
            "headers": { "X-Secret": "!printf cmd-secret-456" },
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    let manager = McpServerManager::new(None);
    let connection = manager.connect("auth", &entry).await.expect("connect");
    assert_eq!(connection.status(), ConnectionStatus::Connected);
    manager.close_all().await;
    stop.cancel();
    std::env::remove_var("RPI_MCP_BEARER_TEST");

    let auth = seen_auth.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!auth.is_empty());
    assert!(
        auth.iter()
            .all(|v| v.as_deref() == Some("Bearer env-token-123")),
        "Authorization header on every request: {auth:?}"
    );
    let custom = seen_custom.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        custom
            .iter()
            .all(|v| v.as_deref() == Some("cmd-secret-456")),
        "X-Secret from !command: {custom:?}"
    );
}

#[test]
fn command_secret_success_failure_and_empty() {
    // Success
    let value = rpi_ext_mcp_adapter::utils::resolve_command_secret("!printf abc", "test ctx")
        .expect("success");
    assert_eq!(value, "abc");
    // Escaped literal
    assert_eq!(
        rpi_ext_mcp_adapter::utils::resolve_command_secret("!!printf abc", "test ctx")
            .expect("escaped"),
        "!printf abc"
    );
    // Non-zero exit
    let err = rpi_ext_mcp_adapter::utils::resolve_command_secret("!exit 3", "test ctx")
        .expect_err("non-zero exit");
    assert!(err.to_string().contains("exited with code 3"), "{err}");
    // Empty output
    let err = rpi_ext_mcp_adapter::utils::resolve_command_secret("!true", "test ctx")
        .expect_err("empty output");
    assert!(err.to_string().contains("empty output"), "{err}");
    // Oversized output (ENOBUFS parity)
    let err = rpi_ext_mcp_adapter::utils::resolve_command_secret(
        "!head -c 2000000 /dev/zero | tr '\\0' 'x'",
        "test ctx",
    )
    .expect_err("oversized output");
    assert!(err.to_string().contains("exceeded 1 MiB"), "{err}");
}

#[derive(Debug)]
struct StubRequestLite {
    #[allow(dead_code)]
    http_method: String,
    rpc_method: String,
    session_header: Option<String>,
    version_header: Option<String>,
}

#[tokio::test]
async fn http_fallback_matrix_404_405_406_415() {
    for status in [404u16, 405, 406, 415] {
        http_fallback_to_legacy_sse_with_status(status).await;
    }
}

async fn http_fallback_to_legacy_sse_with_status(fail_status: u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    // Responses are pushed into the open GET stream AFTER the matching POST
    // arrives (a legacy-SSE server never answers unsolicited).
    let (events_tx, events_rx) = mpsc::channel::<String>(8);
    let events_rx = Arc::new(tokio::sync::Mutex::new(Some(events_rx)));
    let stop = CancellationToken::new();
    let stop_accept = stop.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = tokio::select! {
                _ = stop_accept.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok(pair) => pair,
                    Err(_) => break,
                },
            };
            let events_tx = events_tx.clone();
            let events_rx = events_rx.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let header_end = loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        break pos;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split(' ');
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut content_length = 0usize;
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = buf[header_end + 4..].to_vec();
                while body.len() < content_length {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    body.extend_from_slice(&tmp[..n]);
                }
                let body_text =
                    String::from_utf8_lossy(&body[..content_length.min(body.len())]).to_string();

                match (method.as_str(), path.as_str()) {
                    ("POST", "/sse") => {
                        write_http(&mut socket, fail_status, &[], "not here").await;
                    }
                    ("GET", "/sse") => {
                        // Long-lived stream: endpoint event, then responses as
                        // they are triggered by POSTs to /msg.
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\nevent: endpoint\ndata: /msg\n\n";
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        let mut events_rx = events_rx.lock().await.take();
                        let Some(mut events_rx) = events_rx.take() else {
                            return;
                        };
                        while let Some(payload) = events_rx.recv().await {
                            let frame = format!("event: message\ndata: {payload}\n\n");
                            if socket.write_all(frame.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                    ("POST", "/msg") => {
                        let message: Value =
                            serde_json::from_str(&body_text).unwrap_or(Value::Null);
                        write_http(&mut socket, 202, &[], "").await;
                        if let (Some(id), Some(rpc_method)) = (
                            message.get("id").and_then(Value::as_u64),
                            message.get("method").and_then(Value::as_str),
                        ) {
                            let result = match rpc_method {
                                "initialize" => initialize_result(),
                                "tools/list" => json!({ "tools": [] }),
                                _ => json!({}),
                            };
                            let _ = events_tx
                                .send(
                                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
                                        .to_string(),
                                )
                                .await;
                        }
                    }
                    _ => {
                        write_http(&mut socket, 400, &[], "").await;
                    }
                }
            });
        }
    });

    let entry = ServerEntry(
        json!({ "url": format!("http://127.0.0.1:{port}/sse") })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let manager = McpServerManager::new(None);
    let connection = manager
        .connect("sse", &entry)
        .await
        .expect("sse fallback connects");
    assert_eq!(connection.status(), ConnectionStatus::Connected);
    manager.close_all().await;
    stop.cancel();
}

async fn write_http(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) {
    let mut response = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    let _ = socket.write_all(response.as_bytes()).await;
}

#[tokio::test]
async fn http_401_marks_needs_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |_request: &StubRequest| (401, vec![], "unauthorized".to_string())),
        stop_clone,
    ));

    let entry = ServerEntry(
        json!({ "url": format!("http://127.0.0.1:{port}/mcp") })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let manager = McpServerManager::new(None);
    let connection = manager.connect("auth", &entry).await.expect("connect");
    assert_eq!(connection.status(), ConnectionStatus::NeedsAuth);
    manager.close_all().await;
    stop.cancel();
}

/// FR-P1-01 end-to-end: bootstrap writes the cache; direct tools resolve
/// from it and execute through the live connection.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env mutation; held across await is intentional
async fn direct_tools_resolve_execute_and_sync() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("direct-e2e");
    let home = dir.join("home");
    let project = dir.join("proj");
    let agent = dir.join("agent");
    for d in [&home, &project, &agent] {
        std::fs::create_dir_all(d).expect("dirs");
    }
    let log = dir.join("frames.log");
    let pid = dir.join("server.pid");
    let entry = stdio_entry(&log, &pid);
    let mut fixture_entry = entry.as_map().clone();
    fixture_entry.insert("directTools".to_string(), json!(true));
    std::fs::write(
        project.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": { "fixture": fixture_entry }
        }))
        .expect("json"),
    )
    .expect("write config");

    let saved_home = std::env::var_os("HOME");
    let saved_agent = std::env::var_os("RPI_CODING_AGENT_DIR");
    std::env::set_var("HOME", &home);
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent);

    std::panic::AssertUnwindSafe(async {
        let dispatcher = Arc::new(proxy::ProxyDispatcher::new());
        dispatcher.start_init(project.clone(), None);
        let runtime = dispatcher
            .current_direct()
            .await
            .expect("init ready (bootstrap connects the fixture server)");

        // Cache now exists with the fixture server entry.
        let cache = rpi_ext_mcp_adapter::cache::load_metadata_cache(
            &rpi_ext_mcp_adapter::cache::get_metadata_cache_path(),
        )
        .expect("cache written by bootstrap");
        let specs = rpi_ext_mcp_adapter::direct::resolve_direct_tools(
            &runtime.config,
            Some(&cache),
            rpi_ext_mcp_adapter::metadata::ToolPrefix::Server,
            None,
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(
            names,
            ["fixture_echo", "fixture_fail", "fixture_read_config"]
        );

        // Registration payload parity (label/snippet/normalized parameters).
        let definition = rpi_ext_mcp_adapter::direct::direct_tool_definition(&specs[0]);
        assert_eq!(definition["definition"]["label"], json!("MCP: echo"));
        assert_eq!(
            definition["definition"]["promptSnippet"],
            json!("Echo the query back")
        );
        assert_eq!(
            definition["definition"]["parameters"]["properties"]["query"]["type"],
            json!("string")
        );

        // Execution through the live connection.
        let result = rpi_ext_mcp_adapter::direct::execute_direct_tool(
            &runtime,
            &specs[0],
            &json!({ "query": "pong" }),
        )
        .await;
        assert_eq!(result["content"][0]["text"], json!("pong"));
        assert_eq!(result["details"]["tool"], json!("echo"));

        // Resource direct tool → resources/read.
        let result =
            rpi_ext_mcp_adapter::direct::execute_direct_tool(&runtime, &specs[2], &json!({})).await;
        assert_eq!(result["content"][0]["text"], json!("resource-body"));

        // MCP isError → tool_error details (hook re-flags at the host).
        let result =
            rpi_ext_mcp_adapter::direct::execute_direct_tool(&runtime, &specs[1], &json!({})).await;
        assert_eq!(result["details"]["error"], json!("tool_error"));

        // Output guard through the direct path: tiny maxBytes → truncation.
        let mut config = runtime.config.clone();
        config.settings = Some(
            json!({ "outputGuard": { "maxBytes": 16, "maxLines": 2000 } })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let guarded = rpi_ext_mcp_adapter::guard::guard_mcp_output(
            vec![json!({ "type": "text", "text": "x".repeat(1024) })],
            &rpi_ext_mcp_adapter::guard::resolve_guard_options(config.settings.as_ref()),
        );
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(guard["truncated"], json!(true));
        let _ = std::fs::remove_file(guard["fullOutputPath"].as_str().unwrap_or_default());

        dispatcher.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!pid_alive(&pid), "children reaped (G4)");
    })
    .await;

    match saved_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    match saved_agent {
        Some(agent) => std::env::set_var("RPI_CODING_AGENT_DIR", agent),
        None => std::env::remove_var("RPI_CODING_AGENT_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
