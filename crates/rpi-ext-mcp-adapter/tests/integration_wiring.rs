//! 接入波集成测试：TE-D05/D09/D11/D12 的调用链验证。
//!
//! 上游对应测试（意图命名）：
//! - `__tests__/proxy-modes-auto-auth.test.ts`
//! - `__tests__/direct-tools-auto-auth.test.ts`
//! - `__tests__/session-recovery.test.ts`（isTerminatedSession 真值表在
//!   `src/session_recovery.rs` 单元测试中，这里覆盖重放链路）
//! - `__tests__/mcp-probe.test.ts`（probe 后缀接入侧）
//! - `__tests__/server-manager-legacy-handshake.test.ts`（三档协商接入侧）
//!
//! 全部走真实 manager/protocol/proxy 栈 + HTTP stub（不 mock 内部函数），
//! 断言的是上游 mocked 测试所描述的外部可观察行为。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rpi_ext_mcp_adapter::manager::{ConnectionStatus, McpServerManager};
use rpi_ext_mcp_adapter::metadata::ServerEntry;
use rpi_ext_mcp_adapter::proxy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// 与 integration_mcp.rs 相同的最小 HTTP/1.1 stub。
struct StubRequest {
    #[allow(dead_code)]
    method: String,
    headers: Vec<(String, String)>,
    body: String,
}

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
            let method = request_line
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_string();
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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-wiring-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn http_entry(url: String) -> ServerEntry {
    ServerEntry(
        json!({ "url": url })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    )
}

fn rpc_result(body: &str, result: Value) -> String {
    let id = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(json!(0));
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn initialize_result(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "stub", "version": "0.1" },
    })
}

fn tools_result() -> Value {
    json!({ "tools": [{ "name": "echo", "description": "echo tool" }] })
}

// ============================================================================
// TE-D09: autoAuth 接入（对应 __tests__/proxy-modes-auto-auth.test.ts）
// ============================================================================

/// 构建 autoAuth=false/true 的运行时（.mcp.json + initialize_mcp）。
async fn build_runtime(dir: &Path, port: u16, auto_auth: bool) -> Arc<proxy::McpRuntime> {
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "settings": { "autoAuth": auto_auth },
            "mcpServers": {
                // eager：init 时即连接（lazy 默认不触发 startup connect）。
                "demo": {
                    "url": format!("http://127.0.0.1:{port}/mcp"),
                    "lifecycle": "eager"
                }
            }
        }))
        .expect("json"),
    )
    .expect("write config");
    proxy::initialize_mcp(dir, Some(&dir.join(".mcp.json").to_string_lossy()), None).await
}

/// 上游 "fails fast for non-ui browser auth when autoAuth is enabled" +
/// "does not attempt auto-auth when settings.autoAuth is not true"。
/// 原生插件恒 headless：authorization_code 服务器必须以手动授权指引
/// 快速失败，绝不发起 authenticate()（即不对 stub 发 token 请求）。
#[tokio::test]
async fn proxy_modes_auto_auth_headless_fails_fast_without_authenticate() {
    let dir = temp_dir("autoauth-fastfail");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    // 任何 token/authorize 请求都计为违反 fail-closed。
    let auth_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = auth_requests.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            if request.body.contains("token") || request.body.contains("authorize") {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            // 握手即 401 → needs-auth。
            (401, vec![], "unauthorized".to_string())
        }),
        stop_clone,
    ));

    let runtime = build_runtime(&dir, port, true).await;
    let connection = runtime.manager.get_connection("demo");
    assert!(
        connection.is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth),
        "stub 401 must mark needs-auth"
    );

    // execute_connect：autoAuth=true + authorization_code（默认）→
    // fail fast，文案带 auth-start 与 /mcp-auth 指引。
    let result = proxy::execute_connect(&runtime, "demo").await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("auth-start"), "text: {text}");
    assert!(text.contains("/mcp-auth demo"), "text: {text}");
    assert_eq!(result["details"]["error"], json!("auth_required"));
    assert_eq!(
        auth_requests.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "fail-closed: no token/authorize request may leave the plugin"
    );

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 上游 "uses custom authRequiredMessage for non-ui autoAuth failures"：
/// headless fail-fast 文案走 settings.authRequiredMessage 模板。
#[tokio::test]
async fn proxy_modes_auto_auth_custom_auth_required_message() {
    let dir = temp_dir("autoauth-custom");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(|_request: &StubRequest| (401, vec![], "unauthorized".to_string())),
        stop_clone,
    ));

    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "settings": {
                "autoAuth": true,
                "authRequiredMessage": "Reconnect ${server} from the host app."
            },
            "mcpServers": { "demo": { "url": format!("http://127.0.0.1:{port}/mcp"), "lifecycle": "eager" } }
        }))
        .expect("json"),
    )
    .expect("write config");
    let runtime =
        proxy::initialize_mcp(&dir, Some(&dir.join(".mcp.json").to_string_lossy()), None).await;

    let result = proxy::execute_connect(&runtime, "demo").await;
    assert_eq!(
        result["content"][0]["text"],
        json!("Reconnect demo from the host app.")
    );

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 上游 "auto-authenticates and retries executeConnect once" 的接入侧：
/// client_credentials 无需浏览器。用不可达 token 端点模拟 authenticate
/// 失败 → 错误必须包上 "OAuth authentication failed for ..." 前缀
/// （上游 getAuthFailedMessage）。
#[tokio::test]
async fn proxy_modes_auto_auth_failed_message_wrapped() {
    let dir = temp_dir("autoauth-failedmsg");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(|_request: &StubRequest| (401, vec![], "unauthorized".to_string())),
        stop_clone,
    ));

    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "settings": {
                "autoAuth": true,
                "oauthDir": dir.join("oauth").to_string_lossy()
            },
            "mcpServers": {
                "demo": {
                    "url": format!("http://127.0.0.1:{port}/mcp"),
                    "oauth": {
                        "grantType": "client_credentials",
                        "clientId": "cid",
                        "tokenEndpoint": "http://127.0.0.1:1/token"
                    }
                }
            }
        }))
        .expect("json"),
    )
    .expect("write config");
    let runtime =
        proxy::initialize_mcp(&dir, Some(&dir.join(".mcp.json").to_string_lossy()), None).await;

    let result = proxy::execute_connect(&runtime, "demo").await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("OAuth authentication failed for \"demo\": "),
        "getAuthFailedMessage wrapper missing: {text}"
    );
    assert_eq!(result["details"]["error"], json!("auth_required"));

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// TE-D11: session recovery 接入（对应 __tests__/session-recovery.test.ts）
// ============================================================================

/// 上游 "transparently recovers: fn is retried exactly once against the
/// fresh connection" 的接入侧：404 带 session id → 重连 → 重放一次。
/// stub 记录 initialize 次数与 tools/call 次数。
#[tokio::test]
async fn session_recovery_404_reconnects_and_retries_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();

    let initialize_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let init_counter = initialize_count.clone();
    let call_counter = call_count.clone();

    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => {
                    let n = init_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // 第二个会话的 id 不同，证明走了 fresh initialize。
                    let session = if n == 0 { "sess-stale" } else { "sess-fresh" };
                    (
                        200,
                        vec![
                            ("content-type".to_string(), "application/json".to_string()),
                            ("mcp-session-id".to_string(), session.to_string()),
                        ],
                        rpc_result(&request.body, initialize_result("2025-03-26")),
                    )
                }
                "tools/list" => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    rpc_result(&request.body, tools_result()),
                ),
                "tools/call" => {
                    let n = call_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        // 第一次调用：404 模拟会话过期。
                        (404, vec![], "session expired".to_string())
                    } else {
                        (
                            200,
                            vec![("content-type".to_string(), "application/json".to_string())],
                            rpc_result(
                                &request.body,
                                json!({ "content": [{ "type": "text", "text": "recovered" }] }),
                            ),
                        )
                    }
                }
                _ => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    rpc_result(&request.body, json!({})),
                ),
            }
        }),
        stop_clone,
    ));

    let manager = McpServerManager::new(None);
    let entry = http_entry(format!("http://127.0.0.1:{port}/mcp"));
    let connection = manager.connect("http", &entry).await.expect("connect");
    assert_eq!(connection.status(), ConnectionStatus::Connected);
    let client = connection.client.clone().expect("client");
    assert_eq!(client.session_id().as_deref(), Some("sess-stale"));

    // 走 proxy::run_tool_call + is_terminated_session 的重放链路。
    let result =
        proxy::run_tool_call(&client, None, "echo", json!({}), Duration::from_secs(5)).await;
    // 直接调用不走恢复；恢复链路在 execute_call 中。这里先证明第一跳
    // 会话 id 存在且 404 被识别为 terminated：
    match result {
        Err(error) => {
            assert!(
                rpi_ext_mcp_adapter::session_recovery::is_terminated_session(&error, true),
                "404 with session id must classify as terminated: {error}"
            );
        }
        Ok(_) => panic!("first call must fail with 404"),
    }

    // 重放链路：manager.reconnect（identity-guarded）+ 新 client 重试。
    let fresh = manager
        .reconnect("http", &entry, &connection)
        .await
        .expect("reconnect");
    assert_eq!(fresh.status(), ConnectionStatus::Connected);
    let fresh_client = fresh.client.clone().expect("client");
    assert_eq!(fresh_client.session_id().as_deref(), Some("sess-fresh"));
    let retried = proxy::run_tool_call(
        &fresh_client,
        None,
        "echo",
        json!({}),
        Duration::from_secs(5),
    )
    .await
    .expect("retry on fresh session");
    assert_eq!(retried.0["content"][0]["text"], json!("recovered"));

    assert_eq!(
        initialize_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly one fresh initialize"
    );
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "original + one replay"
    );

    manager.close_all().await;
    stop.cancel();
}

/// 上游 "retries exactly once: a second 404 propagates unchanged"：
/// 重连后再次 404 不再重放，错误原样向上传播。
#[tokio::test]
async fn session_recovery_second_404_propagates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => (
                    200,
                    vec![
                        ("content-type".to_string(), "application/json".to_string()),
                        ("mcp-session-id".to_string(), "sess-a".to_string()),
                    ],
                    rpc_result(&request.body, initialize_result("2025-03-26")),
                ),
                "tools/list" => (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    rpc_result(&request.body, tools_result()),
                ),
                // 每次 tools/call 都 404（服务器始终丢会话表）。通知类
                // 请求（initialized 等无响应方法）返回 202（无 body），
                // 避免被当作 JSON-RPC 信封触发 SSE 回退路径。
                _ => {
                    let has_id = body.get("id").is_some();
                    if has_id {
                        (404, vec![], "session expired".to_string())
                    } else {
                        (202, vec![], String::new())
                    }
                }
            }
        }),
        stop_clone,
    ));

    let manager = McpServerManager::new(None);
    let entry = http_entry(format!("http://127.0.0.1:{port}/mcp"));
    let connection = manager.connect("http", &entry).await.expect("connect");
    let client = connection.client.clone().expect("client");

    let first =
        proxy::run_tool_call(&client, None, "echo", json!({}), Duration::from_secs(5)).await;
    assert!(first.is_err());
    let fresh = manager
        .reconnect("http", &entry, &connection)
        .await
        .expect("reconnect");
    let fresh_client = fresh.client.clone().expect("client");
    let second = proxy::run_tool_call(
        &fresh_client,
        None,
        "echo",
        json!({}),
        Duration::from_secs(5),
    )
    .await;
    assert!(second.is_err(), "second 404 must propagate, not loop");

    manager.close_all().await;
    stop.cancel();
}

// ============================================================================
// TE-D05: probe 后缀接入（对应 __tests__/mcp-probe.test.ts 接入侧）
// ============================================================================

/// 上游 `enrichHttpConnectionError`（server-manager.ts:522-530）接入侧：
/// HTTP 连接失败错误文案必须带 " — probe: " 增强后缀，且 probe 请求
/// 不携带任何凭据头。
#[tokio::test]
async fn http_connection_failure_carries_probe_suffix_without_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();

    let probe_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_headers = Arc::new(std::sync::Mutex::new(Vec::<Vec<(String, String)>>::new()));
    let bodies = probe_bodies.clone();
    let headers_sink = seen_headers.clone();

    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            if method != "initialize" {
                bodies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(request.body.clone());
                headers_sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(request.headers.clone());
            }
            // HTML 200 → probe 分类为 "does not appear to speak MCP"，
            // 同时任何 initialize 都失败（连接失败路径触发 probe）。
            (
                200,
                vec![("content-type".to_string(), "text/html".to_string())],
                "<html>not mcp</html>".to_string(),
            )
        }),
        stop_clone,
    ));

    let manager = McpServerManager::new(None);
    let entry = ServerEntry(
        json!({
            "url": format!("http://127.0.0.1:{port}/mcp"),
            // 自定义凭据头：probe 不得携带（G4）。
            "headers": { "Authorization": "Bearer secret-token-123" }
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    let error = match manager.connect("html", &entry).await {
        Ok(_) => panic!("connect must fail against an HTML endpoint"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(" — probe: "),
        "probe suffix missing: {error}"
    );
    assert!(
        error.contains("does not appear to speak MCP"),
        "classification missing: {error}"
    );

    let headers = seen_headers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(!headers.is_empty(), "probe requests must have been sent");
    for request_headers in &headers {
        for (name, value) in request_headers {
            assert_ne!(
                name, "authorization",
                "probe must not carry credentials (G4)"
            );
            assert!(
                !value.contains("secret-token-123"),
                "probe leaked credential material: {value}"
            );
        }
    }

    manager.close_all().await;
    stop.cancel();
}

// ============================================================================
// TE-D12: protocolVersion 三档接入（对应
// __tests__/server-manager-legacy-handshake.test.ts）
// ============================================================================

/// 三档协商接入侧：默认 legacy、`"auto"` 回退、`"2026-07-28"` pin。
/// stub 断言 initialize 请求中的 protocolVersion 字段序列。
#[tokio::test]
async fn protocol_version_three_modes_reach_the_wire() {
    for (mode, _expected_offers) in [
        (None, &["2025-11-25"][..]),
        (Some("auto"), &["2026-07-28", "2025-11-25"][..]),
        (Some("2026-07-28"), &["2026-07-28"][..]),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let stop = CancellationToken::new();
        let stop_clone = stop.clone();

        let offers = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let offers_sink = offers.clone();
        tokio::spawn(run_stub(
            listener,
            Arc::new(move |request: &StubRequest| {
                let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
                let method = body.get("method").and_then(Value::as_str).unwrap_or("");
                match method {
                    "initialize" => {
                        let offered = body["params"]["protocolVersion"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        offers_sink
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(offered.clone());
                        // 只接受 legacy 版本：模拟 legacy 服务器。
                        let version = if offered == "2026-07-28" {
                            // 现代请求被拒（版本不支持错误）→ auto 档回退。
                            return (
                                200,
                                vec![("content-type".to_string(), "application/json".to_string())],
                                json!({
                                    "jsonrpc": "2.0", "id": body["id"],
                                    "error": { "code": -32602, "message": "Server's protocol version is not supported: 2025-11-25" }
                                })
                                .to_string(),
                            );
                        } else {
                            "2025-11-25"
                        };
                        (
                            200,
                            vec![("content-type".to_string(), "application/json".to_string())],
                            rpc_result(&request.body, initialize_result(version)),
                        )
                    }
                    "tools/list" => (
                        200,
                        vec![("content-type".to_string(), "application/json".to_string())],
                        rpc_result(&request.body, tools_result()),
                    ),
                    _ => (
                        200,
                        vec![("content-type".to_string(), "application/json".to_string())],
                        rpc_result(&request.body, json!({})),
                    ),
                }
            }),
            stop_clone,
        ));

        let mut map = serde_json::Map::new();
        map.insert(
            "url".to_string(),
            json!(format!("http://127.0.0.1:{port}/mcp")),
        );
        if let Some(mode) = mode {
            map.insert("protocolVersion".to_string(), json!(mode));
        }
        let entry = ServerEntry(map);

        let manager = McpServerManager::new(None);
        let outcome = manager.connect("negotiate", &entry).await;
        let offers_seen = offers.lock().unwrap_or_else(|e| e.into_inner()).clone();

        match mode {
            // legacy 默认：一次握手成功。
            None => {
                let connection = outcome.expect("legacy handshake succeeds");
                assert_eq!(connection.status(), ConnectionStatus::Connected);
                assert_eq!(offers_seen, ["2025-11-25"]);
            }
            // auto：先现代被拒，回退 legacy 成功。
            Some("auto") => {
                let connection = outcome.expect("auto falls back to legacy");
                assert_eq!(connection.status(), ConnectionStatus::Connected);
                assert_eq!(offers_seen, ["2026-07-28", "2025-11-25"]);
            }
            // pinned：现代被拒即失败，无回退。
            Some(_) => {
                assert!(
                    outcome.is_err(),
                    "pinned 2026 must fail against legacy server"
                );
                // The connect-failure probe (TE-D05 enrichment) sends its
                // own legacy initialize (offer 2025-06-18) to the same URL;
                // it is not a handshake offer — filter it out.
                let handshake_offers: Vec<&str> = offers_seen
                    .iter()
                    .map(String::as_str)
                    .filter(|offer| *offer != "2025-06-18")
                    .collect();
                assert_eq!(handshake_offers, ["2026-07-28"]);
            }
        }

        manager.close_all().await;
        stop.cancel();
    }
}

// ============================================================================
// TE-D09（direct 侧）: 对应 __tests__/direct-tools-auto-auth.test.ts 的
// "fails fast in non-ui context for browser-based OAuth"。
// ============================================================================

/// direct 执行器对 needs-auth 的渲染与 autoAuth 短路（headless）。
#[tokio::test]
async fn direct_tools_auto_auth_headless_auth_required() {
    let dir = temp_dir("direct-autoauth");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(|_request: &StubRequest| (401, vec![], "unauthorized".to_string())),
        stop_clone,
    ));

    let runtime = build_runtime(&dir, port, true).await;
    // 手工注册一个 direct spec（无需 bootstrap 完成）。
    let spec = rpi_ext_mcp_adapter::direct::DirectToolSpec {
        server_name: "demo".to_string(),
        original_name: "echo".to_string(),
        prefixed_name: "demo_echo".to_string(),
        description: String::new(),
        input_schema: None,
        resource_uri: None,
    };
    let result =
        rpi_ext_mcp_adapter::direct::execute_direct_tool(&runtime, &spec, &json!({})).await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("auth-start"), "text: {text}");
    assert!(text.contains("/mcp-auth demo"), "text: {text}");
    assert_eq!(result["details"]["error"], json!("auth_required"));

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// 批次4 (R7): freezeDirectTools 接线——走真实 install → dispatcher 钩子
// 分发路径（不直接调内部 sync 函数）。
// ============================================================================

use std::sync::Mutex as StdMutex;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::install_for_test;

/// 进程内假宿主：记录 registerTool / unregisterTool / setActiveTools 的
/// 全序列，`{"ok": ...}` 信封回包，ctx.cwd / getFlag 可编程。
struct FakeHost {
    cwd: StdMutex<String>,
    registered: StdMutex<Vec<String>>,
}

impl FakeHost {
    fn new(cwd: &str) -> Arc<Self> {
        Arc::new(Self {
            cwd: StdMutex::new(cwd.to_string()),
            registered: StdMutex::new(Vec::new()),
        })
    }

    fn registered_names(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // SAFETY: the Arc handed to install_for_test stays alive for the whole
    // process (per-test FakeHost instances are never freed); from_raw +
    // mem::forget here only re-borrows it.
    let host = unsafe { Arc::from_raw(host_ptr as *const FakeHost) };
    let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = request.get("call").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "ctx.cwd" => json!({"ok": *host.cwd.lock().unwrap_or_else(|e| e.into_inner())}),
        "getFlag" => json!({"ok": null}),
        "on" | "registerFlag" => json!({"ok": true}),
        "registerTool" => {
            let name = args
                .get("definition")
                .and_then(|d| d.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() {
                let mut registered = host.registered.lock().unwrap_or_else(|e| e.into_inner());
                if !registered.contains(&name) {
                    registered.push(name.clone());
                }
            }
            json!({"ok": true})
        }
        "unregisterTool" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
            let mut registered = host.registered.lock().unwrap_or_else(|e| e.into_inner());
            let removed = registered.iter().position(|n| n == name);
            let _ = removed.map(|pos| registered.remove(pos));
            json!({"ok": removed.is_some()})
        }
        "getActiveTools" => json!({"ok": host.registered_names()}),
        "setActiveTools" => {
            let names: Vec<String> = args
                .get("toolNames")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            *host.registered.lock().unwrap_or_else(|e| e.into_inner()) = names;
            json!({"ok": true})
        }
        "getAllTools" => json!({"ok": []}),
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// freezeDirectTools=true（R7 红线）接线：on_ready 冻结后，metadata 更新
/// 钩子（运行时 on_metadata_updated slot）不得重建 direct 工具面；
/// `mcp({connect})` 的同步钩子（on_connect_sync）仍重建。全程走真实
/// install → 后台驱动 init → 钩子分发路径。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_direct_tools_metadata_hook_skips_connect_hook_syncs() {
    let dir = temp_dir("freeze-wiring");
    let host = FakeHost::new(&dir.to_string_lossy());

    // 元数据缓存：为 stub 服务器造一份可用缓存（direct 面来自缓存，
    // 无需真实连接）。config_hash 必须与 .mcp.json 中的服务器定义一致
    // （isServerCacheValid 按哈希 + cached_at 校验）。写入
    // RPI_CODING_AGENT_DIR 下的 mcp-cache.json，隔离默认解析路径。
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let base = now_unix_ms();
    let entry =
        json!({ "url": "http://127.0.0.1:9/mcp", "lifecycle": "eager", "directTools": true });
    let definition =
        rpi_ext_mcp_adapter::metadata::ServerEntry(entry.as_object().cloned().unwrap_or_default());
    let config_hash =
        rpi_ext_mcp_adapter::cache::compute_server_hash(&definition).expect("server hash computes");
    let cache = json!({
        "version": 1,
        "servers": {
            "demo": {
                "configHash": config_hash,
                "cachedAt": base - 1000,
                "tools": [ { "name": "echo", "description": "v1" } ]
            }
        }
    });
    std::fs::write(
        agent_dir.join("mcp-cache.json"),
        serde_json::to_string(&cache).expect("json"),
    )
    .expect("cache");

    // 会话 cwd 下的 .mcp.json：freezeDirectTools=true + eager 服务器
    // （触发 install 的 load-time prewarm）。eager 连接会失败（无 stub），
    // 但后台 init 仍会完成并发布 Ready。
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "settings": { "freezeDirectTools": true },
            "mcpServers": { "demo": entry }
        }))
        .expect("json"),
    )
    .expect("config");

    std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
    // HOME 指向空目录：全局配置源（~/.config/mcp/... 等）全部不存在，
    // 隔离开发者机器上的真实全局配置。install 后台驱动与后续每次
    // surface sync 都按这两个变量解析缓存/配置路径——整个用例期间保持，
    // 末尾恢复。本用例是本测试二进制内唯一 install_for_test 调用者，
    // 其余用例不读这两个变量。
    let saved_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &dir);
    let host_ptr = Arc::into_raw(host.clone()) as PluginCookie;
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, host_ptr);
    assert_eq!(installed, json!({"ok": true}), "install must succeed");

    // load-time prewarm → install 已通过 start_init 预热（B-1 后台驱动）。
    let dispatcher = rpi_ext_mcp_adapter::dispatcher_for_test().expect("state");
    let deadline = std::time::Duration::from_secs(10);
    let ready = tokio::time::timeout(deadline, async {
        while dispatcher.try_runtime().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "prewarm init must complete in the background"
    );

    // on_ready 钩子（真实分发路径）已完成首次同步并冻结：demo_echo 在面。
    assert!(
        host.registered_names().contains(&"demo_echo".to_string()),
        "post-init sync must register the direct tool, got {:?}",
        host.registered_names()
    );

    // 缓存推进到 v2（echo 描述变化 + 新工具 shout）。
    let cache_v2 = json!({
        "version": 1,
        "servers": {
            "demo": {
                "configHash": config_hash,
                "cachedAt": base + 1000,
                "tools": [
                    { "name": "echo", "description": "v2" },
                    { "name": "shout", "description": "new tool" }
                ]
            }
        }
    });
    std::fs::write(
        agent_dir.join("mcp-cache.json"),
        serde_json::to_string(&cache_v2).expect("json"),
    )
    .expect("cache v2");

    // metadata 更新钩子（index.ts:313-321）：运行期所有刷新点都通过
    // on_metadata_updated slot 分发；frozen=true 时不得重建工具面。
    if let Some(runtime) = dispatcher.try_runtime() {
        let hook = runtime
            .on_metadata_updated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook("demo", "test-metadata-update");
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !host.registered_names().contains(&"demo_shout".to_string()),
        "frozen surface must NOT pick up new cache tools: {:?}",
        host.registered_names()
    );

    // connect 同步钩子（on_connect_sync，index.ts:822-824）仍重建工具面：
    // 走真实 execute → fire_on_connect_sync 分发（连接失败也会触发）。
    let _ = dispatcher.execute(&json!({"connect": "demo"}), &[]).await;
    assert!(
        host.registered_names().contains(&"demo_shout".to_string()),
        "connect sync must rebuild the surface despite freeze: {:?}",
        host.registered_names()
    );

    // 环境恢复（后续用例回到默认 HOME / agent dir）。
    std::env::remove_var("RPI_CODING_AGENT_DIR");
    match saved_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
