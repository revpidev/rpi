//! Code mode extension integration tests (V18-06 follow-up; port of
//! `packages/durable/test/codemode*.test.ts` @ `4a080252f`).
//!
//! Covers the catalog/discovery globals, the store document, persisted start
//! replies, journal divergence, structured and image output, nested errors,
//! corrupt journals, and cancellation, plus the harness-level round trip.

mod generation_support;

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use rpi_ai::models::Models;
use rpi_ai::types::{Message, Tool as AiTool, ToolResultContent};
use rpi_durable::chord::context::{Context, with_abort_signal};
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::codemode::{
    CODEMODE_RUN_DOC, CODEMODE_STORE_DOC, CODEMODE_TOOL_NAME, CodemodeCatalog, CodemodeHost,
    CodemodeImage, CodemodeJournal, CodemodeJournalEntry, CodemodeNamespaceInfo, CodemodeOutcome,
    CodemodeRunState, CodemodeSandbox, CodemodeStart, CodemodeStoreState, CodemodeTextItem,
    CodemodeToolDetails, CodemodeToolInfo, CodemodeToolOptions, GENERATE_IMAGES_TOOL,
    create_codemode_tool, format_outcome,
};
use rpi_durable::harness::types::{DiagnosticSeverity, OutputSkip, OutputWindow};
use rpi_durable::harness::{
    Extension, NestedCallOptions, NestedToolExecutionResult, RetainedOutput, ToolCaller,
    ToolCommitFn, ToolDiagnostic, ToolExecute, ToolExecuteError, ToolExecutionApi,
    ToolExecutionResult, ToolRegistration, ToolReplay,
};
use rpi_durable::ids::TaskId;
use rpi_durable::session::DocOwner;
use rpi_durable::types::{EntryRecord, ScanOrder, TaskQuery};
use tokio_util::sync::CancellationToken;

use generation_support::{
    Script, context, entries, open, root_ready, submit_input, test_models, text_message,
    tool_use_message,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tool_info(name: &str, description: &str) -> CodemodeToolInfo {
    CodemodeToolInfo {
        name: name.to_string(),
        js_name: rpi_durable::codemode::to_codemode_identifier(name),
        description: description.to_string(),
        structured: false,
        hidden: false,
    }
}

fn catalog() -> CodemodeCatalog {
    CodemodeCatalog::new(
        vec![
            tool_info("read_file", "Read a file from disk."),
            tool_info("write_file", "Write a file to disk."),
            tool_info("mcp__github__create_issue", "Create a GitHub issue."),
            tool_info("mcp__github__search_issues", "Search GitHub issues."),
        ],
        vec![CodemodeNamespaceInfo {
            name: "mcp__github".to_string(),
            description: Some("GitHub tools".to_string()),
            instructions: Some("Use the repository's owner/name.".to_string()),
            tools: vec![
                "mcp__github__create_issue".to_string(),
                "mcp__github__search_issues".to_string(),
            ],
        }],
    )
}

fn inner_tool(
    name: &str,
    structured_output_schema: Option<JsonValue>,
    execute: Arc<dyn ToolExecute>,
) -> ToolRegistration {
    ToolRegistration {
        tool: AiTool {
            name: name.to_string(),
            description: name.to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema,
        callers: Some(vec![ToolCaller::Tools]),
        deferred: false,
        execute,
    }
}

fn extension(tools: Vec<ToolRegistration>) -> Extension {
    Extension {
        name: "codemode-test".to_string(),
        tools,
        ..Extension::default()
    }
}

fn tool_result_contents(entries: &[EntryRecord]) -> Vec<Vec<ToolResultContent>> {
    let mut results = Vec::new();
    for entry in entries {
        let Some(messages) = &entry.model else {
            continue;
        };
        for message in messages {
            if let Message::ToolResult(result) = message {
                results.push(result.content.clone());
            }
        }
    }
    results
}

fn tool_result_texts(entries: &[EntryRecord]) -> Vec<String> {
    tool_result_contents(entries)
        .into_iter()
        .map(|content| {
            content
                .iter()
                .filter_map(|block| match block {
                    ToolResultContent::Text(text) => Some(text.text.clone()),
                    ToolResultContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

async fn tool_tasks(opened: &generation_support::Opened, kind: &str) -> Vec<TaskId> {
    let page = opened
        .storage
        .scan_tasks(
            TaskQuery {
                conversation_id: None,
                kind: Some(kind.to_string()),
                status: None,
                abort_requested: None,
                background: None,
                order: Some(ScanOrder::Ascending),
            },
            200,
            None,
            &context(),
        )
        .await
        .expect("scan tasks");
    page.items.iter().map(|task| task.id).collect()
}

// ---------------------------------------------------------------------------
// Discovery and description
// ---------------------------------------------------------------------------

#[test]
fn search_tools_uses_bm25_and_filters_by_namespace() {
    let catalog = catalog();
    let matches = catalog.search("issue github", 8, None);
    assert!(!matches.is_empty());
    assert_eq!(
        matches[0]["name"],
        serde_json::json!("mcp__github__create_issue")
    );

    let namespaced = catalog.search("issue", 8, Some("mcp__github"));
    assert!(!namespaced.is_empty());
    assert!(namespaced.iter().all(|matched| {
        matched["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("mcp__github__"))
    }));

    assert!(catalog.search("issue", 8, Some("missing")).is_empty());
}

#[test]
fn describe_tool_and_namespace_return_portable_json() {
    let catalog = catalog();
    assert_eq!(
        catalog.describe_tool("read_file"),
        Some(serde_json::json!("Read a file from disk."))
    );
    assert_eq!(catalog.describe_tool("missing"), None);
    let namespace = catalog
        .describe_namespace("mcp__github")
        .expect("namespace");
    assert_eq!(namespace["name"], serde_json::json!("mcp__github"));
    assert_eq!(namespace["description"], serde_json::json!("GitHub tools"));
    assert_eq!(
        namespace["tools"],
        serde_json::json!(["mcp__github__create_issue", "mcp__github__search_issues"])
    );
    assert_eq!(catalog.all_tools().len(), 4);
}

#[test]
fn codemode_section_lists_tools_and_the_extension_exposes_the_section() {
    let tool = inner_tool("read_file", None, Arc::new(FailingTool));
    let rendered = rpi_durable::codemode::codemode_section(&[tool], &[], 1_000).expect("section");
    assert!(rendered.contains("read_file"), "got {rendered}");

    let extension = rpi_durable::codemode::create_codemode_extension(CodemodeToolOptions {
        sandbox: Arc::new(UnusedSandbox),
        ..Default::default()
    });
    assert_eq!(extension.name, "codemode");
    assert!(
        extension
            .tools
            .iter()
            .any(|tool| tool.name() == CODEMODE_TOOL_NAME)
    );
    assert!(
        extension.tools.len() >= 3,
        "the code mode and paid model tools are registered"
    );
    assert_eq!(extension.sections.len(), 1);
    assert_eq!(extension.sections[0].key, "codemode");
}

/// Upstream `createCodemodeTool` declares `OUTPUT_LIMITS`: the formatter keeps
/// the result inside them, so the Harness must not cut the formatted head and
/// tail again.
#[test]
fn the_codemode_tool_declares_its_own_output_limits() {
    let tool = create_codemode_tool(CodemodeToolOptions {
        sandbox: Arc::new(UnusedSandbox),
        models: false,
        ..Default::default()
    });
    let limits = tool.output_limits.expect("declared output limits");
    assert_eq!(limits.max_bytes, Some(4 * 1024 * 1024));
    assert_eq!(limits.max_lines, Some(100_000));
    assert_eq!(limits.retain, None);
}

#[test]
fn structured_results_live_in_the_details_channel() {
    let details = CodemodeToolDetails {
        calls: Vec::new(),
        full_output_path: None,
        structured_output: Some(serde_json::json!({ "answer": 42 })),
    };
    let value = serde_json::to_value(&details).expect("details");
    assert_eq!(value["structuredOutput"]["answer"], serde_json::json!(42));
}

// ---------------------------------------------------------------------------
// Journal and output
// ---------------------------------------------------------------------------

fn nested_result(value: i64) -> NestedToolExecutionResult {
    NestedToolExecutionResult {
        task_id: TaskId::from_number(1),
        structured_output: Some(serde_json::json!({ "value": value })),
        is_error: false,
        details: None,
        diagnostics: Vec::new(),
        usage: None,
        duration_ms: Some(5),
    }
}

#[test]
fn journal_divergence_is_an_error_and_the_first_result_stays() {
    let mut journal = CodemodeJournal::default();
    let args = JsonObject::new();
    journal
        .record("c1", "inner", &args, 3.0, nested_result(1))
        .expect("record");
    journal
        .record("c1", "inner", &args, 4.0, nested_result(2))
        .expect("same call");
    assert_eq!(
        journal
            .get("c1")
            .and_then(|result| result.structured_output.clone()),
        Some(serde_json::json!({ "value": 1 }))
    );
    let other_args = serde_json::json!({ "q": "x" })
        .as_object()
        .cloned()
        .unwrap_or_default();
    assert!(
        journal
            .record("c1", "inner", &other_args, 5.0, nested_result(3))
            .is_err()
    );
    assert!(
        journal
            .record("c1", "another", &args, 6.0, nested_result(3))
            .is_err()
    );

    // A journal entry without a fingerprint cannot prove that a replay made
    // the same call; it is corrupt, not a wildcard.
    let mut legacy = CodemodeJournal::default();
    legacy.entries.push(CodemodeJournalEntry {
        key: "c1".to_string(),
        name: "inner".to_string(),
        args_fingerprint: None,
        at: 0.0,
        issued_after: None,
        global: None,
        result: Some(nested_result(1)),
    });
    assert!(legacy.validate("c1", "inner", &args).is_err());

    // Entries keep delivery order, and a global's outcome is journaled too.
    journal
        .record("c2", "inner", &args, 7.0, nested_result(4))
        .expect("second call");
    assert_eq!(
        journal
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["c1", "c2"]
    );
    journal
        .record_global(
            "g1",
            "models.getModelsOfType",
            &serde_json::json!(["chat"]),
            8.0,
            serde_json::json!([{ "id": "m" }]),
        )
        .expect("global");
    assert_eq!(
        journal
            .get_global("g1")
            .map(|global| global.outcome.clone()),
        Some(serde_json::json!([{ "id": "m" }]))
    );
}

#[test]
fn a_missing_journal_is_empty_and_a_corrupt_one_is_rejected() {
    let missing: CodemodeJournal = serde_json::from_value(serde_json::json!({})).expect("empty");
    assert!(missing.entries.is_empty());
    assert!(
        serde_json::from_value::<CodemodeJournal>(serde_json::json!({ "entries": 7 })).is_err()
    );
    assert!(
        serde_json::from_value::<CodemodeJournalEntry>(serde_json::json!({
            "key": 7
        }))
        .is_err()
    );
}

#[tokio::test]
async fn structured_output_and_images_format_into_the_result() {
    let outcome = CodemodeOutcome {
        output: "text".to_string(),
        text_items: vec![CodemodeTextItem {
            text: "console".to_string(),
            console: true,
        }],
        structured_output: Some(serde_json::json!({ "answer": 42 })),
        images: vec![CodemodeImage {
            data: base64::engine::general_purpose::STANDARD.encode([9_u8, 8, 7]),
            mime_type: "image/png".to_string(),
        }],
        ..Default::default()
    };
    let formatted = format_outcome(&outcome, 500.0, 10_000, None, &[], 0).await;
    let text = formatted
        .content
        .iter()
        .filter_map(|content| match content {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Script completed"));
    assert!(text.contains("<console_output>\nconsole\n</console_output>"));
    assert!(text.contains("{\"answer\":42}"));
    assert!(
        formatted
            .content
            .iter()
            .any(|content| matches!(content, ToolResultContent::Image(_)))
    );
}

#[test]
fn the_store_document_is_rewindable_and_forked_as_of() {
    use rpi_durable::types::{DocumentFork, DocumentHistory};

    let definition = &CODEMODE_STORE_DOC.definition;
    assert_eq!(
        definition.scope,
        rpi_durable::documents::DocScope::Conversation
    );
    assert_eq!(definition.history, Some(DocumentHistory::Rewindable));
    assert_eq!(definition.fork, Some(DocumentFork::AsOf));
}

// ---------------------------------------------------------------------------
// Harness round trips
// ---------------------------------------------------------------------------

struct StoreSandbox;

#[async_trait]
impl CodemodeSandbox for StoreSandbox {
    fn fingerprint(&self) -> String {
        "store-1".to_string()
    }
    async fn run(
        &self,
        start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        if start.code.contains("delete") {
            host.delete("greeting").await?;
            Ok(CodemodeOutcome::output("deleted"))
        } else if start.code.contains("load") {
            let value = host.load("greeting").await?;
            Ok(CodemodeOutcome::output(
                value
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "missing".to_string()),
            ))
        } else {
            host.set("greeting", serde_json::json!("hello")).await?;
            Ok(CodemodeOutcome::output("stored"))
        }
    }
}

#[tokio::test]
async fn store_round_trips_across_codemode_calls() {
    let code = create_codemode_tool(CodemodeToolOptions {
        sandbox: Arc::new(StoreSandbox),
        ..Default::default()
    });
    let opened = open(
        vec![extension(vec![code])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "store" }),
            )),
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c2",
                serde_json::json!({ "code": "load" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(results.len(), 2, "two code mode results: {results:?}");
    assert!(results[0].contains("stored"), "got {}", results[0]);
    assert!(results[1].contains("hello"), "got {}", results[1]);

    let store = opened
        .harness
        .session()
        .snapshot(
            &CODEMODE_STORE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("store snapshot")
        .expect("store document");
    let store: CodemodeStoreState =
        serde_json::from_value(JsonValue::Object(store)).expect("store state");
    assert_eq!(
        store.values.get("greeting"),
        Some(&serde_json::json!("hello"))
    );

    assert_eq!(tool_tasks(&opened, "pi.tool").await.len(), 2);
}

#[tokio::test]
async fn store_delete_round_trips() {
    let code = create_codemode_tool(CodemodeToolOptions {
        sandbox: Arc::new(StoreSandbox),
        ..Default::default()
    });
    let opened = open(
        vec![extension(vec![code])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "store" }),
            )),
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c2",
                serde_json::json!({ "code": "delete" }),
            )),
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c3",
                serde_json::json!({ "code": "load" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(results.len(), 3, "three code mode results: {results:?}");
    assert!(results[0].contains("stored"), "got {}", results[0]);
    assert!(results[1].contains("deleted"), "got {}", results[1]);
    assert!(results[2].contains("missing"), "got {}", results[2]);

    let store = opened
        .harness
        .session()
        .snapshot(
            &CODEMODE_STORE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("store snapshot")
        .expect("store document");
    let store: CodemodeStoreState =
        serde_json::from_value(JsonValue::Object(store)).expect("store state");
    assert!(!store.values.contains_key("greeting"));
}

struct NestedErrorSandbox {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for NestedErrorSandbox {
    fn fingerprint(&self) -> String {
        "nested-error-1".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let result = host.call_tool("failing", JsonObject::new(), "c1").await?;
        self.started.notify_one();
        self.release.notified().await;
        Ok(CodemodeOutcome::output(format!(
            "is_error={} diagnostics={}",
            result.is_error,
            result.diagnostics.len()
        )))
    }
}

struct FailingTool;

#[async_trait]
impl ToolExecute for FailingTool {
    async fn execute(
        &self,
        _args: JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        Ok(ToolExecutionResult {
            is_error: Some(true),
            diagnostics: Some(vec![ToolDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: "nope".to_string(),
                code: None,
            }]),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_nested_error_result_is_journaled_and_visible_to_the_script() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let opened = open(
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::new(NestedErrorSandbox {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                }),
                ..Default::default()
            }),
            inner_tool("failing", None, Arc::new(FailingTool)),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let waiting = {
        let submission = Arc::clone(&submission);
        tokio::spawn(async move { submission.wait(&context()).await })
    };
    started.notified().await;

    // The task is still running, so its journal document is alive.
    let task = tool_tasks(&opened, "pi.tool").await[0];
    let journal = opened
        .harness
        .session()
        .snapshot(
            &rpi_durable::codemode::CODEMODE_JOURNAL_DOC,
            DocOwner::Task(task),
            &context(),
        )
        .await
        .expect("journal snapshot")
        .expect("journal");
    let journal: CodemodeJournal =
        serde_json::from_value(JsonValue::Object(journal)).expect("journal state");
    let result = journal.get("c1").expect("journaled result");
    assert!(result.is_error);
    assert_eq!(result.diagnostics.len(), 1);

    release.notify_one();
    waiting.await.expect("join").expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("is_error=true diagnostics=1"),
        "got {}",
        results[0]
    );
}

struct ImageSandbox;

#[async_trait]
impl CodemodeSandbox for ImageSandbox {
    fn fingerprint(&self) -> String {
        "image-1".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let result = host
            .call_tool("image_inner", JsonObject::new(), "c1")
            .await?;
        let images = result
            .structured_output
            .as_ref()
            .and_then(|value| value.get("images"))
            .and_then(JsonValue::as_array)
            .map(|images| {
                images
                    .iter()
                    .filter_map(|image| {
                        Some(CodemodeImage {
                            data: image.get("data")?.as_str()?.to_string(),
                            mime_type: image.get("mimeType")?.as_str()?.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(CodemodeOutcome::output("rendered").with_images(images))
    }
}

struct ImageTool;

#[async_trait]
impl ToolExecute for ImageTool {
    async fn execute(
        &self,
        _args: JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        Ok(ToolExecutionResult {
            structured_output: Some(serde_json::json!({
                "images": [{
                    "data": base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]),
                    "mimeType": "image/png"
                }]
            })),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn an_image_result_flows_through_a_nested_call() {
    let opened = open(
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::new(ImageSandbox),
                ..Default::default()
            }),
            inner_tool(
                "image_inner",
                Some(serde_json::json!({ "type": "object" })),
                Arc::new(ImageTool),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let contents = tool_result_contents(&entries(&conversation).await);
    assert_eq!(contents.len(), 1);
    assert!(
        contents[0]
            .iter()
            .any(|content| matches!(content, ToolResultContent::Image(_))),
        "expected an image block: {:?}",
        contents[0]
    );
    let texts = tool_result_texts(&entries(&conversation).await);
    assert!(texts[0].contains("rendered"));
}

struct GeneratedImageTool;

#[async_trait]
impl ToolExecute for GeneratedImageTool {
    async fn execute(
        &self,
        _args: JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        Ok(ToolExecutionResult {
            structured_output: Some(serde_json::json!({
                "output": [{
                    "type": "image",
                    "data": base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]),
                    "mimeType": "image/png"
                }]
            })),
            ..Default::default()
        })
    }
}

fn generated_images(result: &NestedToolExecutionResult) -> Vec<CodemodeImage> {
    result
        .structured_output
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(JsonValue::as_str) != Some("image") {
                        return None;
                    }
                    Some(CodemodeImage {
                        data: item.get("data")?.as_str()?.to_string(),
                        mime_type: item.get("mimeType")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

struct HiddenGeneratedImagesSandbox;

#[async_trait]
impl CodemodeSandbox for HiddenGeneratedImagesSandbox {
    fn fingerprint(&self) -> String {
        "generated-images-hidden".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let result = host
            .call_tool(GENERATE_IMAGES_TOOL, JsonObject::new(), "c1")
            .await?;
        assert_eq!(generated_images(&result).len(), 1);
        Ok(CodemodeOutcome::output("done"))
    }
}

struct ShownGeneratedImagesSandbox;

#[async_trait]
impl CodemodeSandbox for ShownGeneratedImagesSandbox {
    fn fingerprint(&self) -> String {
        "generated-images-shown".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let result = host
            .call_tool(GENERATE_IMAGES_TOOL, JsonObject::new(), "c1")
            .await?;
        Ok(CodemodeOutcome::output("done").with_images(generated_images(&result)))
    }
}

async fn open_codemode_case(sandbox: Arc<dyn CodemodeSandbox>) -> generation_support::Opened {
    open(
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox,
                ..Default::default()
            }),
            inner_tool(
                GENERATE_IMAGES_TOOL,
                Some(serde_json::json!({ "type": "object" })),
                Arc::new(GeneratedImageTool),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await
}

/// Upstream `codemode-models.test.ts`: generated images the script did not show
/// are named; shown images are returned without the note.
#[tokio::test]
async fn generated_images_the_script_did_not_show_are_noted() {
    let opened = open_codemode_case(Arc::new(HiddenGeneratedImagesSandbox)).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let texts = tool_result_texts(&entries(&conversation).await);
    assert!(
        texts.iter().any(|text| text.contains(
            "Note: models.generateImages() returned 1 image that the script did not show"
        )),
        "got {texts:?}"
    );

    let opened = open_codemode_case(Arc::new(ShownGeneratedImagesSandbox)).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let entries = entries(&conversation).await;
    let texts = tool_result_texts(&entries);
    assert!(
        !texts.iter().any(|text| text.contains("Note:")),
        "shown images need no note: {texts:?}"
    );
    assert!(
        tool_result_contents(&entries)
            .iter()
            .flatten()
            .any(|content| matches!(content, ToolResultContent::Image(_))),
        "the shown image is returned"
    );
}

struct LongOutputSandbox;

#[async_trait]
impl CodemodeSandbox for LongOutputSandbox {
    fn fingerprint(&self) -> String {
        "long-output-1".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        _host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let text = (0..200_000)
            .map(|index| match index {
                0 => "BEGIN".to_string(),
                199_999 => "END".to_string(),
                _ => "a".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CodemodeOutcome::output(text))
    }
}

/// Upstream `codemode.test.ts` 200,000-line case: both ends survive, the line
/// limit applies, and the Harness does not cut the formatted result again.
#[tokio::test]
async fn a_long_result_keeps_both_ends_within_the_line_limit() {
    let opened = open_codemode_case(Arc::new(LongOutputSandbox)).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let texts = tool_result_texts(&entries(&conversation).await);
    let text = texts
        .iter()
        .find(|text| text.contains("Warning: truncated output"))
        .unwrap_or_else(|| panic!("no truncated result: {texts:?}"));
    assert!(text.contains("BEGIN"), "the head is kept");
    assert!(text.contains("END"), "the tail is kept");
    assert!(
        text.split('\n').count() < 100_000,
        "the result is bounded by the line limit"
    );
    assert!(!text.contains("0 tokens truncated"), "{text}");
}

// ---------------------------------------------------------------------------
// Start data and the host's start reply
// ---------------------------------------------------------------------------

struct HeldStartSandbox {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for HeldStartSandbox {
    fn fingerprint(&self) -> String {
        "start-reply-1".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        let reply = host.start_reply();
        self.started.notify_one();
        self.release.notified().await;
        Ok(CodemodeOutcome::output(format!("reply={reply}")))
    }
}

#[tokio::test]
async fn start_data_and_reply_are_persisted_before_the_sandbox_runs() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let opened = open(
        vec![extension(vec![create_codemode_tool(CodemodeToolOptions {
            sandbox: Arc::new(HeldStartSandbox {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
            ..Default::default()
        })])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({
                    "code": "// @options: {\"max_output_tokens\": 123, \"timeout_ms\": 60000}\nreturn 1"
                }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let waiting = {
        let submission = Arc::clone(&submission);
        tokio::spawn(async move { submission.wait(&context()).await })
    };
    started.notified().await;

    let task = tool_tasks(&opened, "pi.tool").await[0];
    let run = opened
        .harness
        .session()
        .snapshot(&CODEMODE_RUN_DOC, DocOwner::Task(task), &context())
        .await
        .expect("run snapshot")
        .expect("run document");
    let run: CodemodeRunState = serde_json::from_value(JsonValue::Object(run)).expect("run state");
    let start = run.start.expect("start data");
    assert_eq!(
        start.version,
        rpi_durable::codemode::CODEMODE_MODULE_VERSION
    );
    assert_eq!(start.max_output_tokens, 123);
    assert!(start.deadline.is_some());
    assert!(
        start
            .globals
            .iter()
            .any(|global| global.name == "searchTools")
    );
    assert_eq!(run.start_reply, Some(JsonValue::Null));

    release.notify_one();
    waiting.await.expect("join").expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert!(
        results.iter().any(|text| text.contains("reply=null")),
        "got {results:?}"
    );
}

#[test]
fn a_persisted_null_start_reply_is_not_an_absent_reply() {
    let state = CodemodeRunState {
        start_reply: Some(JsonValue::Null),
        ..Default::default()
    };
    let encoded = serde_json::to_value(&state).expect("encode");
    let decoded: CodemodeRunState = serde_json::from_value(encoded).expect("decode");
    assert_eq!(decoded.start_reply, Some(JsonValue::Null));
    let absent: CodemodeRunState = serde_json::from_value(serde_json::json!({})).expect("absent");
    assert_eq!(absent.start_reply, None);
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Never used: a cancelled invocation returns before touching the API.
struct PanicApi;

impl PanicApi {
    fn never<T>() -> T {
        unreachable!("a cancelled code mode call must not touch the tool API")
    }
}

#[async_trait]
impl ToolExecutionApi for PanicApi {
    fn task_id(&self) -> TaskId {
        Self::never()
    }
    fn conversation_id(&self) -> rpi_durable::ids::ConversationId {
        Self::never()
    }
    fn call_id(&self) -> &str {
        Self::never()
    }
    fn registry(&self) -> Arc<dyn rpi_durable::harness::RegistrySnapshot> {
        Self::never()
    }
    async fn agent(
        &self,
        _context: &Context,
    ) -> Result<rpi_durable::harness::Agent, rpi_durable::session::SessionError> {
        Self::never()
    }
    fn models(&self) -> Models {
        Self::never()
    }
    fn env(&self) -> Option<Arc<dyn rpi_durable::harness::ExecutionEnv>> {
        Self::never()
    }
    fn now(&self) -> Result<f64, rpi_durable::session::SessionError> {
        Self::never()
    }
    fn output(&self, _chunk: &[u8], _skipped: Option<OutputSkip>) -> Result<(), String> {
        Self::never()
    }
    fn output_window(&self) -> Option<OutputWindow> {
        Self::never()
    }
    fn retained_output(&self) -> Result<RetainedOutput, String> {
        Self::never()
    }
    fn diagnostic(&self, _diagnostic: ToolDiagnostic) {
        Self::never()
    }
    async fn details(
        &self,
        _value: JsonValue,
        _context: &Context,
    ) -> Result<(), rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn commit(
        &self,
        _change: ToolCommitFn,
        _context: &Context,
    ) -> Result<JsonValue, rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn memo(
        &self,
        _name: &str,
        _context: &Context,
    ) -> Result<Option<JsonValue>, rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn memo_set(
        &self,
        _name: &str,
        _value: JsonValue,
        _context: &Context,
    ) -> Result<JsonValue, rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn create_task(
        &self,
        _definition: rpi_durable::harness::AnyTask,
        _input: JsonValue,
        _ownership: rpi_durable::session::TaskOwnership,
        _background: bool,
        _abandon_on_restart: Option<bool>,
        _context: &Context,
    ) -> Result<TaskId, rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn get_task(
        &self,
        _id: TaskId,
        _context: &Context,
    ) -> Result<Option<rpi_durable::types::TaskRecord>, rpi_durable::session::SessionError> {
        Self::never()
    }
    async fn wait_for_task(
        &self,
        _id: TaskId,
        _context: &Context,
    ) -> Result<rpi_durable::harness::SettledTask<JsonValue>, rpi_durable::session::SessionError>
    {
        Self::never()
    }
    async fn conversation(
        &self,
        _id: rpi_durable::ids::ConversationId,
        _context: &Context,
    ) -> Result<
        Option<Arc<dyn rpi_durable::harness::ConversationHandle>>,
        rpi_durable::session::SessionError,
    > {
        Self::never()
    }
    async fn execute_tool(
        &self,
        _name: &str,
        _args: JsonObject,
        _context: &Context,
        _options: NestedCallOptions,
    ) -> Result<NestedToolExecutionResult, rpi_durable::session::SessionError> {
        Self::never()
    }
}

struct UnusedSandbox;

#[async_trait]
impl CodemodeSandbox for UnusedSandbox {
    fn fingerprint(&self) -> String {
        "unused".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        _host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        Err("must not run".to_string())
    }
}

#[tokio::test]
async fn a_cancelled_context_does_not_run_or_store_a_result() {
    let tool = create_codemode_tool(CodemodeToolOptions {
        sandbox: Arc::new(UnusedSandbox),
        ..Default::default()
    });
    let token = CancellationToken::new();
    token.cancel();
    let cancelled = with_abort_signal(token, &context());
    let args = serde_json::json!({ "code": "return 1" })
        .as_object()
        .cloned()
        .expect("arguments");
    let error = tool
        .execute
        .execute(args, &PanicApi, &cancelled)
        .await
        .expect_err("a cancelled run is not a tool result");
    assert!(error.to_string().contains("aborted"), "got {error}");
}
