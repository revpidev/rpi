//! Markdown transform chain end-to-end (T29): extension-registered
//! width-aware transformers flow from the host runner into assistant
//! message rendering.
//!
//! Anchored to `external/pi/packages/coding-agent/src/modes/interactive/
//! components/markdown-transform.ts` @ 4181f66 (714978bf5) and
//! runner.ts:589-591. The heavy `InteractiveUi` tree is not needed: the
//! runner → `createMarkdownTransform` seam and the assistant component's
//! context wiring cover the same chain (component-level messageType/
//! isStreaming assertions live in the assistant/user message unit tests).

use std::sync::{Arc, Mutex};

use rpi::core::extension_host_adapter::ExtensionHostAdapter;
use rpi::core::extensions::ExtensionRunner;
use rpi::core::themes::load_theme;
use rpi::modes::interactive::components::assistant_message::AssistantMessageComponent;
use rpi::modes::interactive::theme::markdown_theme;
use rpi_ai::types::{
    AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, Usage,
};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use rpi_ext_host::types::MarkdownTransformContext;
use rpi_test_support::vt::strip_ansi;
use rpi_tui::tui::Component;

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

/// Extension whose transformer wraps its input as `[label](md)` and records
/// the context it received.
fn transformer_ext(
    label: &'static str,
    calls: Arc<Mutex<Vec<(String, MarkdownTransformContext)>>>,
) -> InlineExtension {
    InlineExtension::Anonymous(Arc::new(move |api| {
        let calls = Arc::clone(&calls);
        api.register_markdown_transformer(Arc::new(move |md, ctx| {
            calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((label.to_owned(), ctx.clone()));
            format!("[{label}]{md}")
        }))
        .expect("register_markdown_transformer");
        Box::pin(async { Ok(()) })
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn markdown_transformers_chain_through_runner_into_assistant_rendering() {
    let calls: Arc<Mutex<Vec<(String, MarkdownTransformContext)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let host = NativeExtensionHost::new("/md-cwd");
    let errors = host
        .load_inline(&[
            transformer_ext("a", Arc::clone(&calls)),
            transformer_ext("b", Arc::clone(&calls)),
        ])
        .await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    // runner.getMarkdownTransformers() (runner.ts:589-591): both
    // transformers, in load order.
    let runner: Arc<dyn ExtensionRunner> = Arc::new(ExtensionHostAdapter::new(Arc::new(host)));
    let transformers = runner.get_markdown_transformers();
    assert_eq!(transformers.len(), 2);

    // Rendering a complete (non-streaming) assistant message applies the
    // chain in order with messageType "assistant" and isStreaming false.
    let theme = Arc::new(load_theme("dark", None).expect("builtin dark theme"));
    let component = AssistantMessageComponent::new(
        Some(assistant_message("hello")),
        false,
        Arc::clone(&theme),
        markdown_theme(&load_theme("dark", None).unwrap()),
        "Thinking...",
        1,
        transformers,
    );
    let stripped = strip_ansi(&component.render(80).join("\n"));
    // Chain order: a applies first, b wraps a's output.
    assert!(stripped.contains("[b][a]hello"), "stripped: {stripped}");

    {
        let calls = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a", "load order = chain order");
        assert_eq!(calls[1].0, "b");
        for (_, context) in &calls {
            assert_eq!(context.message_type, "assistant");
            assert!(!context.is_streaming);
            // width - padding_x * 2 = 80 - 2.
            assert_eq!(context.available_width, 78);
        }
    }

    // A streaming update marks isStreaming true (interactive-mode.ts:3140).
    calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let mut streaming_component = AssistantMessageComponent::new(
        None,
        false,
        Arc::clone(&theme),
        markdown_theme(&load_theme("dark", None).unwrap()),
        "Thinking...",
        1,
        runner.get_markdown_transformers(),
    );
    streaming_component.update_content(&assistant_message("stream"), true);
    let stripped = strip_ansi(&streaming_component.render(80).join("\n"));
    assert!(stripped.contains("[b][a]stream"), "stripped: {stripped}");

    let calls = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "a");
    assert_eq!(calls[1].0, "b");
    for (_, context) in &calls {
        assert_eq!(context.message_type, "assistant");
        assert!(context.is_streaming);
        assert_eq!(context.available_width, 78);
    }
}
