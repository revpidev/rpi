//! Tool task integration tests (port of `packages/durable/test/harness-tools`
//! cases @ `4a080252f`): validation failure, an executed result entry, and
//! diagnostics.

mod generation_support;

use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::types::{Message, Tool as AiTool, ToolCall, ToolResultContent};
use rpi_durable::chord::context::Context;
use rpi_durable::entries::{ASSISTANT_ENTRY, TOOL_RESULT_ENTRY};
use rpi_durable::harness::registry::TOOL_TASK_NAME;
use rpi_durable::harness::types::{DiagnosticSeverity, ToolExecuteError};
use rpi_durable::harness::{
    Extension, ToolDiagnostic, ToolExecute, ToolExecutionApi, ToolExecutionResult,
    ToolRegistration, ToolReplay,
};
use rpi_durable::ids::{ConversationId, TaskId};
use rpi_durable::session::{EntryDraft, TaskCreate, TaskOwnership, commit_fn};
use rpi_durable::types::{EntryRecord, ScanOrder, Storage};

use generation_support::{Opened, context, open, test_models};

/// A tool whose `execute` reports output and one diagnostic.
struct ReportingTool;

#[async_trait]
impl ToolExecute for ReportingTool {
    async fn execute(
        &self,
        _args: rpi_durable::chord::json::JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        api.output(b"hello output", None).expect("output");
        api.diagnostic(ToolDiagnostic {
            severity: DiagnosticSeverity::Warn,
            message: "note".to_string(),
            code: Some("note".to_string()),
        });
        Ok(ToolExecutionResult::default())
    }
}

fn tool_extension(name: &str, parameters: serde_json::Value) -> Extension {
    Extension {
        name: format!("ext-{name}"),
        tools: vec![ToolRegistration {
            tool: AiTool {
                name: name.to_string(),
                description: format!("{name} tool"),
                parameters,
                constrained_sampling: None,
            },
            replay: ToolReplay::Unsafe,
            execution_mode: None,
            prepare_arguments: None,
            output_limits: None,
            execute: Arc::new(ReportingTool),
        }],
        ..Extension::default()
    }
}

fn assistant_with_call(call: &ToolCall) -> Message {
    let content = serde_json::json!({
        "type": "toolCall",
        "id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    });
    let value = serde_json::json!({
        "role": "assistant",
        "content": [content],
        "api": "openai",
        "provider": "test",
        "model": "test-model",
        "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 } },
        "stopReason": "toolUse",
        "timestamp": 1
    });
    Message::Assistant(serde_json::from_value(value).expect("assistant"))
}

/// Append an assistant call and create its conversation-owned `pi.tool` task.
async fn create_tool_task(
    opened: &Opened,
    conversation_id: ConversationId,
    call: ToolCall,
) -> TaskId {
    let change = commit_fn(move |tx| {
        let call = call.clone();
        Box::pin(async move {
            let mut draft = EntryDraft::new(ASSISTANT_ENTRY);
            draft.model = Some(vec![assistant_with_call(&call)]);
            let entry = tx.append_entry(conversation_id, draft).await?;
            tx.create_task(TaskCreate {
                kind: TOOL_TASK_NAME.to_string(),
                version: 1,
                input: serde_json::json!({ "assistant": entry.id, "callId": call.id }),
                checkpoint: serde_json::json!({ "phase": "call" }),
                ownership: TaskOwnership::Conversation(conversation_id),
                conversation_id: Some(conversation_id),
                background: false,
            })
            .await
        })
    });
    opened
        .harness
        .session()
        .commit(change, &context())
        .await
        .expect("create tool task")
}

/// The one `pi.tool-result` entry's `(isError, text, data, durationMs)`.
fn tool_result(entries: &[EntryRecord]) -> (bool, String, serde_json::Value, Option<u64>) {
    let entry = entries
        .iter()
        .find(|entry| entry.kind == TOOL_RESULT_ENTRY)
        .expect("tool result entry");
    for message in entry.model.iter().flatten() {
        if let Message::ToolResult(result) = message {
            let text = result
                .content
                .iter()
                .filter_map(|content| match content {
                    ToolResultContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return (
                result.is_error,
                text,
                entry.data.clone().unwrap_or(serde_json::Value::Null),
                result.duration_ms,
            );
        }
    }
    panic!("tool result message missing");
}

/// upstream: "an executed tool appends its result entry with output and diagnostics".
#[tokio::test]
async fn execute_appends_result_with_output_and_diagnostics() {
    let opened: Opened = open(
        vec![tool_extension(
            "echo",
            serde_json::json!({ "type": "object", "properties": {} }),
        )],
        None,
        test_models(Vec::new()),
    )
    .await;
    let conversation = opened
        .harness
        .root(&context(), Default::default())
        .await
        .expect("root");
    let call = ToolCall {
        id: "c1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::Map::new(),
        thought_signature: None,
        namespace: None,
    };
    let task_id = create_tool_task(&opened, conversation.id(), call).await;
    let settled = opened
        .harness
        .wait_for_task(task_id, &context())
        .await
        .expect("wait");
    assert!(matches!(
        settled.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
    let entries = conversation_entries(&opened, conversation.id()).await;
    let (is_error, text, data, duration_ms) = tool_result(&entries);
    assert!(!is_error);
    assert!(text.contains("hello output"), "got {text}");
    assert!(text.contains("[warn] note"), "got {text}");
    assert!(
        duration_ms.is_some(),
        "the tool task records the execution duration"
    );
    assert_eq!(
        data.get("diagnostics")
            .and_then(|diagnostics| diagnostics.get(0))
            .and_then(|diagnostic| diagnostic.get("code")),
        Some(&serde_json::json!("note"))
    );
    assert!(entries.iter().any(|entry| entry.kind == ASSISTANT_ENTRY));
}

/// upstream: "invalid arguments complete with an invalid_arguments result".
#[tokio::test]
async fn validation_failure_result() {
    let opened = open(
        vec![tool_extension(
            "strict",
            serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        )],
        None,
        test_models(Vec::new()),
    )
    .await;
    let conversation = opened
        .harness
        .root(&context(), Default::default())
        .await
        .expect("root");
    let call = ToolCall {
        id: "c2".to_string(),
        name: "strict".to_string(),
        arguments: serde_json::Map::new(),
        thought_signature: None,
        namespace: None,
    };
    let task_id = create_tool_task(&opened, conversation.id(), call).await;
    let settled = opened
        .harness
        .wait_for_task(task_id, &context())
        .await
        .expect("wait");
    // A tool result with `isError` still completes.
    assert!(matches!(
        settled.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
    let entries = conversation_entries(&opened, conversation.id()).await;
    let (is_error, text, data, _) = tool_result(&entries);
    assert!(is_error);
    assert!(text.contains("Validation failed"), "got {text}");
    assert_eq!(
        data.get("diagnostics")
            .and_then(|diagnostics| diagnostics.get(0))
            .and_then(|diagnostic| diagnostic.get("code")),
        Some(&serde_json::json!("invalid_arguments"))
    );
}

async fn conversation_entries(
    opened: &Opened,
    conversation_id: ConversationId,
) -> Vec<EntryRecord> {
    let storage: &Arc<dyn Storage> = &opened.storage;
    storage
        .scan_entries(
            rpi_durable::types::EntryQuery {
                conversation_id,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            200,
            None,
            &context(),
        )
        .await
        .expect("scan")
        .items
}
