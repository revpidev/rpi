//! End-to-end deferred-tool discovery through the durable `tool_search` tool.

mod generation_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rpi_ai::types::Message;
use rpi_durable::chord::context::Context;
use rpi_durable::chord::json::JsonObject;
use rpi_durable::harness::{
    AGENT_DOC, Extension, ToolCaller, ToolExecute, ToolExecuteError, ToolExecutionApi,
    ToolExecutionResult, ToolRegistration, ToolReplay,
};
use rpi_durable::session::DocOwner;
use rpi_durable::tool_search::create_tool_search_extension;
use rpi_durable::types::EntryRecord;

use generation_support::{
    Script, context, entries, open, root_ready, submit_input, test_models, text_message,
    tool_use_message,
};

struct InnerTool {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolExecute for InnerTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        api.output(b"inner ran", None).expect("output");
        Ok(ToolExecutionResult::default())
    }
}

fn inner_tool(runs: Arc<AtomicUsize>) -> ToolRegistration {
    ToolRegistration {
        tool: rpi_ai::types::Tool {
            name: "inner".to_string(),
            description: "Inner deferred helper".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: None,
        callers: Some(vec![ToolCaller::Model]),
        deferred: true,
        execute: Arc::new(InnerTool { runs }),
    }
}

fn texts(entries: &[EntryRecord]) -> Vec<String> {
    let mut texts = Vec::new();
    for entry in entries {
        let Some(messages) = &entry.model else {
            continue;
        };
        for message in messages {
            if let Message::ToolResult(result) = message {
                texts.push(
                    result
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
    }
    texts
}

#[tokio::test]
async fn tool_search_loads_a_deferred_tool_for_the_next_model_round() {
    let runs = Arc::new(AtomicUsize::new(0));
    let opened = open(
        vec![
            create_tool_search_extension(),
            Extension {
                name: "deferred-test".to_string(),
                tools: vec![inner_tool(Arc::clone(&runs))],
                ..Extension::default()
            },
        ],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                "tool_search",
                "search-1",
                serde_json::json!({ "query": "inner" }),
            )),
            Script::Message(tool_use_message("inner", "call-1", serde_json::json!({}))),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "find the inner tool").await;
    submission.wait(&context()).await.expect("wait");

    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let results = texts(&entries(&conversation).await);
    assert!(
        results.iter().any(|text| text.contains("Loaded 1 tool")),
        "{results:?}"
    );
    assert!(
        results.iter().any(|text| text.contains("inner ran")),
        "{results:?}"
    );

    let agent = opened
        .harness
        .session()
        .snapshot(
            &AGENT_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("agent")
        .expect("present");
    assert_eq!(agent["modelTools"]["add"], serde_json::json!(["inner"]));
}
