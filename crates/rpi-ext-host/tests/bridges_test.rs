//! `NullUiBridge` tests (T15 W4) — the per-method `noOpUIContext` semantics
//! (runner.ts:233-264) and the unbound-default alignment (runner.ts:269,
//! 438-440).

use std::sync::Arc;

use rpi_ext_host::api::UiBridge;
use rpi_ext_host::bridges::NullUiBridge;
use serde_json::json;

#[tokio::test]
async fn null_bridge_matches_noop_ui_context() {
    let bridge = NullUiBridge::new(json!({"name": "dark"}));

    assert!(bridge.select("t", &["a".to_owned()], None).await.is_none());
    assert!(!bridge.confirm("t", "m", None).await);
    assert!(bridge.input("t", None, None).await.is_none());
    assert!(bridge.editor("t", None).await.is_none());
    assert!(bridge.custom(json!({"type": "text"}), None).await.is_none());
    assert_eq!(bridge.get_editor_text(), "");
    assert!(bridge.get_theme("dark").is_none());
    assert!(bridge.get_all_themes().is_empty());
    assert!(!bridge.get_tools_expanded());
    assert!(bridge.get_editor_component().is_none());

    let set_theme = bridge.set_theme(json!("dark"));
    assert!(!set_theme.success);
    assert_eq!(set_theme.error.as_deref(), Some("UI not available"));

    // no-op 方法可调用且不产出任何效果（不 panic）。
    bridge.notify("m", rpi_ext_host::api::NotifyType::Info);
    bridge.set_status("k", Some("v"));
    bridge.set_working_message(Some("m"));
    bridge.set_working_visible(true);
    bridge.set_working_indicator(None);
    bridge.set_hidden_thinking_label(Some("l"));
    bridge.set_widget("w", None, None);
    bridge.set_footer(None);
    bridge.set_header(None);
    bridge.set_title("t");
    bridge.paste_to_editor("p");
    bridge.set_editor_text("p");
    bridge.add_autocomplete_provider(json!({}));
    bridge.set_editor_component(None);
    bridge.set_tools_expanded(true);
    let _unsub = bridge.on_terminal_input(Arc::new(|_| None));

    // theme getter 返回构造时注入的默认主题（runner.ts:256-258）。
    assert_eq!(bridge.theme(), json!({"name": "dark"}));
    assert!(bridge.is_noop());
}

#[tokio::test]
async fn unbound_ui_falls_back_to_null_bridge_and_has_ui_false() {
    // runner.ts:269 + :438-440：未绑定时 ui 是 noOp 而非抛错，hasUI=false。
    let host = rpi_ext_host::host::NativeExtensionHost::new("/bridges-cwd");
    let ctx = host.core().create_context();
    let bridge = ctx.ui().expect("null fallback, not an error");
    assert!(bridge.select("t", &[], None).await.is_none());
    assert!(!ctx.has_ui().unwrap());

    // 绑了 Null 桥同样 hasUI=false（runner.ts:439 的 identity 检查语义）。
    host.runtime().set_ui_bridge(
        Some(Arc::new(NullUiBridge::default())),
        rpi_ext_host::types::ExtensionMode::Print,
    );
    assert!(!host.runtime().has_ui());
    assert!(!ctx.has_ui().unwrap());
}
