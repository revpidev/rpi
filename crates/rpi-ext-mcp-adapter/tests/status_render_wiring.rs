//! 状态行 + renderResult 折叠接线测试（独立二进制：STATE OnceLock 只允许
//! 本二进制内一次 install_for_test）。
//!
//! 上游对应：
//! - init.ts:520-556 `updateStatusBar`（ui.setStatus("mcp", ...)）
//! - index.ts:161-162 / :719（direct + proxy 工具注册带 renderResult）
//! - tool-result-renderer.ts:269-297 `renderMcpToolResult`

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::{dispatch, install_for_test};
use serde_json::{json, Value};

/// 进程内假宿主：记录 registerTool 的 definition 全量与 ui.setStatus 序列。
struct FakeHost {
    cwd: StdMutex<String>,
    registered: StdMutex<Vec<Value>>,
    status: StdMutex<Vec<(String, Option<String>)>>,
}

impl FakeHost {
    fn new(cwd: &str) -> Arc<Self> {
        Arc::new(Self {
            cwd: StdMutex::new(cwd.to_string()),
            registered: StdMutex::new(Vec::new()),
            status: StdMutex::new(Vec::new()),
        })
    }

    fn definitions(&self, name: &str) -> Vec<Value> {
        self.registered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|d| d.get("name").and_then(Value::as_str) == Some(name))
            .cloned()
            .collect()
    }

    fn status_calls(&self, key: &str) -> Vec<Option<String>> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, text)| text.clone())
            .collect()
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
        "getFlag" | "getActiveTools" | "getAllTools" => json!({"ok": null}),
        "on" | "registerFlag" | "setActiveTools" | "ui.setStatus" => {
            if method == "ui.setStatus" {
                let key = args.get("key").and_then(Value::as_str).unwrap_or("");
                let text = args.get("text").and_then(Value::as_str).map(str::to_string);
                host.status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((key.to_string(), text));
            }
            json!({"ok": true})
        }
        "registerTool" => {
            if let Some(definition) = args.get("definition") {
                host.registered
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(definition.clone());
            }
            json!({"ok": true})
        }
        "unregisterTool" => json!({"ok": false}),
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_bar_and_render_result_are_wired_to_the_host() {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-statusrender-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // 元数据缓存：给 demo 造一份 direct 工具（无需真实连接）。
    let entry =
        json!({ "url": "http://127.0.0.1:9/mcp", "lifecycle": "eager", "directTools": true });
    let definition =
        rpi_ext_mcp_adapter::metadata::ServerEntry(entry.as_object().cloned().unwrap_or_default());
    let config_hash = rpi_ext_mcp_adapter::cache::compute_server_hash(&definition).expect("hash");
    let cache = json!({
        "version": 1,
        "servers": {
            "demo": {
                "configHash": config_hash,
                "cachedAt": now - 1000,
                "tools": [ { "name": "echo", "description": "v1" } ]
            }
        }
    });
    std::fs::write(
        agent_dir.join("mcp-cache.json"),
        serde_json::to_string(&cache).expect("json"),
    )
    .expect("cache");

    // settings 未设置 mcpFooterStatus → 上游默认 full。
    // demo2：lazy 档、不可连接（127.0.0.1:9），用于懒连接瞬态断言。
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "demo": entry,
                "demo2": { "url": "http://127.0.0.1:9/mcp" }
            }
        }))
        .expect("json"),
    )
    .expect("config");

    std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
    let saved_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &dir);

    let host = FakeHost::new(&dir.to_string_lossy());
    let host_ptr = Arc::into_raw(host.clone()) as PluginCookie;
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, host_ptr);
    assert_eq!(installed, json!({"ok": true}), "install must succeed");

    // load-time prewarm → 后台 init → on_ready 钩子发布状态行。
    let dispatcher = rpi_ext_mcp_adapter::dispatcher_for_test().expect("state");
    let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while dispatcher.try_runtime().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ready.is_ok(), "init must complete in the background");
    // on_ready 在 Ready 发布后同步执行；稍等一拍让钩子跑完。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // updateStatusBar（init.ts:520-556）：默认 full、无连接、showStatusIcon
    // 默认开启 → "🔌 MCP: 2 servers enabled"。
    let calls = host.status_calls("mcp");
    assert!(!calls.is_empty(), "on_ready must publish ui.setStatus");
    assert_eq!(
        calls.last(),
        Some(&Some("🔌 MCP: 2 servers enabled".to_string()))
    );

    // renderResult 标记：proxy `mcp` 工具 + direct 工具。
    let mcp_defs = host.definitions("mcp");
    assert!(
        mcp_defs
            .iter()
            .any(|d| d.get("renderResult") == Some(&json!(true))),
        "mcp tool definition must carry renderResult: {mcp_defs:?}"
    );
    let echo_defs = host.definitions("demo_echo");
    assert!(
        echo_defs
            .iter()
            .any(|d| d.get("renderResult") == Some(&json!(true))),
        "direct tool definition must carry renderResult: {echo_defs:?}"
    );

    // 渲染分发（host_call.rs:263-281 的回程）：折叠 + identity 行。
    let message = json!({
        "kind": "render",
        "what": "toolResult",
        "result": {
            "content": [{ "type": "text", "text": "one\ntwo\nthree\nfour" }],
            "details": { "mode": "call", "server": "demo", "tool": "echo" }
        },
        "options": { "expanded": false, "isPartial": false },
        "context": { "isError": false },
    });
    let reply = dispatch(
        host_ptr,
        RVec::from(serde_json::to_vec(&message).expect("json")),
    );
    let tree: Value = serde_json::from_slice(&reply[..]).expect("component tree");
    assert_eq!(tree["type"], json!("text"));
    assert_eq!(
        tree["props"]["text"],
        json!("MCP demo/echo\none\ntwo\nthree\n…\n(Ctrl+O to expand)")
    );

    // 其他 what → null（宿主按错误处理，不触发）。
    let message = json!({ "kind": "render", "what": "toolCall", "context": {} });
    let reply = dispatch(
        host_ptr,
        RVec::from(serde_json::to_vec(&message).expect("json")),
    );
    let tree: Value = serde_json::from_slice(&reply[..]).expect("json");
    assert_eq!(tree, Value::Null);

    // 懒连接瞬态与失败回刷（init.ts:588-591 + proxy-modes.ts:760）：
    // call 一个未缓存工具的 lazy 服务器 → 先 "connecting to demo2..."，
    // 连接失败（127.0.0.1:9 拒绝）后回刷为计数文本。
    // toolExecute 内部走 block_on，必须离开 tokio 测试线程再 dispatch。
    let message = json!({
        "kind": "toolExecute",
        "toolName": "mcp",
        "params": { "tool": "echo", "server": "demo2", "args": "{}" },
    });
    let bytes = serde_json::to_vec(&message).expect("json");
    let cookie = host_ptr as usize;
    let reply = std::thread::scope(|s| {
        s.spawn(move || dispatch(cookie as PluginCookie, RVec::from(bytes)))
            .join()
            .expect("dispatch thread")
    });
    let _ = reply;
    let calls = host.status_calls("mcp");
    assert!(
        calls
            .iter()
            .any(|t| t.as_deref() == Some("🔌 MCP: connecting to demo2...")),
        "lazy connect must publish the transient status: {calls:?}"
    );
    assert_eq!(
        calls.last(),
        Some(&Some("🔌 MCP: 2 servers enabled".to_string())),
        "failed lazy connect must repaint the counts: {calls:?}"
    );

    // 环境恢复。
    std::env::remove_var("RPI_CODING_AGENT_DIR");
    match saved_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
