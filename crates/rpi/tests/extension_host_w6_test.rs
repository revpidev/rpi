//! T15 W6 e2e + dual-backend parity: wasm extensions and native inline
//! extensions behave identically on the same capability surface (the
//! permission-gate scenario: tool_call block + custom-tool registration and
//! execution + context queries).
//!
//! The wasm fixtures are embedded WAT (compiled directly by wasmtime); no
//! wasm32 toolchain needed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-ext-w6e2e-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// gate guest（on tool_call → block reason "gate-block"）+ tool guest
/// （注册 wasm_tool 执行返回 "gate-output"）合并为一个双能力 guest。
const PARITY_GUEST_WAT: &str = r#"
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
  ;; 子串匹配（定长 needle，无需 NUL）。
  (func $contains (param $hay i32) (param $haylen i32) (param $needle i32) (param $needlelen i32) (result i32)
    (local $i i32) (local $j i32) (local $match i32)
    (block $outer_done
      (loop $outer
        (br_if $outer_done (i32.gt_u (i32.add (local.get $i) (local.get $needlelen)) (local.get $haylen)))
        (local.set $j (i32.const 0))
        (local.set $match (i32.const 1))
        (block $inner_done
          (loop $inner
            (br_if $inner_done (i32.ge_u (local.get $j) (local.get $needlelen)))
            (if (i32.ne
                  (i32.load8_u (i32.add (i32.add (local.get $hay) (local.get $i)) (local.get $j)))
                  (i32.load8_u (i32.add (local.get $needle) (local.get $j))))
              (then (local.set $match (i32.const 0)) (br $inner_done)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $inner)))
        (if (local.get $match) (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (drop (call $host_call (i32.const 256) (call $strlen (i32.const 256))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param $ptr i32) (param $len i32) (result i64)
    ;; 针对 "read" 的 tool_call → block；toolExecute → 工具结果；其他 → null。
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 1536) (i32.const 4))
      (then (return (call $pack (i32.const 768)))))
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 1544) (i32.const 11))
      (then (return (call $pack (i32.const 1024)))))
    (return (call $pack (i32.const 1560))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 256) "{\"call\":\"registerTool\",\"args\":{\"name\":\"gate_tool\",\"label\":\"Gate Tool\",\"description\":\"parity tool\",\"parameters\":{\"type\":\"object\"}}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 768) "{\"block\":true,\"reason\":\"gate-block\"}\00")
  (data (i32.const 1024) "{\"content\":[{\"type\":\"text\",\"text\":\"gate-output\"}],\"details\":null}\00")
  (data (i32.const 1536) "read")
  (data (i32.const 1544) "toolExecute")
  (data (i32.const 1560) "null\00")
)
"#;

struct E2eFixture {
    session: rpi::core::agent_session::AgentSession,
    _tmp: TempDir,
}

/// 完整会话管线（faux provider + 真宿主），wasm 包放在
/// `<cwd>/.rpi/extensions/parity/`。
async fn e2e_fixture(responses: Vec<FauxResponseStep>, wasm_wat: Option<&str>) -> E2eFixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    if let Some(wat) = wasm_wat {
        let pkg = cwd.join(".rpi/extensions/parity");
        std::fs::create_dir_all(pkg.join("dist")).expect("pkg dist");
        std::fs::write(pkg.join("dist/guest.wasm"), wat).expect("guest");
        std::fs::write(
            pkg.join("rpi-extension.json"),
            r#"{"name":"parity","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["tools","session"],"rpiAbi":1}"#,
        )
        .expect("manifest");
    }
    let errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            false,
        )
        .await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");
    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux");
    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");
    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(Arc::new(host)),
        ..Default::default()
    })
    .await
    .expect("create session");
    E2eFixture {
        session: created.session,
        _tmp: tmp,
    }
}

fn tool_call_step(name: &str) -> FauxResponseStep {
    let args = if name == "read" {
        json!({"path": "x"})
    } else {
        json!({})
    };
    faux_assistant_message(
        faux_tool_call(name, args.as_object().cloned().unwrap_or_default(), None),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

fn first_tool_result(session: &rpi::core::agent_session::AgentSession) -> Value {
    session
        .messages()
        .into_iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .find(|v| v["role"] == "toolResult")
        .expect("toolResult message")
}

/// 对拍场景：先调 gate_tool（执行），再调 read（被 gate block）。
async fn run_parity_scenario(fixture: &E2eFixture) -> (String, String) {
    fixture
        .session
        .prompt("go", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;
    let result = first_tool_result(&fixture.session);
    (
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        result["isError"].to_string(),
    )
}

// ---------------------------------------------------------------------------
// e2e + 对拍
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w6_wasm_tool_executes_and_gate_blocks_in_agent_loop() {
    // gate_tool 执行（toolExecute 分发）。
    let fixture = e2e_fixture(
        vec![tool_call_step("gate_tool"), text_step("done")],
        Some(PARITY_GUEST_WAT),
    )
    .await;
    let (content, is_error) = run_parity_scenario(&fixture).await;
    assert_eq!(content, "gate-output");
    assert_eq!(is_error, "false");

    // read 被 gate block。
    let fixture = e2e_fixture(
        vec![tool_call_step("read"), text_step("done")],
        Some(PARITY_GUEST_WAT),
    )
    .await;
    let (content, is_error) = run_parity_scenario(&fixture).await;
    assert!(content.contains("gate-block"), "content: {content}");
    assert_eq!(is_error, "true");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w6_native_and_wasm_gate_behave_identically() {
    // native 版 gate：同 reason 的 block + 同名工具同输出。
    let native_ext = InlineExtension::Anonymous(Arc::new(|api| {
        api.on(
            "tool_call",
            Arc::new(|payload, _ctx| {
                Box::pin(async move {
                    if payload["toolName"] == "read" {
                        Ok(json!({"block": true, "reason": "gate-block"}))
                    } else {
                        Ok(Value::Null)
                    }
                })
            }),
        )
        .expect("on");
        api.register_tool(rpi_ext_host::types::ToolDefinition {
            name: "gate_tool".to_owned(),
            label: "Gate Tool".to_owned(),
            description: "parity tool".to_owned(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters: json!({"type": "object"}),
            constrained_sampling: None,
            render_shell: None,
            prepare_arguments: None,
            execution_mode: None,
            execute: Arc::new(|_req, _ctx| {
                Box::pin(async {
                    Ok(rpi_agent::types::AgentToolResult {
                        content: vec![rpi_ai::types::ToolResultContent::Text(
                            rpi_ai::types::TextContent {
                                text: "gate-output".to_owned(),
                                text_signature: None,
                            },
                        )],
                        ..Default::default()
                    })
                })
            }),
            render_call: None,
            render_result: None,
        })
        .expect("tool");
        Box::pin(async { Ok(()) })
    }));

    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host.load_inline(&[native_ext]).await;
    assert!(errors.is_empty());

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(vec![tool_call_step("gate_tool"), text_step("done")]);
    let model = provider.get_model(None).expect("faux model");
    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux");
    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");
    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(Arc::new(host)),
        ..Default::default()
    })
    .await
    .expect("create session");
    let native = E2eFixture {
        session: created.session,
        _tmp: tmp,
    };
    let native_outcome = run_parity_scenario(&native).await;

    let wasm = e2e_fixture(
        vec![tool_call_step("gate_tool"), text_step("done")],
        Some(PARITY_GUEST_WAT),
    )
    .await;
    let wasm_outcome = run_parity_scenario(&wasm).await;

    assert_eq!(native_outcome, wasm_outcome, "native vs wasm parity");
    assert_eq!(native_outcome.0, "gate-output");
}
