//! TE28 deliverable 4 — host-capability pre-verifications from
//! `00-feasibility.md` §7:
//!
//! 1. **tool execute 内连续宿主对话框**: an extension tool's `execute` may
//!    await consecutive host dialogs (`ui.select` twice) with no deadlock;
//!    the plugin dispatch thread blocks on a oneshot while the host services
//!    the dialog. This binary drives the REAL `NativeExtensionHost` with a
//!    recording `UiBridge` and asserts the two dialogs run strictly
//!    sequentially and both answers reach the tool result.
//! 4. **取消语义**: a dialog cancelled (bridge returns `None`, the `Esc`
//!    contract) surfaces as `None` to the plugin — the value TE29 maps to a
//!    whole-questionnaire cancel.
//!
//! The test uses a synthetic inline extension (not this crate's cdylib): Q0's
//! own `execute` has no dialog branch yet (TE29/TE30), so the host capability
//! is what must be proven before those tasks rely on it.

mod common;

use std::sync::Arc;

use common::{result_text, RecordingBridge};
use rpi_agent::types::AgentToolResult;
use rpi_ai::types::{TextContent, ToolResultContent};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types as ext;
use serde_json::json;

/// Inline extension registering one tool whose `execute` awaits the given
/// number of consecutive `ui.select` dialogs and returns `"A|B"`-style text.
fn dialog_extension(dialogs: usize) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| {
        let execute: ext::ToolExecuteFn = Arc::new(move |_request, ctx| {
            Box::pin(async move {
                let ui = ctx.ui().map_err(|error| error.to_string())?;
                let mut answers: Vec<String> = Vec::new();
                for index in 0..dialogs {
                    let answer = ui
                        .select(
                            &format!("Q{}", index + 1),
                            &["A".to_owned(), "B".to_owned()],
                            None,
                        )
                        .await;
                    answers.push(answer.unwrap_or_else(|| "<cancelled>".to_owned()));
                }
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text: answers.join("|"),
                        text_signature: None,
                    })],
                    ..Default::default()
                })
            })
        });
        api.register_tool(ext::ToolDefinition {
            name: "dialog_asker".to_owned(),
            label: "dialog_asker".to_owned(),
            description: "consecutive host dialogs".to_owned(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters: json!({"type": "object"}),
            constrained_sampling: None,
            render_shell: None,
            prepare_arguments: None,
            execution_mode: None,
            execute,
            render_call: None,
            render_result: None,
        })
        .expect("register dialog_asker");
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Anonymous(factory)
}

async fn run_tool(
    bridge: Arc<RecordingBridge>,
    dialogs: usize,
) -> (AgentToolResult, Arc<RecordingBridge>) {
    let host = std::sync::Arc::new(NativeExtensionHost::new("/prereq-dialogs"));
    let errors = host.load_inline(&[dialog_extension(dialogs)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    host.set_ui(
        Some(bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let definition = host
        .get_tool_definition("dialog_asker")
        .expect("dialog_asker registered");
    let result = (definition.execute)(
        ext::ToolExecuteRequest {
            tool_call_id: "prereq-1".to_owned(),
            params: json!({}),
            signal: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        },
        host.core().create_context(),
    )
    .await
    .expect("tool execute resolves");
    (result, bridge)
}

/// 00 §7 verification 1: consecutive host dialogs inside one tool execute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_execute_runs_consecutive_host_dialogs_without_deadlock() {
    let bridge = RecordingBridge::new(vec![Some("A"), Some("B")]);
    let (result, bridge) = run_tool(bridge, 2).await;

    assert_eq!(result_text(&result), "A|B");
    assert_eq!(bridge.selects(), vec!["Q1", "Q2"], "dialogs run in order");
    assert_eq!(
        bridge.max_in_flight(),
        1,
        "dialogs are strictly sequential (no overlap)"
    );
}

/// 00 §7 verification 4: a cancelled dialog (`Esc` → `None`) surfaces as
/// `None` to the plugin; TE29 maps it to a whole-questionnaire cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_execute_sees_cancelled_dialog_as_none() {
    let bridge = RecordingBridge::new(vec![None]);
    let (result, bridge) = run_tool(bridge, 1).await;

    assert_eq!(result_text(&result), "<cancelled>");
    assert_eq!(bridge.selects(), vec!["Q1"]);
}
