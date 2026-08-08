//! T15 W6 host-level tests (WAT fixtures — no wasm32 toolchain needed:
//! wasmtime compiles the WAT text directly).
//!
//! ABI v1 coverage: init registration/receipt, serial dispatch, toolExecute
//! forwarding, capability denial (bare .wasm vs manifest), fuel exhaustion,
//! missing exports, cache reuse.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pir_ext_host::host::NativeExtensionHost;
use serde_json::json;

/// tool_call gate guest：on("tool_call")，dispatch 恒返回 block。
const GATE_GUEST_WAT: &str = r#"
(module
  (import "pir" "pir_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "pir_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func (export "pir_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "pir_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"block\":true,\"reason\":\"wasm-gate\"}\00")
)
"#;

/// 工具 guest：registerTool("wasm_tool")，toolExecute 恒返回固定结果。
const TOOL_GUEST_WAT: &str = r#"
(module
  (import "pir" "pir_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "pir_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func (export "pir_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "pir_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"registerTool\",\"args\":{\"name\":\"wasm_tool\",\"label\":\"Wasm Tool\",\"description\":\"wasm tool\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"content\":[{\"type\":\"text\",\"text\":\"wasm-output\"}],\"details\":null}\00")
)
"#;

/// 能力探针 guest：init 调 registerTool 并把响应原样作为回执返回。
const PROBE_GUEST_WAT: &str = r#"
(module
  (import "pir" "pir_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "pir_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func (export "pir_extension_init") (result i64)
    ;; 直接转发 host_call 的响应作为 init 回执。
    (return (call $host_call (i32.const 16) (call $strlen (i32.const 16)))))
  (func (export "pir_dispatch") (param i32 i32) (result i64)
    (return (i64.const 0)))
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (data (i32.const 16) "{\"call\":\"registerTool\",\"args\":{\"name\":\"probe_tool\",\"description\":\"p\",\"parameters\":{\"type\":\"object\"}}}\00")
)
"#;

/// 死循环 guest：init 永不返回，fuel 耗尽应 trap 成加载错误。
const LOOP_GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "pir_alloc") (param i32) (result i32) (i32.const 64))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func (export "pir_extension_init") (result i64)
    (loop $spin (br $spin))
    (i64.const 0))
  (func (export "pir_dispatch") (param i32 i32) (result i64) (i64.const 0))
)
"#;

/// 缺 pir_dispatch 导出的 guest。
const NO_DISPATCH_GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "pir_alloc") (param i32) (result i32) (i32.const 64))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func (export "pir_extension_init") (result i64) (i64.const 0))
)
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pir-ext-w6-{tag}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// 写一个 guest 包目录：dist/*.wat（.wasm 扩展名）+ 可选 manifest。
    fn write_guest(&self, dir_name: &str, wat: &str, manifest: Option<&str>) -> PathBuf {
        let dir = self.0.join(dir_name);
        std::fs::create_dir_all(dir.join("dist")).expect("dist");
        let wasm = dir.join("dist/guest.wasm");
        std::fs::write(&wasm, wat).expect("write guest");
        if let Some(manifest) = manifest {
            std::fs::write(dir.join("pir-extension.json"), manifest).expect("write manifest");
        }
        wasm
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn host_loading(paths: &[PathBuf]) -> (NativeExtensionHost, Vec<String>) {
    let host = NativeExtensionHost::new("/w6-cwd");
    let errors = host
        .load_paths(paths)
        .await
        .iter()
        .map(|e| e.error.clone())
        .collect::<Vec<_>>();
    (host, errors)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_gate_guest_blocks_tool_call() {
    let tmp = TempDir::new("gate");
    let wasm = tmp.write_guest(
        "gate",
        GATE_GUEST_WAT,
        Some(r#"{"name":"gate","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":[],"pirAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let result = host
        .emit_tool_call(json!({"type":"tool_call","toolCallId":"c1","toolName":"bash","input":{"command":"rm -rf /"}}))
        .await
        .expect("no host error")
        .expect("gate blocked");
    assert_eq!(result["block"], true);
    assert_eq!(result["reason"], "wasm-gate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_tool_guest_registers_and_executes() {
    let tmp = TempDir::new("tool");
    let wasm = tmp.write_guest(
        "tool",
        TOOL_GUEST_WAT,
        Some(r#"{"name":"tool","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"pirAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let definition = host.get_tool_definition("wasm_tool").expect("registered");
    assert_eq!(definition.description, "wasm tool");
    let request = pir_ext_host::types::ToolExecuteRequest {
        tool_call_id: "tc-1".to_owned(),
        params: json!({}),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("execute");
    assert_eq!(
        result.content[0],
        pir_ai::types::ToolResultContent::Text(pir_ai::types::TextContent {
            text: "wasm-output".to_owned(),
            text_signature: None
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_capability_denied_for_bare_guest_and_allowed_by_manifest() {
    let tmp = TempDir::new("caps");
    // 裸 .wasm（无 manifest）→ capabilities=[] → registerTool 被拒。
    let bare = tmp.write_guest("bare", PROBE_GUEST_WAT, None);
    let (_host, errors) = host_loading(&[bare]).await;
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("capabilityDenied"),
        "bare guest must be denied: {errors:?}"
    );

    // manifest 授予 tools → 同一 guest 加载成功。
    let granted = tmp.write_guest(
        "granted",
        PROBE_GUEST_WAT,
        Some(r#"{"name":"probe","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"pirAbi":1}"#),
    );
    let (host, errors) = host_loading(&[granted]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.get_tool_definition("probe_tool").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_fuel_stops_runaway_guest_init() {
    let tmp = TempDir::new("fuel");
    let wasm = tmp.write_guest("loopy", LOOP_GUEST_WAT, None);
    let (_host, errors) =
        tokio::time::timeout(std::time::Duration::from_secs(30), host_loading(&[wasm]))
            .await
            .expect("fuel exhaustion must not hang");
    assert_eq!(
        errors.len(),
        1,
        "fuel trap surfaces as load error: {errors:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_missing_export_is_a_load_error() {
    let tmp = TempDir::new("exports");
    let wasm = tmp.write_guest("nodispatch", NO_DISPATCH_GUEST_WAT, None);
    let (_host, errors) = host_loading(&[wasm]).await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("pir_dispatch"), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_abi_version_mismatch_is_rejected() {
    let tmp = TempDir::new("abi");
    let wasm = tmp.write_guest(
        "old",
        TOOL_GUEST_WAT,
        Some(r#"{"name":"old","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"pirAbi":99}"#),
    );
    let (_host, errors) = host_loading(&[wasm]).await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("pirAbi"), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_module_cache_reused_within_generation() {
    // 同一代 (cwd+generation) 下二次加载命中缓存的 factory；都能跑通即
    // 为证据（编译只发生一次——由 FactoryCache 语义保证，W1 单测锚定）。
    let tmp = TempDir::new("cache");
    let wasm = tmp.write_guest(
        "cached",
        TOOL_GUEST_WAT,
        Some(r#"{"name":"cached","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"pirAbi":1}"#),
    );
    let host = NativeExtensionHost::new("/w6-cwd");
    let first = host.load_paths(std::slice::from_ref(&wasm)).await;
    assert!(first.is_empty(), "{first:?}");
    let second = host.load_paths(&[wasm]).await;
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(host.get_extension_paths().len(), 2);
}

// ============================================================================
// 阻塞渲染往返（TUI 线程 dispatch_blocking）超时回归（审查修复）
// ============================================================================

/// 阻塞型 guest：每次 dispatch 都调 host_call("exec") 并把宿主响应原样
/// 返回——宿主 exec 未决时 guest 线程阻塞在 host call 上（对应真实场景：
/// guest 在 ui.select 对话框 host call 里等待 TUI 输入）。
const BLOCKING_EXEC_GUEST_WAT: &str = r#"
(module
  (import "pir" "pir_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "pir_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "pir_dealloc") (param i32 i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (global $first (mut i32) (i32.const 1))
  (func (export "pir_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "pir_dispatch") (param i32 i32) (result i64)
    ;; 仅第一次 dispatch 调 exec（挂起）；之后的 dispatch 返回 null——
    ;; 超时后残留在队列里的 dispatch 恢复执行时不再阻塞。
    (if (i32.eqz (global.get $first))
      (then (return (call $pack (i32.const 128)))))
    (global.set $first (i32.const 0))
    ;; 原样转发 exec host call 的响应（宿主在 guest 内存里分配的
    ;; (ptr<<32)|len 就是 dispatch 的返回格式）。
    (return (call $host_call (i32.const 64) (call $strlen (i32.const 64)))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 64) "{\"call\":\"exec\",\"args\":{\"command\":\"sleep\",\"args\":[]}}\00")
  (data (i32.const 128) "null\00")
)
"#;

/// Stub actions：exec 挂起直到 Notify 放行（模拟未决对话框/慢宿主动作）。
struct BlockingExecActions {
    notify: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl pir_ext_host::api::HostActions for BlockingExecActions {
    fn send_message(
        &self,
        _message: serde_json::Value,
        _options: Option<pir_ext_host::api::SendMessageOptions>,
    ) {
    }
    fn send_user_message(
        &self,
        _content: serde_json::Value,
        _options: Option<pir_ext_host::api::SendUserMessageOptions>,
    ) {
    }
    fn append_entry(&self, _custom_type: &str, _data: Option<serde_json::Value>) {}
    fn set_session_name(&self, _name: &str) {}
    fn get_session_name(&self) -> Option<String> {
        None
    }
    fn set_label(&self, _entry_id: &str, _label: Option<&str>) {}
    async fn exec(
        &self,
        _command: &str,
        _args: &[String],
        _options: Option<pir_ext_host::api::ExecOptions>,
    ) -> Result<pir_ext_host::api::ExecResult, pir_ext_host::ExtError> {
        self.notify.notified().await;
        Ok(pir_ext_host::api::ExecResult::default())
    }
    fn get_active_tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn get_all_tools(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    fn set_active_tools(&self, _tool_names: Vec<String>) {}
    fn refresh_tools(&self) {}
    fn get_commands(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn set_model(&self, _model: serde_json::Value) -> bool {
        false
    }
    fn get_thinking_level(&self) -> String {
        "off".to_owned()
    }
    fn set_thinking_level(&self, _level: &str) {}
    async fn register_provider(
        &self,
        _name: &str,
        _config: serde_json::Value,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn register_native_provider(
        &self,
        _provider: std::sync::Arc<dyn pir_ai::models::Provider>,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn unregister_provider(&self, _name: &str) {}
}

/// guest 阻塞在宿主动作（对话框等）上时，TUI 线程的渲染往返
/// `dispatch_blocking` 必须超时失败（回退默认渲染）而非永久卡死。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_blocking_render_roundtrip_times_out_when_guest_busy() {
    use std::sync::Arc;

    let tmp = TempDir::new("busy");
    let wasm = tmp.write_guest(
        "busy",
        BLOCKING_EXEC_GUEST_WAT,
        Some(r#"{"name":"busy","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["session","exec"],"pirAbi":1}"#),
    );
    let host = std::sync::Arc::new(NativeExtensionHost::new("/w6-busy-cwd"));
    let errors = host.load_paths(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let notify = Arc::new(tokio::sync::Notify::new());
    host.bind_actions(Arc::new(BlockingExecActions {
        notify: notify.clone(),
    }))
    .await;

    // 1. 触发一次事件分发：guest 进入 dispatch handler → host_call("exec")
    //    → 阻塞在未决的 exec 上（模拟对话框挂起）。
    let emit_host = host.clone();
    let emit_task = tokio::spawn(async move {
        emit_host
            .emit("tool_call", json!({ "type": "tool_call" }))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. TUI 线程的阻塞渲染往返：必须超时失败而不是死锁。
    let extension = host.core().extensions()[0].clone();
    let forward = extension.wasm_forward().expect("wasm guest attached");
    let started = std::time::Instant::now();
    let result = forward.dispatch_blocking(
        json!({"kind": "event", "event": "tool_call", "payload": {}}),
        false,
    );
    assert!(result.is_err(), "busy guest must time out, got {result:?}");
    assert!(result.unwrap_err().contains("timed out"));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(1) && elapsed < std::time::Duration::from_secs(5),
        "timed out in ~2s, took {elapsed:?}"
    );

    // 3. 释放宿主动作：guest 恢复，挂起的 emit 完成，串行队列继续工作。
    // （第一个 notify 被挂起的 emit 消费；第二次放行给恢复后的 dispatch。）
    notify.notify_one();
    emit_task.await.expect("emit completes after release");
    notify.notify_one();
    let resumed = forward.dispatch_blocking(
        json!({"kind": "event", "event": "tool_call", "payload": {}}),
        false,
    );
    if let Err(ref e) = resumed {
        eprintln!("resumed error: {e}");
    }
    assert!(resumed.is_ok(), "guest resumes after the action resolves");
}
