//! T15 W4 tests: RpcUiBridge protocol contract (9 dialog methods) + the 18
//! degraded methods, ComponentTree v1 mapping, and the renderer pipeline
//! (message-render end to end + tool render-slot inheritance).
//!
//! Upstream anchors: rpc-mode.ts:88-309 (createDialogPromise +
//! createExtensionUIContext), docs/rpc.md:1143-1333 (wire format).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::modes::rpc::ui_bridge::{new_pending_ui_table, PendingUiTable, RpcUiBridge};
use pir_ext_host::api::{NotifyType, UiBridge, UiDialogOptions, WidgetContent};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// 桥 + 帧接收端 + pending 表。
struct RpcRig {
    bridge: Arc<RpcUiBridge>,
    rx: mpsc::UnboundedReceiver<String>,
    pending: PendingUiTable,
}

fn rpc_rig() -> RpcRig {
    let (tx, rx) = mpsc::unbounded_channel();
    let pending = new_pending_ui_table();
    let bridge = Arc::new(RpcUiBridge::new(
        tx,
        pending.clone(),
        pir::core::themes::default_theme_json(),
    ));
    RpcRig {
        bridge,
        rx,
        pending,
    }
}

impl RpcRig {
    /// 下一帧（解析为 JSON；带超时的异步等待）。
    async fn next_frame(&mut self) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), self.rx.recv())
            .await
            .expect("frame within timeout")
            .expect("channel open");
        serde_json::from_str(line.trim_end()).expect("json frame")
    }

    fn assert_no_frame(&mut self) {
        assert!(self.rx.try_recv().is_err(), "unexpected frame");
    }

    /// 客户端应答：经 pending 表 resolve（stdin 路由的测试替身）。
    fn respond(&self, id: &str, response: Value) {
        let tx = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .expect("pending entry");
        tx.send(response).expect("resolve");
    }
}

// ---------------------------------------------------------------------------
// Dialog 方法（rpc-mode.ts:90-160 + docs/rpc.md）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w4_rpc_select_frame_shape_and_value_response() {
    let mut rig = rpc_rig();
    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move {
        bridge
            .select(
                "Allow dangerous command?",
                &["Allow".to_owned(), "Block".to_owned()],
                None,
            )
            .await
    });
    let frame = rig.next_frame().await;
    assert_eq!(frame["type"], "extension_ui_request");
    assert_eq!(frame["method"], "select");
    assert_eq!(frame["title"], "Allow dangerous command?");
    assert_eq!(frame["options"], json!(["Allow", "Block"]));
    assert!(frame.get("timeout").is_none());
    rig.respond(frame["id"].as_str().unwrap(), json!({"value": "Allow"}));
    assert_eq!(task.await.unwrap(), Some("Allow".to_owned()));
}

#[tokio::test]
async fn w4_rpc_select_cancelled_maps_to_none() {
    let mut rig = rpc_rig();
    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move { bridge.select("t", &["a".to_owned()], None).await });
    let frame = rig.next_frame().await;
    rig.respond(frame["id"].as_str().unwrap(), json!({"cancelled": true}));
    assert_eq!(task.await.unwrap(), None);
}

#[tokio::test]
async fn w4_rpc_confirm_frame_and_response_mapping() {
    let mut rig = rpc_rig();
    let bridge = rig.bridge.clone();
    let task =
        tokio::spawn(async move { bridge.confirm("Clear session?", "All lost.", None).await });
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "confirm");
    assert_eq!(frame["message"], "All lost.");
    rig.respond(frame["id"].as_str().unwrap(), json!({"confirmed": true}));
    assert!(task.await.unwrap());

    // cancelled → false
    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move { bridge.confirm("t", "m", None).await });
    let frame = rig.next_frame().await;
    rig.respond(frame["id"].as_str().unwrap(), json!({"cancelled": true}));
    assert!(!task.await.unwrap());
}

#[tokio::test]
async fn w4_rpc_input_and_editor_frames() {
    let mut rig = rpc_rig();
    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move { bridge.input("Enter", Some("type..."), None).await });
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "input");
    assert_eq!(frame["placeholder"], "type...");
    rig.respond(frame["id"].as_str().unwrap(), json!({"value": "hello"}));
    assert_eq!(task.await.unwrap(), Some("hello".to_owned()));

    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move { bridge.editor("Edit", Some("Line 1\nLine 2")).await });
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "editor");
    assert_eq!(frame["prefill"], "Line 1\nLine 2");
    // editor 不带 timeout 字段（rpc-mode.ts:238-262）。
    assert!(frame.get("timeout").is_none());
    rig.respond(frame["id"].as_str().unwrap(), json!({"value": "edited"}));
    assert_eq!(task.await.unwrap(), Some("edited".to_owned()));
}

#[tokio::test]
async fn w4_rpc_dialog_timeout_auto_resolves_default() {
    let mut rig = rpc_rig();
    let frame_id = Arc::new(Mutex::new(String::new()));
    let bridge = rig.bridge.clone();
    let id_sink = frame_id.clone();
    let task = tokio::spawn(async move {
        bridge
            .select(
                "t",
                &["a".to_owned()],
                Some(UiDialogOptions { timeout: Some(40) }),
            )
            .await
    });
    let frame = rig.next_frame().await;
    assert_eq!(frame["timeout"], 40);
    *id_sink.lock().unwrap() = frame["id"].as_str().unwrap().to_owned();
    // 不应答 → 超时后 select → None。
    assert_eq!(task.await.unwrap(), None);
    // pending 条目已清理（rpc-mode.ts:101-108）。
    assert!(rig.pending.lock().unwrap().is_empty());

    // confirm 超时 → false。
    let bridge = rig.bridge.clone();
    let task = tokio::spawn(async move {
        bridge
            .confirm("t", "m", Some(UiDialogOptions { timeout: Some(40) }))
            .await
    });
    let _ = rig.next_frame().await;
    assert!(!task.await.unwrap());
}

// ---------------------------------------------------------------------------
// Fire-and-forget 5 方法
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w4_rpc_fire_and_forget_frames() {
    let mut rig = rpc_rig();
    rig.bridge.notify("blocked", NotifyType::Warning);
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "notify");
    assert_eq!(frame["message"], "blocked");
    assert_eq!(frame["notifyType"], "warning");

    rig.bridge.set_status("my-ext", Some("Turn 3"));
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "setStatus");
    assert_eq!(frame["statusKey"], "my-ext");
    assert_eq!(frame["statusText"], "Turn 3");

    rig.bridge.set_status("my-ext", None);
    let frame = rig.next_frame().await;
    assert!(frame["statusText"].is_null());

    rig.bridge.set_widget(
        "w",
        Some(WidgetContent::Lines(vec!["l1".to_owned(), "l2".to_owned()])),
        Some(pir_ext_host::api::ExtensionWidgetOptions {
            placement: Some(pir_ext_host::api::WidgetPlacement::BelowEditor),
        }),
    );
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "setWidget");
    assert_eq!(frame["widgetLines"], json!(["l1", "l2"]));
    assert_eq!(frame["widgetPlacement"], "belowEditor");

    // 组件描述符被忽略（不发帧，rpc-mode.ts:200-203）。
    rig.bridge.set_widget(
        "w2",
        Some(WidgetContent::Component(json!({"type": "text"}))),
        None,
    );
    rig.assert_no_frame();

    rig.bridge.set_title("pi - proj");
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "setTitle");
    assert_eq!(frame["title"], "pi - proj");

    rig.bridge.set_editor_text("prefill");
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "set_editor_text");
    assert_eq!(frame["text"], "prefill");

    // pasteToEditor 退化为 set_editor_text 帧。
    rig.bridge.paste_to_editor("pasted");
    let frame = rig.next_frame().await;
    assert_eq!(frame["method"], "set_editor_text");
    assert_eq!(frame["text"], "pasted");
}

// ---------------------------------------------------------------------------
// 降级 18 项（rpc-mode.ts:162-309 逐项）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w4_rpc_degraded_methods() {
    let mut rig = rpc_rig();
    // custom → undefined
    assert_eq!(rig.bridge.custom(json!({"type": "text"}), None).await, None);
    // no-op 集合（无任何帧）
    rig.bridge.set_working_message(Some("x"));
    rig.bridge.set_working_visible(false);
    rig.bridge.set_working_indicator(None);
    rig.bridge.set_hidden_thinking_label(Some("x"));
    rig.bridge.set_footer(Some(json!({"type": "text"})));
    rig.bridge.set_header(Some(json!({"type": "text"})));
    rig.bridge
        .set_editor_component(Some(json!({"type": "text"})));
    rig.bridge.set_tools_expanded(true);
    rig.bridge.add_autocomplete_provider(json!({}));
    let _unsub = rig.bridge.on_terminal_input(Arc::new(|_| None));
    rig.assert_no_frame();
    // 固定返回值
    assert_eq!(rig.bridge.get_editor_text(), "");
    assert!(!rig.bridge.get_tools_expanded());
    assert!(rig.bridge.get_editor_component().is_none());
    assert!(rig.bridge.get_all_themes().is_empty());
    assert!(rig.bridge.get_theme("dark").is_none());
    let result = rig.bridge.set_theme(json!("dark"));
    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("Theme switching not supported in RPC mode")
    );
    // theme getter 返回默认主题 JSON（rpc-mode.ts:283-285）。
    assert_eq!(rig.bridge.theme()["name"], "dark");
}

// ---------------------------------------------------------------------------
// ComponentTree v1 映射（schema: COMPONENT_TREE_SCHEMA_V1）
// ---------------------------------------------------------------------------

#[test]
fn w4_component_tree_text_spacer_box_column() {
    let theme = Arc::new(pir::core::themes::get_theme_by_name("dark").expect("dark"));
    let render = |tree: Value| -> String {
        let component = pir::modes::interactive::component_tree::component_from_tree(&tree, &theme);
        pir_test_support::vt::strip_ansi(&component.render(80).join("\n"))
    };

    // text + 样式
    let out =
        render(json!({"type": "text", "props": {"text": "hello", "bold": true, "fg": "accent"}}));
    assert!(out.contains("hello"));
    // spacer（直接数行：尾部空行在 join 后被 lines() 吞掉）
    let component = pir::modes::interactive::component_tree::component_from_tree(
        &json!({"type": "spacer", "props": {"lines": 3}}),
        &theme,
    );
    assert_eq!(component.render(80).len(), 3);
    // box + children（含 borderColor 边框线）
    let out = render(json!({
        "type": "box",
        "props": {"borderColor": "border"},
        "children": [
            {"type": "text", "props": {"text": "inside"}},
            {"type": "text", "props": {"text": "second"}}
        ]
    }));
    assert!(out.contains("inside"));
    assert!(out.contains("second"));
    assert!(out.contains("─"), "border line present: {out:?}");
    // column
    let out = render(json!({
        "type": "column",
        "children": [{"type": "text", "props": {"text": "a"}}, {"type": "text", "props": {"text": "b"}}]
    }));
    assert!(out.find("a").unwrap() < out.find("b").unwrap());
    // 未知类型：fail-visible（渲染出 JSON 而不 panic）。
    let out = render(json!({"type": "mystery", "props": {}}));
    assert!(out.contains("mystery"));
}

// ---------------------------------------------------------------------------
// Renderer 管线（全链路：inline 扩展注册 renderer → TUI 组件）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w4_message_renderer_descriptor_renders_in_tui() {
    let render_ext = pir_ext_host::loader::InlineExtension::Anonymous(Arc::new(|api| {
        api.register_message_renderer(
            "card",
            Arc::new(|message, _opts| {
                let text = format!("CARD:{}", message["customType"].as_str().unwrap_or(""));
                Ok(Some(json!({
                    "type": "text",
                    "props": {"text": text, "fg": "accent"}
                })))
            }),
        )
        .expect("renderer");
        Box::pin(async { Ok(()) })
    }));
    let host = pir_ext_host::host::NativeExtensionHost::new("/w4-cwd");
    let errors = host.load_inline(&[render_ext]).await;
    assert!(errors.is_empty());

    let (session, _tmp) = w4_session_with_host(host).await;
    let renderer =
        pir::modes::interactive::extension_renderers::host_message_renderer(&session, "card")
            .expect("renderer resolved");
    let message: pir_agent::messages::CustomMessage = serde_json::from_value(json!({
        "role": "custom", "customType": "card", "content": [], "display": true, "timestamp": 1
    }))
    .expect("message");
    let theme = Arc::new(pir::core::themes::get_theme_by_name("dark").expect("dark"));
    let component = renderer(
        &message,
        pir::modes::interactive::components::custom_message::MessageRenderOptions {
            expanded: false,
            output_pad: 0,
        },
        &theme,
    )
    .expect("tree rendered");
    let out = pir_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(out.contains("CARD:card"), "out: {out}");

    // 未注册的 customType → None（默认渲染回退）。
    assert!(
        pir::modes::interactive::extension_renderers::host_message_renderer(&session, "other")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w4_tool_render_override_and_inheritance() {
    // 扩展覆盖内置 read：renderCall 有 → 描述符生效；无 renderResult →
    // 结果侧回退内置（继承）。
    let tool_ext = pir_ext_host::loader::InlineExtension::Anonymous(Arc::new(|api| {
        api.register_tool(pir_ext_host::types::ToolDefinition {
            name: "read".to_owned(),
            label: "read".to_owned(),
            description: "overridden read".to_owned(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters: json!({"type": "object"}),
            constrained_sampling: None,
            render_shell: None,
            prepare_arguments: None,
            execution_mode: None,
            execute: Arc::new(|_req, _ctx| {
                Box::pin(async { Ok(pir_agent::types::AgentToolResult::default()) })
            }),
            render_call: Some(Arc::new(|ctx| {
                let text = format!("EXT-CALL:{}", ctx.tool_call_id);
                Ok(json!({
                    "type": "text",
                    "props": {"text": text}
                }))
            })),
            render_result: None,
        })
        .expect("tool");
        Box::pin(async { Ok(()) })
    }));
    let host = pir_ext_host::host::NativeExtensionHost::new("/w4-cwd");
    let errors = host.load_inline(&[tool_ext]).await;
    assert!(errors.is_empty());

    let (session, _tmp) = w4_session_with_host(host).await;
    let definition =
        pir::modes::interactive::extension_renderers::host_tool_definition(&session, "read")
            .expect("render definition");
    let theme = pir::core::themes::get_theme_by_name("dark").expect("dark");
    let context = pir::modes::interactive::components::tool_execution::ToolRenderContext {
        args: json!({}),
        tool_call_id: "call-9".to_owned(),
        render_handle: pir_tui::tui::RenderHandle::new(|| {}),
        state: Value::Null,
        cwd: "/w4-cwd".to_owned(),
        execution_started: true,
        args_complete: true,
        is_partial: false,
        expanded: false,
        show_images: false,
        is_error: false,
    };
    let component = definition
        .render_call(&json!({}), &theme, &context)
        .expect("call render");
    let out = pir_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(out.contains("EXT-CALL:call-9"), "out: {out}");
    // renderResult 未提供 → None（组件回退内置结果渲染 = slot 继承）。
    assert!(definition
        .render_result(
            &pir::modes::interactive::components::tool_execution::ToolResultState {
                content: Vec::new(),
                is_error: false,
                details: None,
            },
            pir::modes::interactive::components::tool_execution::ResultRenderOptions {
                expanded: false,
                is_partial: false,
            },
            &theme,
            &context,
        )
        .is_none());
}

/// 带真宿主的会话（W3 fixture 的 W4 版）。
async fn w4_session_with_host(
    host: pir_ext_host::host::NativeExtensionHost,
) -> (pir::core::agent_session::AgentSession, W4TempDir) {
    use pir_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };
    let tmp = W4TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

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
    let model = provider.get_model(None).expect("faux model");
    let model_runtime = pir::core::model_runtime::ModelRuntime::create(
        pir::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: pir::core::model_runtime::ModelsPathInput::Path(
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
    let services = pir::core::agent_session_services::create_agent_session_services(
        pir::core::agent_session_services::CreateAgentSessionServicesOptions {
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
        pir::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            pir::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
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
    (created.session, tmp)
}

struct W4TempDir(PathBuf);

impl W4TempDir {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pir-ext-w4-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        W4TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for W4TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
