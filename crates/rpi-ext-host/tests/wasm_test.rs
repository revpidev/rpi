//! T15 W6 host-level tests (WAT fixtures — no wasm32 toolchain needed:
//! wasmtime compiles the WAT text directly).
//!
//! ABI v1 coverage: init registration/receipt, serial dispatch, toolExecute
//! forwarding, capability denial (bare .wasm vs manifest), fuel exhaustion,
//! missing exports, cache reuse.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rpi_ext_host::host::NativeExtensionHost;
use serde_json::json;

/// tool_call gate guest: on("tool_call"), dispatch always returns block.
const GATE_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
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
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"block\":true,\"reason\":\"wasm-gate\"}\00")
)
"#;

/// Tool guest: registerTool("wasm_tool"), toolExecute always returns a fixed result.
const TOOL_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
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
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"registerTool\",\"args\":{\"name\":\"wasm_tool\",\"label\":\"Wasm Tool\",\"description\":\"wasm tool\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"content\":[{\"type\":\"text\",\"text\":\"wasm-output\"}],\"details\":null}\00")
)
"#;

/// Capability probe guest: init calls registerTool and returns the response verbatim as
/// the init receipt.
const PROBE_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func (export "rpi_extension_init") (result i64)
    ;; forwards the host_call response directly as the init receipt.
    (return (call $host_call (i32.const 16) (call $strlen (i32.const 16)))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
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

/// Infinite-loop guest: init never returns; fuel exhaustion should trap into a load error.
const LOOP_GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "rpi_alloc") (param i32) (result i32) (i32.const 64))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func (export "rpi_extension_init") (result i64)
    (loop $spin (br $spin))
    (i64.const 0))
  (func (export "rpi_dispatch") (param i32 i32) (result i64) (i64.const 0))
)
"#;

/// Guest missing the rpi_dispatch export.
const NO_DISPATCH_GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "rpi_alloc") (param i32) (result i32) (i32.const 64))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func (export "rpi_extension_init") (result i64) (i64.const 0))
)
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-ext-w6-{tag}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// Writes a guest package directory: dist/*.wat (with a .wasm extension) + optional
    /// manifest.
    fn write_guest(&self, dir_name: &str, wat: &str, manifest: Option<&str>) -> PathBuf {
        let dir = self.0.join(dir_name);
        std::fs::create_dir_all(dir.join("dist")).expect("dist");
        let wasm = dir.join("dist/guest.wasm");
        std::fs::write(&wasm, wat).expect("write guest");
        if let Some(manifest) = manifest {
            std::fs::write(dir.join("rpi-extension.json"), manifest).expect("write manifest");
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
        Some(r#"{"name":"gate","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":[],"rpiAbi":1}"#),
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
        Some(r#"{"name":"tool","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let definition = host.get_tool_definition("wasm_tool").expect("registered");
    assert_eq!(definition.description, "wasm tool");
    let request = rpi_ext_host::types::ToolExecuteRequest {
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
        rpi_ai::types::ToolResultContent::Text(rpi_ai::types::TextContent {
            text: "wasm-output".to_owned(),
            text_signature: None
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_capability_denied_for_bare_guest_and_allowed_by_manifest() {
    let tmp = TempDir::new("caps");
    // Bare .wasm (no manifest) → capabilities=[] → registerTool is rejected.
    let bare = tmp.write_guest("bare", PROBE_GUEST_WAT, None);
    let (_host, errors) = host_loading(&[bare]).await;
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("capabilityDenied"),
        "bare guest must be denied: {errors:?}"
    );

    // Manifest grants tools → the same guest loads successfully.
    let granted = tmp.write_guest(
        "granted",
        PROBE_GUEST_WAT,
        Some(r#"{"name":"probe","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
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
    assert!(errors[0].contains("rpi_dispatch"), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_abi_version_mismatch_is_rejected() {
    let tmp = TempDir::new("abi");
    let wasm = tmp.write_guest(
        "old",
        TOOL_GUEST_WAT,
        Some(r#"{"name":"old","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":99}"#),
    );
    let (_host, errors) = host_loading(&[wasm]).await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("rpiAbi"), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_module_cache_reused_within_generation() {
    // A second load under the same generation (cwd+generation) hits the cached factory;
    // both succeeding is the evidence (compilation happens exactly once — guaranteed by
    // FactoryCache semantics, anchored by W1 unit tests).
    let tmp = TempDir::new("cache");
    let wasm = tmp.write_guest(
        "cached",
        TOOL_GUEST_WAT,
        Some(r#"{"name":"cached","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
    );
    let host = NativeExtensionHost::new("/w6-cwd");
    let first = host.load_paths(std::slice::from_ref(&wasm)).await;
    assert!(first.is_empty(), "{first:?}");
    let second = host.load_paths(&[wasm]).await;
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(host.get_extension_paths().len(), 2);
}

// ============================================================================
// Blocking render round-trip (TUI-thread dispatch_blocking) timeout regression (review fix)
// ============================================================================

/// Blocking guest: every dispatch calls host_call("exec") and returns the host
/// response verbatim — while the host exec is pending, the guest thread blocks on the
/// host call (mirrors the real scenario: a guest waiting on TUI input inside a
/// ui.select dialog host call).
const BLOCKING_EXEC_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
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
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    ;; only the first dispatch calls exec (hangs); later dispatches return null —
    ;; dispatches left in the queue after the timeout resume without blocking.
    (if (i32.eqz (global.get $first))
      (then (return (call $pack (i32.const 128)))))
    (global.set $first (i32.const 0))
    ;; forwards the exec host call response verbatim (the (ptr<<32)|len the host
    ;; allocates in guest memory is the dispatch return format).
    (return (call $host_call (i32.const 64) (call $strlen (i32.const 64)))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 64) "{\"call\":\"exec\",\"args\":{\"command\":\"sleep\",\"args\":[]}}\00")
  (data (i32.const 128) "null\00")
)
"#;

/// Stub actions: exec stays pending until Notify releases it (mimics a pending dialog /
/// slow host action).
struct BlockingExecActions {
    notify: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl rpi_ext_host::api::HostActions for BlockingExecActions {
    fn send_message(
        &self,
        _message: serde_json::Value,
        _options: Option<rpi_ext_host::api::SendMessageOptions>,
    ) {
    }
    fn send_user_message(
        &self,
        _content: serde_json::Value,
        _options: Option<rpi_ext_host::api::SendUserMessageOptions>,
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
        _options: Option<rpi_ext_host::api::ExecOptions>,
    ) -> Result<rpi_ext_host::api::ExecResult, rpi_ext_host::ExtError> {
        self.notify.notified().await;
        Ok(rpi_ext_host::api::ExecResult::default())
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
        _provider: std::sync::Arc<dyn rpi_ai::models::Provider>,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn unregister_provider(&self, _name: &str) {}

    async fn model_registry_complete(
        &self,
        _model: serde_json::Value,
        _context: serde_json::Value,
        _options: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        None
    }

    fn model_registry_find(&self, _provider: &str, _model_id: &str) -> Option<serde_json::Value> {
        None
    }

    fn model_registry_has_configured_auth(&self, _provider_id: &str) -> bool {
        false
    }

    async fn get_api_key_and_headers(&self, _model: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"ok": false, "error": "not configured"})
    }

    async fn set_runtime_api_key(
        &self,
        _provider_id: &str,
        _api_key: &str,
        _options: Option<rpi_ext_host::types::AuthOperationOptions>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn remove_runtime_api_key(&self, _provider_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// When a guest blocks on a host action (dialog, etc.), the TUI thread's render
/// round-trip `dispatch_blocking` must time out and fail (falling back to default
/// rendering) instead of hanging forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_blocking_render_roundtrip_times_out_when_guest_busy() {
    use std::sync::Arc;

    let tmp = TempDir::new("busy");
    let wasm = tmp.write_guest(
        "busy",
        BLOCKING_EXEC_GUEST_WAT,
        Some(r#"{"name":"busy","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["session","exec"],"rpiAbi":1}"#),
    );
    let host = std::sync::Arc::new(NativeExtensionHost::new("/w6-busy-cwd"));
    let errors = host.load_paths(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let notify = Arc::new(tokio::sync::Notify::new());
    host.bind_actions(Arc::new(BlockingExecActions {
        notify: notify.clone(),
    }))
    .await;

    // 1. Trigger one event dispatch: the guest enters its dispatch handler →
    //    host_call("exec") → blocks on the pending exec (mimics a hung dialog).
    let emit_host = host.clone();
    let emit_task = tokio::spawn(async move {
        emit_host
            .emit("tool_call", json!({ "type": "tool_call" }))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. The TUI thread's blocking render round-trip: must time out and fail rather
    //    than deadlock.
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

    // 3. Release the host action: the guest resumes, the pending emit completes, and
    //    the serial queue keeps working. (The first notify is consumed by the pending
    //    emit; the second release goes to the resumed dispatch.)
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

// ============================================================================
// T27.1: markdown transformer non-string return preserves content
// (markdown-transform.ts:19-23)
// ============================================================================

/// Guest that registers a markdown transformer on init. For every dispatch
/// (including `markdownTransform`), returns `{"ok":true}` — a non-string
/// JSON value. The host's transformer closure must preserve the original
/// markdown, not replace it with an empty string.
const MD_TRANSFORM_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
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
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"registerMarkdownTransformer\",\"args\":{}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"ok\":true}\00")
)
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t271_markdown_transformer_non_string_return_preserves_content() {
    let tmp = TempDir::new("md-transform");
    let wasm = tmp.write_guest(
        "md-transform",
        MD_TRANSFORM_GUEST_WAT,
        Some(r#"{"name":"md-transform","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["ui"],"rpiAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let extension = host.core().extensions()[0].clone();
    let transformer = extension
        .markdown_transformer()
        .expect("markdown transformer registered");

    // The guest returns {"ok":true} for every dispatch — a non-string value.
    // markdown-transform.ts:19-23: `if (typeof transformed === "string")`
    // must skip the assignment and preserve the input.
    let input = "# Hello World\n\nThis is original content.".to_owned();
    let result = transformer(input.clone(), Default::default());
    assert_eq!(
        result, input,
        "non-string return must preserve original markdown (was clearing to empty string)"
    );
}

// ============================================================================
// TE01 / ADR-0015: unregisterTool + toolUpdate (additive ABI v1 methods)
// ============================================================================

/// Unregister-cycle guest: registers `wasm_cycle`, then unregisterTool →
/// repeat unregister → unknown unregister, turning each boolean outcome into
/// an observable evidence registration (WAT cannot compare JSON text, so it
/// compares the response byte length: `{"ok":true}` = 11, `{"ok":false}` =
/// 12), and finally re-registers `wasm_cycle`.
const UNREGISTER_CYCLE_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $call (param $ptr i32) (result i64)
    (call $host_call (local.get $ptr) (call $strlen (local.get $ptr))))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $call (i32.const 16)))
    ;; first unregister: {"ok":true} (11 bytes) -> evidence_true
    (if (i32.eq (i32.wrap_i64 (call $call (i32.const 512))) (i32.const 11))
      (then (drop (call $call (i32.const 768)))))
    ;; repeat unregister: {"ok":false} (12 bytes) -> evidence_repeat_false
    (if (i32.eq (i32.wrap_i64 (call $call (i32.const 1024))) (i32.const 12))
      (then (drop (call $call (i32.const 1280)))))
    ;; unknown unregister: {"ok":false} (12 bytes) -> evidence_unknown_false
    (if (i32.eq (i32.wrap_i64 (call $call (i32.const 1536))) (i32.const 12))
      (then (drop (call $call (i32.const 1792)))))
    ;; re-register the tool (full cycle)
    (drop (call $call (i32.const 2048)))
    (return (call $pack (i32.const 2304))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 2304))))
  (data (i32.const 16) "{\"call\":\"registerTool\",\"args\":{\"name\":\"wasm_cycle\",\"description\":\"c\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 512) "{\"call\":\"unregisterTool\",\"args\":{\"name\":\"wasm_cycle\"}}\00")
  (data (i32.const 768) "{\"call\":\"registerTool\",\"args\":{\"name\":\"evidence_true\",\"description\":\"e\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 1024) "{\"call\":\"unregisterTool\",\"args\":{\"name\":\"wasm_cycle\"}}\00")
  (data (i32.const 1280) "{\"call\":\"registerTool\",\"args\":{\"name\":\"evidence_repeat_false\",\"description\":\"e\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 1536) "{\"call\":\"unregisterTool\",\"args\":{\"name\":\"wasm_never\"}}\00")
  (data (i32.const 1792) "{\"call\":\"registerTool\",\"args\":{\"name\":\"evidence_unknown_false\",\"description\":\"e\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 2048) "{\"call\":\"registerTool\",\"args\":{\"name\":\"wasm_cycle\",\"description\":\"c\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 2304) "{\"ok\":true}\00")
)
"#;

/// Updater guest: registers `wasm_updater`; every dispatch reports one
/// partial result via `toolUpdate` (hardcoded toolCallId `tc-update` — the
/// host test drives the execute with exactly that id) and returns the final
/// result.
const UPDATER_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $call (param $ptr i32) (result i64)
    (call $host_call (local.get $ptr) (call $strlen (local.get $ptr))))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $call (i32.const 16)))
    (return (call $pack (i32.const 1024))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (drop (call $call (i32.const 512)))
    (return (call $pack (i32.const 1536))))
  (data (i32.const 16) "{\"call\":\"registerTool\",\"args\":{\"name\":\"wasm_updater\",\"description\":\"u\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 512) "{\"call\":\"toolUpdate\",\"args\":{\"toolCallId\":\"tc-update\",\"update\":{\"content\":[{\"type\":\"text\",\"text\":\"wasm-partial\"}],\"details\":null}}}\00")
  (data (i32.const 1024) "{\"ok\":true}\00")
  (data (i32.const 1536) "{\"content\":[{\"type\":\"text\",\"text\":\"wasm-final\"}],\"details\":null}\00")
)
"#;

/// Builds a probe guest whose init forwards the given host call's response
/// verbatim as the init receipt (capability-denial probing). `call_json` is
/// plain JSON; quotes/backslashes are escaped for the WAT string literal.
fn te01_probe_wat(call_json: &str) -> String {
    let escaped = call_json.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func (export "rpi_extension_init") (result i64)
    (return (call $host_call (i32.const 16) (call $strlen (i32.const 16)))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (i64.const 0)))
  (data (i32.const 16) "{escaped}\00")
)
"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_unregister_tool_full_cycle_and_boundaries() {
    let tmp = TempDir::new("unreg");
    let wasm = tmp.write_guest(
        "unreg",
        UNREGISTER_CYCLE_GUEST_WAT,
        Some(r#"{"name":"unreg","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    // First unregister returned true, repeat and unknown returned false
    // (evidence tools registered by the guest's length comparisons).
    assert!(host.get_tool_definition("evidence_true").is_some());
    assert!(host.get_tool_definition("evidence_repeat_false").is_some());
    assert!(host.get_tool_definition("evidence_unknown_false").is_some());
    // Re-registration after unregister works (full cycle).
    assert!(host.get_tool_definition("wasm_cycle").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_unregister_tool_and_tool_update_capability_denied_for_bare_guest() {
    let tmp = TempDir::new("te01-caps");
    for (dir, call) in [
        (
            "bare-unregister",
            r#"{"call":"unregisterTool","args":{"name":"x"}}"#,
        ),
        (
            "bare-toolupdate",
            r#"{"call":"toolUpdate","args":{"toolCallId":"x","update":{"content":[],"details":null}}}"#,
        ),
    ] {
        let wasm = tmp.write_guest(dir, &te01_probe_wat(call), None);
        let (_host, errors) = host_loading(&[wasm]).await;
        assert_eq!(errors.len(), 1, "{dir}: {errors:?}");
        assert!(
            errors[0].contains("capabilityDenied"),
            "{dir} must be denied: {errors:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_tool_update_outside_execute_is_dropped_with_ok() {
    let tmp = TempDir::new("te01-late");
    // Granted capabilities: toolUpdate with an unknown toolCallId (no
    // in-flight execution) must be dropped but answered {"ok": null} — the
    // forwarded receipt loads cleanly.
    let wasm = tmp.write_guest(
        "late",
        &te01_probe_wat(
            r#"{"call":"toolUpdate","args":{"toolCallId":"never-started","update":{"content":[],"details":null}}}"#,
        ),
        Some(r#"{"name":"late","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
    );
    let (_host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_tool_execute_streams_on_update_to_host() {
    let tmp = TempDir::new("te01-update");
    let wasm = tmp.write_guest(
        "updater",
        UPDATER_GUEST_WAT,
        Some(r#"{"name":"updater","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools"],"rpiAbi":1}"#),
    );
    let (host, errors) = host_loading(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");

    let updates: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = updates.clone();
    let on_update: rpi_agent::types::AgentToolUpdateCallback =
        Box::new(move |result: rpi_agent::types::AgentToolResult| {
            if let Some(rpi_ai::types::ToolResultContent::Text(text)) = result.content.first() {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(text.text.clone());
            }
        });

    let definition = host
        .get_tool_definition("wasm_updater")
        .expect("registered");
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "tc-update".to_owned(),
        params: json!({}),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: Some(on_update),
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("execute");

    assert_eq!(
        updates.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        &["wasm-partial".to_owned()],
        "partial result must reach the host on_update sink"
    );
    assert_eq!(
        result.content[0],
        rpi_ai::types::ToolResultContent::Text(rpi_ai::types::TextContent {
            text: "wasm-final".to_owned(),
            text_signature: None
        })
    );
}
