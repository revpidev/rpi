//! `/resume` 状态行丢失回归（独立二进制：STATE OnceLock 只允许本二进制
//! 内一次 install_for_test 成功构建 PluginState；本测试正是在其上验证
//! 第二个宿主的 rebind 路径）。
//!
//! 复现序列（对应用户报告「/resume 后 🔌 MCP: 1 server enabled 消失」）：
//! 1. 宿主 A（TUI 已接）install + session_start → on_ready 发布
//!    "🔌 MCP: 1 server enabled"；
//! 2. `/resume` teardown：session_shutdown → 状态行清空（text=null）、
//!    dispatcher 复位 NotStarted；
//! 3. 新宿主 B（create_runtime 重载同一 dlopen 记忆化路径，UI 桥尚未
//!    rebind）再次 install —— 修复前返回
//!    `{"error":{"kind":"init","message":"plugin already initialized"}}`，
//!    扩展在新宿主上整体缺失（工具/flag/事件/状态行全没）；
//! 4. 修复后：rebind 成功，B 上重新注册 flag/事件/mcp 代理工具，
//!    session_start 在 B 上重新初始化，UI 接上后重试循环把状态行
//!    推回 B —— 且旧宿主 A 不再收到任何推送（无 stale 通道）。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::{dispatch, install_for_test, BRIDGE_RETRY_MS_TEST};
use serde_json::{json, Value};

const KEY: &str = "mcp";

/// 进程内假宿主：cwd 可变（模拟跨目录 /resume）、hasUI 可翻转（模拟
/// rebind_session_ui 前后）、记录 registerTool/on/registerFlag 与
/// ui.setStatus（hasUI=false 时丢弃——对齐真宿主 null-bridge 行为）。
struct FakeHost {
    cwd: StdMutex<String>,
    has_ui: StdMutex<bool>,
    registered: StdMutex<Vec<Value>>,
    events: StdMutex<Vec<String>>,
    flags: StdMutex<Vec<String>>,
    status: StdMutex<Vec<(String, Option<String>)>>,
}

impl FakeHost {
    fn new(cwd: &str, has_ui: bool) -> Arc<Self> {
        Arc::new(Self {
            cwd: StdMutex::new(cwd.to_string()),
            has_ui: StdMutex::new(has_ui),
            registered: StdMutex::new(Vec::new()),
            events: StdMutex::new(Vec::new()),
            flags: StdMutex::new(Vec::new()),
            status: StdMutex::new(Vec::new()),
        })
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

    fn tool_names(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|d| d.get("name").and_then(Value::as_str).map(str::to_string))
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
        "ctx.hasUI" => {
            json!({"ok": *host.has_ui.lock().unwrap_or_else(|e| e.into_inner())})
        }
        "ui.setStatus" => {
            // 对齐真宿主：无 UI 桥时 ui.setStatus 落在 null bridge 上被丢弃。
            let has_ui = *host.has_ui.lock().unwrap_or_else(|e| e.into_inner());
            if has_ui {
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
        "registerFlag" => {
            if let Some(name) = args.get("name").and_then(Value::as_str) {
                host.flags
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(name.to_string());
            }
            json!({"ok": true})
        }
        "on" => {
            if let Some(event) = args.get("event").and_then(Value::as_str) {
                host.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event.to_string());
            }
            json!({"ok": true})
        }
        "unregisterTool" => json!({"ok": false}),
        "getFlag" | "getActiveTools" | "getAllTools" | "setActiveTools" => json!({"ok": null}),
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

/// 在独立线程上 dispatch（事件/toolExecute 处理器内部 block_on，必须
/// 离开 tokio 测试线程）。
fn dispatch_on_thread(cookie: PluginCookie, message: Value) -> Value {
    let bytes = serde_json::to_vec(&message).expect("json");
    let cookie = cookie as usize;
    let reply = std::thread::scope(|s| {
        s.spawn(move || dispatch(cookie as PluginCookie, RVec::from(bytes)))
            .join()
            .expect("dispatch thread")
    });
    serde_json::from_slice(&reply[..]).unwrap_or(Value::Null)
}

async fn wait_for_ready(timeout: std::time::Duration) -> bool {
    let dispatcher = rpi_ext_mcp_adapter::dispatcher_for_test().expect("state");
    tokio::time::timeout(timeout, async {
        while dispatcher.try_runtime().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_rebinds_second_host_and_republishes_status() {
    BRIDGE_RETRY_MS_TEST.store(30, Ordering::Relaxed);

    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-rebind-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    // 单个 lazy 服务器：未连接也显示 "🔌 MCP: 1 server enabled"（用户
    // 报告的原文），且不会触发 load-time prewarm。
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "demo": { "url": "http://127.0.0.1:9/mcp" }
            }
        }))
        .expect("json"),
    )
    .expect("config");

    std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
    let saved_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &dir);

    // ── 宿主 A：进程启动 ─────────────────────────────────────────────
    let host_a = FakeHost::new(&dir.to_string_lossy(), true);
    let ptr_a = Arc::into_raw(host_a.clone()) as PluginCookie;
    let installed = install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        ptr_a,
    );
    assert_eq!(installed, json!({"ok": true}), "install A must succeed");

    // session_start → 后台 init → on_ready 发布状态行。
    dispatch_on_thread(ptr_a, json!({"kind": "event", "event": "session_start"}));
    assert!(
        wait_for_ready(std::time::Duration::from_secs(10)).await,
        "init A must complete"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let calls = host_a.status_calls(KEY);
    assert_eq!(
        calls.last(),
        Some(&Some("🔌 MCP: 1 server enabled".to_string())),
        "on_ready must publish the footer on host A: {calls:?}"
    );
    let a_status_len = calls.len();
    let a_registered_mcp = host_a.tool_names().iter().filter(|n| *n == "mcp").count();

    // ── `/resume` teardown：session_shutdown（旧宿主） ────────────────
    dispatch_on_thread(ptr_a, json!({"kind": "event", "event": "session_shutdown"}));
    let calls = host_a.status_calls(KEY);
    assert_eq!(
        calls.last(),
        Some(&None),
        "session_shutdown must clear the footer entry: {calls:?}"
    );
    let dispatcher = rpi_ext_mcp_adapter::dispatcher_for_test().expect("state");
    assert!(
        dispatcher.try_runtime().is_none(),
        "session_shutdown must reset the dispatcher gate"
    );
    let a_status_len_after_shutdown = calls.len();

    // ── 宿主 B：create_runtime 重载同一插件（修复点） ─────────────────
    // UI 桥尚未 rebind（interactive 模式在 switch_session 返回后才
    // set_ui），hasUI=false。
    let host_b = FakeHost::new(&dir.to_string_lossy(), false);
    let ptr_b = Arc::into_raw(host_b.clone()) as PluginCookie;
    let installed = install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        ptr_b,
    );
    assert_eq!(
        installed,
        json!({"ok": true}),
        "second-host install must rebind, not fail with `plugin already initialized`"
    );
    // 新宿主上的注册面（fresh host registry 为空，必须全部重新注册）。
    assert_eq!(host_b.flags.lock().unwrap().clone(), vec!["mcp-config"]);
    let mut events = host_b.events.lock().unwrap().clone();
    events.sort();
    assert_eq!(
        events,
        vec!["session_shutdown", "session_start", "tool_result"],
        "event handlers must be re-registered on host B"
    );
    assert!(
        host_b.tool_names().iter().any(|n| n == "mcp"),
        "mcp proxy tool must be re-registered on host B: {:?}",
        host_b.tool_names()
    );

    // session_start 在 B 上重新初始化 → on_ready 通过新通道推送。
    dispatch_on_thread(ptr_b, json!({"kind": "event", "event": "session_start"}));
    assert!(
        wait_for_ready(std::time::Duration::from_secs(10)).await,
        "init B must complete"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // hasUI=false：on_ready 的推送落在 null bridge 上被丢弃——B 还看不到。
    assert!(
        host_b.status_calls(KEY).is_empty(),
        "no UI yet: pushes must be dropped by the null bridge: {:?}",
        host_b.status_calls(KEY)
    );
    // 旧宿主 A 在 rebind 之后不得再收到任何推送（stale 通道回归断言）。
    assert_eq!(
        host_a.status_calls(KEY).len(),
        a_status_len_after_shutdown,
        "host A must not receive post-rebind pushes"
    );
    assert_eq!(
        host_a.tool_names().iter().filter(|n| *n == "mcp").count(),
        a_registered_mcp,
        "host A registry must be untouched by the rebind"
    );

    // ── rebind_session_ui：UI 桥接上 → 重试循环把状态行推回 ──────────
    *host_b.has_ui.lock().unwrap_or_else(|e| e.into_inner()) = true;
    let republished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while host_b.status_calls(KEY).is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        republished,
        "bridge-retry must republish the footer once the UI is bound"
    );
    assert_eq!(
        host_b.status_calls(KEY).last(),
        Some(&Some("🔌 MCP: 1 server enabled".to_string())),
        "the republished footer must carry the enabled counts: {:?}",
        host_b.status_calls(KEY)
    );

    // 环境恢复。
    std::env::remove_var("RPI_CODING_AGENT_DIR");
    match saved_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = a_status_len;
}
