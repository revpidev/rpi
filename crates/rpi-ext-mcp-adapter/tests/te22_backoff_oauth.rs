//! TE22 行为面接线测试（R7.2.3 / R7.2.5 / R7.2.6 / R7.2.9）。
//!
//! 上游对应测试（意图命名）：
//! - `__tests__/mcp-status.test.ts` / `__tests__/proxy-modes-discovery.test.ts`
//!   / `__tests__/search-ranking.test.ts` / `__tests__/direct-tools.test.ts`
//!   （#434 退避可见性：status 文本/快照、list/search/describe、direct 面）
//! - `__tests__/init-failure-state.test.ts`（失败窗口过期后恢复）
//! - `__tests__/metadata-cache-ttl.test.ts`（#446 ttlMs，纯函数部分在
//!   `cache.rs` 单测；这里覆盖 runtime 写盘接线）
//! - `mcp-local-oauth-flow.test.ts`（#503 invalid_grant 重注册）
//! - `__tests__/proxy-modes-*.test.ts` 401 → needs-auth 语义
//!
//! 全部走真实 manager/protocol/proxy/oauth 栈 + HTTP stub（不 mock 内部
//! 函数），断言的是上游测试所描述的外部可观察行为。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_ext_mcp_adapter::cache::{compute_server_hash, load_metadata_cache};
use rpi_ext_mcp_adapter::direct::{resolve_direct_tools, DirectToolSpec};
use rpi_ext_mcp_adapter::manager::{ConnectionStatus, McpServerManager};
use rpi_ext_mcp_adapter::metadata::{ServerEntry, ToolPrefix};
use rpi_ext_mcp_adapter::oauth::store::{
    AuthEntry, AuthStorageOptions, MemorySecretStore, OAuthCredentialStore, StoredClientInfo,
    StoredTokens,
};
use rpi_ext_mcp_adapter::oauth::{
    authenticate_with_store, remove_auth_if_token_matches, AuthenticateOptions,
};
use rpi_ext_mcp_adapter::proxy;
use rpi_ext_mcp_adapter::status;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// ============================================================================
// 最小 HTTP/1.1 stub（与 integration_wiring.rs 相同形状）
// ============================================================================

struct StubRequest {
    method: String,
    path: String,
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
                match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&tmp[..n]),
                }
            }
            let request = StubRequest {
                method,
                path,
                body: String::from_utf8_lossy(&body).to_string(),
            };
            let (status, headers, response_body) = handler(&request);
            let mut response = format!(
                "HTTP/1.1 {status} {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                if status == 200 { "OK" } else { "ERROR" },
                response_body.len()
            );
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&response_body);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-te22-{}-{}-{}",
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// 401 stub：连接即 needs-auth（supportsOAuth 条目）。
async fn spawn_401_stub(stop: CancellationToken) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(run_stub(
        listener,
        Arc::new(|_request: &StubRequest| (401, vec![], "unauthorized".to_string())),
        stop,
    ));
    port
}

/// 占用并立即释放一个本地端口，得到一个必然拒连的地址（失败注入）。
async fn refused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

fn entry(value: Value) -> ServerEntry {
    ServerEntry(value.as_object().cloned().unwrap_or_default())
}

fn seed_cache(cache_path: &Path, definition: &ServerEntry, tool_name: &str) {
    let hash = compute_server_hash(definition).expect("server hash");
    let cache = json!({
        "version": 1,
        "servers": {
            "demo": {
                "configHash": hash,
                "cachedAt": now_ms() - 1_000,
                "tools": [ { "name": tool_name, "description": "cached tool" } ]
            }
        }
    });
    std::fs::write(cache_path, serde_json::to_string(&cache).expect("json")).expect("cache");
}

fn write_config(dir: &Path, entry: &ServerEntry, settings: Value) {
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "settings": settings,
            "mcpServers": { "demo": entry.0 },
        }))
        .expect("json"),
    )
    .expect("config");
}

async fn wait_for_failure(runtime: &Arc<proxy::McpRuntime>, server: &str) {
    for _ in 0..500 {
        if runtime.failures.failure_age_seconds(server).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("failure for {server} was not recorded in time");
}

fn direct_specs(runtime: &Arc<proxy::McpRuntime>, cache_path: &Path) -> Vec<DirectToolSpec> {
    let cache = load_metadata_cache(cache_path);
    resolve_direct_tools(
        &runtime.config,
        cache.as_ref(),
        ToolPrefix::Server,
        None,
        &runtime.active_failure_servers(),
    )
}

// ============================================================================
// A1–A5 / A3：退避贯穿（status 文本/快照、list/search/describe、direct）
// ============================================================================

#[tokio::test]
async fn backoff_hides_failed_server_from_every_tool_surface() {
    let dir = temp_dir("backoff-all");
    let cache_path = dir.join("mcp-cache.json");
    let port = refused_port().await;
    let definition = entry(json!({
        "url": format!("http://127.0.0.1:{port}/mcp"),
        "lifecycle": "eager",
        "directTools": true
    }));
    seed_cache(&cache_path, &definition, "echo");
    write_config(&dir, &definition, json!({}));

    let runtime = proxy::initialize_mcp(
        &dir,
        Some(&dir.join(".mcp.json").to_string_lossy()),
        Some(cache_path.clone()),
    )
    .await;
    wait_for_failure(&runtime, "demo").await;

    // A3: status 文本保留该 server 并标 failed/failedAgoSeconds；计数归零。
    let status_result = proxy::execute_status(&runtime);
    let status_text = status_result["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        status_text.contains("✗ demo (failed"),
        "text: {status_text}"
    );
    let demo = status_result["details"]["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .find(|s| s["name"] == "demo")
        .expect("demo row");
    assert_eq!(demo["status"], json!("failed"));
    assert_eq!(demo["toolCount"], json!(0));
    assert!(demo["failedAgo"].as_u64().is_some(), "row: {demo}");

    // A1: search 不含该 server 工具（全局 + server 级 + regex 三条路径）。
    let search = proxy::execute_search(&runtime, "echo", false, None, None, None, None);
    assert_eq!(search["details"]["count"], json!(0), "{search}");
    let search_scoped =
        proxy::execute_search(&runtime, "echo", false, Some("demo"), None, None, None);
    assert_eq!(search_scoped["details"]["error"], json!("server_backoff"));
    let search_regex = proxy::execute_search(&runtime, "echo", true, None, None, None, None);
    assert_eq!(search_regex["details"]["count"], json!(0), "{search_regex}");

    // A2: describe 不可达。
    let describe = proxy::execute_describe(&runtime, "demo_echo");
    assert_eq!(describe["details"]["error"], json!("server_backoff"));
    assert!(describe["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("not available (last failed"));

    // list / instructions 同样以退避结果短路。
    let list = proxy::execute_list(&runtime, "demo");
    assert_eq!(list["details"]["error"], json!("server_backoff"));
    assert_eq!(list["details"]["count"], json!(0));
    let instructions = proxy::execute_instructions(&runtime, "demo");
    assert_eq!(instructions["details"]["error"], json!("server_backoff"));

    // A4: direct surface 摘除该 server 的工具（缓存仍含 echo）。
    let specs = direct_specs(&runtime, &cache_path);
    assert!(specs.is_empty(), "specs: {specs:?}");

    // 快照：failed + toolCount 0 + directToolCount 0（#484）。direct 计数由
    // 调用方喂入实际 sync 结果（上游 `state.directToolCounts`），这里用真实
    // 解析结果构造，断言「快照读数 == 实际暴露数」。
    let counts = |specs: &[DirectToolSpec]| -> Vec<(String, usize)> {
        let mut map: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for spec in specs {
            *map.entry(spec.server_name.clone()).or_insert(0) += 1;
        }
        map.into_iter().collect()
    };
    let snapshot = status::create_mcp_status_snapshot(
        &runtime.config,
        &runtime.manager,
        &[("demo".to_string(), 1)],
        &counts(&specs),
        &[],
        &[("demo".to_string(), now_ms() - 1_000)],
    );
    let row = snapshot
        .servers
        .iter()
        .find(|s| s.name == "demo")
        .expect("snapshot row");
    assert_eq!(row.status, status::ServerRuntimeStatus::Failed);
    assert_eq!(row.tool_count, 0);
    assert_eq!(row.direct_tool_count, 0);
    assert!(row.failed_ago_seconds.is_some());

    // A5: 失败窗口过期后各入口恢复可见（时间戳回拨 61s 触发过期语义）。
    runtime
        .failures
        .record_failure_at("demo", now_ms() - 61_000, "boom");
    assert!(!runtime.is_server_in_active_failure_backoff("demo"));
    let restored = proxy::execute_search(&runtime, "echo", false, None, None, None, None);
    assert_eq!(restored["details"]["count"], json!(1), "{restored}");
    let restored_describe = proxy::execute_describe(&runtime, "demo_echo");
    assert_eq!(restored_describe["details"]["mode"], json!("describe"));
    let restored_list = proxy::execute_list(&runtime, "demo");
    assert!(restored_list["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("lazy: tools from cache"));
    let restored_specs = direct_specs(&runtime, &cache_path);
    assert_eq!(restored_specs.len(), 1, "specs: {restored_specs:?}");
    let restored_snapshot = status::create_mcp_status_snapshot(
        &runtime.config,
        &runtime.manager,
        &[("demo".to_string(), 1)],
        &counts(&restored_specs),
        &[],
        &[],
    );
    let restored_row = restored_snapshot
        .servers
        .iter()
        .find(|s| s.name == "demo")
        .expect("restored snapshot row");
    assert_eq!(restored_row.direct_tool_count, 1);
    let restored_status = proxy::execute_status(&runtime);
    assert_eq!(
        restored_status["details"]["servers"][0]["status"],
        json!("cached")
    );

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// A6 / R7.2.3.2（#474）：列表状态区分 needs-auth 与 cached lazy
// ============================================================================

#[tokio::test]
async fn list_status_distinguishes_needs_auth_from_cached_lazy() {
    let stop = CancellationToken::new();

    // needs-auth：401 stub + 可 OAuth 条目。
    let auth_dir = temp_dir("list-needs-auth");
    let auth_cache = auth_dir.join("mcp-cache.json");
    let auth_port = spawn_401_stub(stop.clone()).await;
    let auth_definition = entry(json!({
        "url": format!("http://127.0.0.1:{auth_port}/mcp"),
        "lifecycle": "lazy",
        "directTools": true
    }));
    seed_cache(&auth_cache, &auth_definition, "echo");
    write_config(&auth_dir, &auth_definition, json!({}));
    let auth_runtime = proxy::initialize_mcp(
        &auth_dir,
        Some(&auth_dir.join(".mcp.json").to_string_lossy()),
        Some(auth_cache.clone()),
    )
    .await;
    let connect_result = proxy::execute_connect(&auth_runtime, "demo").await;
    assert_eq!(connect_result["details"]["error"], json!("auth_required"));
    assert_eq!(
        auth_runtime
            .manager
            .get_connection("demo")
            .map(|c| c.status()),
        Some(ConnectionStatus::NeedsAuth)
    );
    let auth_list = proxy::execute_list(&auth_runtime, "demo");
    let auth_text = auth_list["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        auth_text.contains("needs auth — run mcp({ action: \"auth-start\""),
        "text: {auth_text}"
    );

    // cached lazy：从未连接（lazy 生命周期）+ 有效缓存 → 缓存工具可见。
    let lazy_dir = temp_dir("list-cached-lazy");
    let lazy_cache = lazy_dir.join("mcp-cache.json");
    let lazy_definition = entry(json!({
        "url": "http://127.0.0.1:9/mcp",
        "lifecycle": "lazy",
        "directTools": true
    }));
    seed_cache(&lazy_cache, &lazy_definition, "echo");
    write_config(&lazy_dir, &lazy_definition, json!({}));
    let lazy_runtime = proxy::initialize_mcp(
        &lazy_dir,
        Some(&lazy_dir.join(".mcp.json").to_string_lossy()),
        Some(lazy_cache.clone()),
    )
    .await;
    assert!(lazy_runtime.manager.get_connection("demo").is_none());
    let lazy_list = proxy::execute_list(&lazy_runtime, "demo");
    let lazy_text = lazy_list["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        lazy_text.contains("lazy: tools from cache, not connected yet"),
        "text: {lazy_text}"
    );

    auth_runtime.owner_cancel.cancel();
    auth_runtime.manager.close_all().await;
    lazy_runtime.owner_cancel.cancel();
    lazy_runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&auth_dir);
    let _ = std::fs::remove_dir_all(&lazy_dir);
}

// ============================================================================
// A10 / R7.2.9.1（#422/#423）：401 → compare-and-delete 存储凭据
// ============================================================================

#[tokio::test]
async fn oauth_401_deletes_only_the_failed_stored_token() {
    let stop = CancellationToken::new();
    let port = spawn_401_stub(stop.clone()).await;
    let server_url = format!("http://127.0.0.1:{port}/mcp");

    let store = Arc::new(OAuthCredentialStore::with_backend(
        Box::new(MemorySecretStore::new()),
        AuthStorageOptions::default(),
    ));
    store
        .save_entry(
            "demo",
            AuthEntry {
                tokens: Some(StoredTokens {
                    access_token: "stale-token".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(&server_url),
        )
        .expect("seed credential");

    let manager = McpServerManager::new(None);
    manager.set_auth_store_override(store.clone());
    let definition = entry(json!({ "url": server_url }));
    let connection = manager
        .connect("demo", &definition)
        .await
        .expect("connect returns needs-auth placeholder");
    assert_eq!(connection.status(), ConnectionStatus::NeedsAuth);
    assert!(
        store
            .get_for_url("demo", &server_url)
            .expect("store read")
            .is_none(),
        "the failed token must be removed"
    );

    // compare-and-delete：存储条目已被其它进程替换为 new-token 时不得删除。
    store
        .save_entry(
            "demo",
            AuthEntry {
                tokens: Some(StoredTokens {
                    access_token: "new-token".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(&server_url),
        )
        .expect("replacement credential");
    assert!(
        !remove_auth_if_token_matches(&store, "demo", &server_url, "stale-token")
            .expect("compare-and-delete")
    );
    let preserved = store
        .get_for_url("demo", &server_url)
        .expect("store read")
        .expect("entry preserved");
    assert_eq!(
        preserved.tokens.as_ref().map(|t| t.access_token.as_str()),
        Some("new-token")
    );

    manager.close_all().await;
    stop.cancel();
}

// ============================================================================
// A11 / R7.2.9.2（#503）：invalid_grant 后重注册陈旧动态客户端
// ============================================================================

/// 记录 OAuth 请求的 stub AS（RFC 8414 + DCR + authorize 302 + token）。
struct OAuthStub {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    _stop: CancellationToken,
}

impl OAuthStub {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

async fn spawn_oauth_stub() -> OAuthStub {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stop = CancellationToken::new();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let base = format!("http://127.0.0.1:{port}");
            let json_header = vec![("content-type".to_string(), "application/json".to_string())];
            if request.method == "GET" && request.path == "/.well-known/oauth-authorization-server"
            {
                return (
                    200,
                    json_header,
                    json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "response_types_supported": ["code"],
                        "code_challenge_methods_supported": ["S256"],
                        "grant_types_supported": ["authorization_code", "refresh_token"],
                        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
                    })
                    .to_string(),
                );
            }
            if request.method == "GET" && request.path.starts_with("/authorize") {
                let query = request.path.split_once('?').map(|(_, q)| q).unwrap_or("");
                let params: std::collections::HashMap<String, String> =
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect();
                recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(json!({ "kind": "authorize", "params": params }));
                let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
                let state = params.get("state").cloned().unwrap_or_default();
                let mut target = url::Url::parse(&redirect_uri).expect("redirect uri");
                target.query_pairs_mut().append_pair("code", "stub-code");
                if !state.is_empty() {
                    target.query_pairs_mut().append_pair("state", &state);
                }
                return (
                    302,
                    vec![("location".to_string(), target.to_string())],
                    String::new(),
                );
            }
            if request.method == "POST" && request.path == "/token" {
                let form: std::collections::HashMap<String, String> =
                    url::form_urlencoded::parse(request.body.as_bytes())
                        .into_owned()
                        .collect();
                recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(json!({ "kind": "token", "params": form }));
                if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
                    return (
                        400,
                        json_header,
                        json!({ "error": "invalid_grant" }).to_string(),
                    );
                }
                return (
                    200,
                    json_header,
                    json!({
                        "access_token": "fresh-access-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "fresh-refresh-token",
                    })
                    .to_string(),
                );
            }
            if request.method == "POST" && request.path == "/register" {
                let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
                recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(json!({ "kind": "register", "body": body }));
                let redirect_uris = body
                    .get("redirect_uris")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                return (
                    201,
                    json_header,
                    json!({
                        "client_id": "new-dynamic-client",
                        "client_secret": "new-dynamic-secret",
                        "redirect_uris": redirect_uris,
                    })
                    .to_string(),
                );
            }
            (404, json_header, json!({}).to_string())
        }),
        stop.clone(),
    ));
    OAuthStub {
        port,
        requests,
        _stop: stop,
    }
}

#[tokio::test]
async fn oauth_invalid_grant_reregisters_stale_dynamic_client() {
    let stub = spawn_oauth_stub().await;
    let server_url = format!("{}/mcp", stub.base());

    let dir = temp_dir("oauth-invalid-grant");
    let store = OAuthCredentialStore::with_backend(
        Box::new(MemorySecretStore::new()),
        AuthStorageOptions {
            base_dir: Some(dir.clone()),
        },
    );
    // 陈旧动态客户端 + 过期 token + 可刷新 refresh token（issuer 绑定让
    // authenticate 走 refresh 分支）。
    store
        .save_entry(
            "demo",
            AuthEntry {
                tokens: Some(StoredTokens {
                    access_token: "expired-access".to_string(),
                    refresh_token: Some("revoked-refresh".to_string()),
                    expires_at: Some(1.0),
                    issuer: Some(stub.base()),
                    ..Default::default()
                }),
                client_info: Some(StoredClientInfo {
                    client_id: "stale-dynamic-client".to_string(),
                    redirect_uris: Some(vec!["http://localhost:1/callback".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(&server_url),
        )
        .expect("seed stale credentials");

    let definition = entry(json!({ "url": server_url, "oauth": {} }));
    let options = AuthenticateOptions {
        on_authorization_url: Some(Arc::new(|url: &str| {
            let url = url.to_string();
            tokio::spawn(async move {
                // 无头浏览器：GET authorize → stub 302 → 回调 listener。
                let _ = reqwest::get(&url).await;
            });
        })),
        auth_storage_options: AuthStorageOptions {
            base_dir: Some(dir.clone()),
        },
        ..Default::default()
    };
    let status = authenticate_with_store(&store, "demo", &server_url, &definition, &options)
        .await
        .expect("authenticate completes after re-registration");
    assert!(format!("{status:?}").contains("Authenticated"));

    let requests = stub.requests();
    // 刷新请求体：逐字段（RFC 6749 §6 + stale client id）。
    let refresh = requests
        .iter()
        .find(|r| r["kind"] == "token" && r["params"]["grant_type"] == "refresh_token")
        .expect("refresh attempt recorded");
    assert_eq!(refresh["params"]["refresh_token"], json!("revoked-refresh"));
    assert_eq!(
        refresh["params"]["client_id"],
        json!("stale-dynamic-client")
    );

    // DCR 重注册：redirect_uris 为新回调端口；请求体字段与上游
    // clientMetadata 一致（client_uri 为 rpi 品牌 [VARIANT]）。
    let register = requests
        .iter()
        .find(|r| r["kind"] == "register")
        .expect("re-registration recorded");
    let body = &register["body"];
    assert_eq!(
        body["redirect_uris"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0),
        1
    );
    let registered_redirect = body["redirect_uris"][0].as_str().unwrap_or_default();
    assert!(registered_redirect.starts_with("http://localhost:"));
    assert!(registered_redirect.ends_with("/callback"));
    assert_ne!(registered_redirect, "http://localhost:1/callback");
    assert_eq!(body["client_name"], json!("rpi"));
    assert_eq!(body["client_uri"], json!("https://rpi.dev"));
    assert_eq!(
        body["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(body["response_types"], json!(["code"]));
    assert_eq!(body["token_endpoint_auth_method"], json!("none"));
    assert_eq!(body["application_type"], json!("native"));

    // 授权 URL 使用新注册的 client + 回调。
    let authorize = requests
        .iter()
        .find(|r| r["kind"] == "authorize")
        .expect("authorize recorded");
    assert_eq!(
        authorize["params"]["client_id"],
        json!("new-dynamic-client")
    );
    assert_eq!(
        authorize["params"]["redirect_uri"],
        json!(registered_redirect)
    );

    // 存储态：client 注册被替换、token 换成新值。
    let entry = store
        .get_for_url("demo", &server_url)
        .expect("store read")
        .expect("entry");
    assert_eq!(
        entry.client_info.as_ref().map(|c| c.client_id.as_str()),
        Some("new-dynamic-client")
    );
    assert_eq!(
        entry.tokens.as_ref().map(|t| t.access_token.as_str()),
        Some("fresh-access-token")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// A9 / R7.2.6.1（#446）：`tools/list` 的 ttlMs/cacheScope 写盘接线
// ============================================================================

async fn spawn_hint_stub(stop: CancellationToken, hints: Value) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(run_stub(
        listener,
        Arc::new(move |request: &StubRequest| {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            let json_header = vec![("content-type".to_string(), "application/json".to_string())];
            match method {
                "initialize" => (
                    200,
                    json_header,
                    rpc_result(&request.body, initialize_result("2025-03-26")),
                ),
                "tools/list" => {
                    let mut result = json!({ "tools": [{ "name": "echo" }] });
                    if let Value::Object(hints) = &hints {
                        for (key, value) in hints {
                            result[key] = value.clone();
                        }
                    }
                    (200, json_header, rpc_result(&request.body, result))
                }
                _ => (200, json_header, rpc_result(&request.body, json!({}))),
            }
        }),
        stop,
    ));
    port
}

#[tokio::test]
async fn runtime_persists_tools_list_cache_hints() {
    let stop = CancellationToken::new();
    let dir = temp_dir("ttl-hints");
    let cache_path = dir.join("mcp-cache.json");
    let port = spawn_hint_stub(
        stop.clone(),
        json!({ "ttlMs": 12_345, "cacheScope": "private" }),
    )
    .await;
    let definition = entry(json!({
        "url": format!("http://127.0.0.1:{port}/mcp"),
        "lifecycle": "eager"
    }));
    write_config(&dir, &definition, json!({}));
    let runtime = proxy::initialize_mcp(
        &dir,
        Some(&dir.join(".mcp.json").to_string_lossy()),
        Some(cache_path.clone()),
    )
    .await;

    let mut entry_value = None;
    for _ in 0..500 {
        if let Some(cache) = load_metadata_cache(&cache_path) {
            if let Some(entry) = cache.servers.get("demo") {
                entry_value = Some(entry.clone());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hints_entry = entry_value.expect("cache entry written");
    assert_eq!(hints_entry.ttl_ms, Some(12_345));
    assert_eq!(hints_entry.cache_scope.as_deref(), Some("private"));
    assert!(rpi_ext_mcp_adapter::cache::is_server_cache_valid(
        &hints_entry,
        &definition,
        rpi_ext_mcp_adapter::cache::CACHE_MAX_AGE_MS,
        now_ms()
    ));

    // ttlMs == 0 → 缓存立即失效（即便 maxAge 未到）。
    let zero_dir = temp_dir("ttl-zero");
    let zero_cache = zero_dir.join("mcp-cache.json");
    let zero_port = spawn_hint_stub(stop.clone(), json!({ "ttlMs": 0 })).await;
    let zero_definition = entry(json!({
        "url": format!("http://127.0.0.1:{zero_port}/mcp"),
        "lifecycle": "eager"
    }));
    write_config(&zero_dir, &zero_definition, json!({}));
    let zero_runtime = proxy::initialize_mcp(
        &zero_dir,
        Some(&zero_dir.join(".mcp.json").to_string_lossy()),
        Some(zero_cache.clone()),
    )
    .await;
    let mut zero_entry = None;
    for _ in 0..500 {
        if let Some(cache) = load_metadata_cache(&zero_cache) {
            if let Some(entry) = cache.servers.get("demo") {
                zero_entry = Some(entry.clone());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let zero_entry = zero_entry.expect("zero-ttl cache entry written");
    assert_eq!(zero_entry.ttl_ms, Some(0));
    assert!(!rpi_ext_mcp_adapter::cache::is_server_cache_valid(
        &zero_entry,
        &zero_definition,
        rpi_ext_mcp_adapter::cache::CACHE_MAX_AGE_MS,
        now_ms()
    ));

    runtime.owner_cancel.cancel();
    runtime.manager.close_all().await;
    zero_runtime.owner_cancel.cancel();
    zero_runtime.manager.close_all().await;
    stop.cancel();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&zero_dir);
}
