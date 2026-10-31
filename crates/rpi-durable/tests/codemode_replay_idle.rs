//! Idle-driven codemode replay regressions for concurrent crash windows and
//! independently dispatched calls.

mod generation_support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::{StreamExt, stream::FuturesUnordered};
use rpi_ai::types::Message;
use rpi_durable::chord::context::Context;
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::codemode::{
    CODEMODE_JOURNAL_DOC, CODEMODE_TOOL_NAME, CodemodeHost, CodemodeJournal, CodemodeOutcome,
    CodemodeSandbox, CodemodeStart, CodemodeToolOptions, create_codemode_tool,
};
use rpi_durable::harness::{
    Extension, NestedToolExecutionResult, ToolCaller, ToolExecute, ToolExecuteError,
    ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay,
};
use rpi_durable::ids::TaskId;
use rpi_durable::session::DocOwner;
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{EntryRecord, Storage};

use generation_support::{
    Script, context, entries, open_with_storage, root_ready, submit_input, test_models,
    text_message, tool_use_message,
};

fn tool(name: &str, execute: Arc<dyn ToolExecute>) -> ToolRegistration {
    ToolRegistration {
        tool: rpi_ai::types::Tool {
            name: name.to_string(),
            description: name.to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: Some(serde_json::json!({})),
        callers: Some(vec![ToolCaller::Tools]),
        deferred: false,
        execute,
    }
}

fn extension(tools: Vec<ToolRegistration>) -> Extension {
    Extension {
        name: "codemode-idle-test".to_string(),
        tools,
        ..Extension::default()
    }
}

struct ReplayTool {
    runs: Arc<AtomicUsize>,
    gate_key: Option<&'static str>,
    gate_open: AtomicBool,
    started: Option<Arc<tokio::sync::Notify>>,
    release: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait]
impl ToolExecute for ReplayTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self
            .gate_key
            .is_some_and(|key| api.call_id().ends_with(key))
            && !self.gate_open.swap(true, Ordering::SeqCst)
        {
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
        }
        Ok(ToolExecutionResult {
            structured_output: Some(serde_json::json!({ "ok": true })),
            ..Default::default()
        })
    }
}

fn tool_result_texts(entries: &[EntryRecord]) -> Vec<String> {
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

fn codemode_call_count(entries: &[EntryRecord]) -> Option<usize> {
    entries.iter().find_map(|entry| {
        entry.model.as_ref()?.iter().find_map(|message| {
            let Message::ToolResult(result) = message else {
                return None;
            };
            result
                .details
                .as_ref()?
                .get("calls")?
                .as_array()
                .map(Vec::len)
        })
    })
}

async fn parent_task(harness: &rpi_durable::harness::HarnessImpl) -> TaskId {
    harness
        .inspect(&context())
        .await
        .expect("inspect")
        .tasks
        .into_iter()
        .find(|task| task.record.kind == "pi.tool" && task.record.input["kind"] == "model")
        .expect("codemode parent")
        .record
        .id
}

async fn journal(harness: &rpi_durable::harness::HarnessImpl, task: TaskId) -> CodemodeJournal {
    let value = harness
        .session()
        .snapshot(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task), &context())
        .await
        .expect("journal")
        .expect("present");
    serde_json::from_value(JsonValue::Object(value)).expect("journal json")
}

fn models() -> rpi_ai::models::Models {
    test_models(vec![
        Script::Message(tool_use_message(
            CODEMODE_TOOL_NAME,
            "c1",
            serde_json::json!({ "code": "return 1" }),
        )),
        Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
    ])
}

struct ConcurrentAfterFirstSandbox {
    attempts: AtomicUsize,
    journaled: Arc<tokio::sync::Notify>,
    replay_ready: Arc<tokio::sync::Notify>,
    crash: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for ConcurrentAfterFirstSandbox {
    fn fingerprint(&self) -> String {
        "concurrent-after-first-1".to_string()
    }

    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        type CallFuture<'a> =
            Pin<Box<dyn Future<Output = Result<NestedToolExecutionResult, String>> + Send + 'a>>;
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        host.call_tool("inner", JsonObject::new(), "c1").await?;
        let mut calls: FuturesUnordered<CallFuture<'_>> = FuturesUnordered::new();
        calls.push(Box::pin(host.call_tool("inner", JsonObject::new(), "c2")));
        calls.push(Box::pin(host.call_tool("inner", JsonObject::new(), "c3")));
        let first = calls.next().await.expect("two calls")?;
        {
            let pending = calls.next();
            tokio::pin!(pending);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut pending).await;
        }
        if attempt == 0 {
            self.journaled.notify_one();
            self.crash.notified().await;
        } else {
            let idle = host.idle(2, 3).await;
            self.replay_ready.notify_one();
            idle?;
        }
        drop(first);
        while let Some(result) = calls.next().await {
            result?;
        }
        Ok(CodemodeOutcome::output("done"))
    }
}

#[tokio::test]
async fn calls_issued_after_an_earlier_result_replay_without_false_divergence() {
    let runs = Arc::new(AtomicUsize::new(0));
    let c3_started = Arc::new(tokio::sync::Notify::new());
    let c3_release = Arc::new(tokio::sync::Notify::new());
    let journaled = Arc::new(tokio::sync::Notify::new());
    let replay_ready = Arc::new(tokio::sync::Notify::new());
    let crash = Arc::new(tokio::sync::Notify::new());
    let sandbox = Arc::new(ConcurrentAfterFirstSandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        replay_ready: Arc::clone(&replay_ready),
        crash,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "inner",
                Arc::new(ReplayTool {
                    runs: Arc::clone(&runs),
                    gate_key: Some("/c3"),
                    gate_open: AtomicBool::new(false),
                    started: Some(Arc::clone(&c3_started)),
                    release: Some(Arc::clone(&c3_release)),
                }),
            ),
        ])]
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    journaled.notified().await;
    c3_started.notified().await;

    let parent = parent_task(&opened.harness).await;
    let journal = journal(&opened.harness, parent).await;
    assert_eq!(journal.entries.len(), 2);
    assert_eq!(journal.entries[0].key, "c1");
    // The count is written only by `idle()`: the sandbox never reported this
    // boundary before the crash, so it stays unknown and the replay does not
    // compare it (upstream records counts only from the VM's idle report).
    assert_eq!(journal.first_issued, None);
    assert_eq!(journal.entries[0].issued_after, None);
    assert_eq!(journal.entries[1].key, "c2");

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(std::time::Duration::from_secs(2), replay_ready.notified())
        .await
        .expect("replay did not reach idle");
    c3_release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("replay did not settle")
    .expect("replay settled");

    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("done")),
        "{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("different path")),
        "{results:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 3, "no nested call ran twice");
    assert_eq!(
        codemode_call_count(&entries(&root_ready(&reopened.harness).await).await),
        Some(3)
    );
}

struct DelayedArrivalSandbox {
    attempts: AtomicUsize,
    journaled: Arc<tokio::sync::Notify>,
    crash: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for DelayedArrivalSandbox {
    fn fingerprint(&self) -> String {
        "delayed-arrival-1".to_string()
    }

    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            host.call_tool("inner", JsonObject::new(), "c1").await?;
            host.call_tool("inner", JsonObject::new(), "c2").await?;
            self.journaled.notify_one();
            self.crash.notified().await;
            return Ok(CodemodeOutcome::output("unreachable"));
        }

        let later = host.call_tool("inner", JsonObject::new(), "c2");
        tokio::pin!(later);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut later).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        let first = host.call_tool("inner", JsonObject::new(), "c1");
        tokio::pin!(first);
        let (first, later) = tokio::join!(first, later);
        first?;
        later?;
        host.idle(2, 2).await?;
        Ok(CodemodeOutcome::output("done"))
    }
}

#[tokio::test]
async fn a_call_delayed_beyond_one_yield_can_arrive_before_idle() {
    let runs = Arc::new(AtomicUsize::new(0));
    let journaled = Arc::new(tokio::sync::Notify::new());
    let crash = Arc::new(tokio::sync::Notify::new());
    let sandbox = Arc::new(DelayedArrivalSandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        crash,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "inner",
                Arc::new(ReplayTool {
                    runs: Arc::clone(&runs),
                    gate_key: None,
                    gate_open: AtomicBool::new(false),
                    started: None,
                    release: None,
                }),
            ),
        ])]
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    journaled.notified().await;
    let parent = parent_task(&opened.harness).await;
    assert_eq!(journal(&opened.harness, parent).await.entries.len(), 2);

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("replay did not settle")
    .expect("replay settled");

    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("done")),
        "{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("different path")),
        "{results:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        codemode_call_count(&entries(&root_ready(&reopened.harness).await).await),
        Some(2)
    );
}

struct SwallowedDivergenceSandbox {
    attempts: AtomicUsize,
    journaled: Arc<tokio::sync::Notify>,
    crash: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for SwallowedDivergenceSandbox {
    fn fingerprint(&self) -> String {
        "swallowed-divergence-1".to_string()
    }

    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            host.call_tool("inner", JsonObject::new(), "c1").await?;
            self.journaled.notify_one();
            self.crash.notified().await;
            return Ok(CodemodeOutcome::output("unreachable"));
        }
        host.call_tool("inner", JsonObject::new(), "c1").await?;
        host.idle(1, 1).await?;
        let different = serde_json::json!({ "different": true });
        let _ = host
            .call_tool(
                "inner",
                different.as_object().cloned().unwrap_or_default(),
                "c1",
            )
            .await;
        Ok(CodemodeOutcome::output("swallowed"))
    }
}

/// A sandbox that ignores a divergence error still fails the run; the host
/// latches the divergence and checks it after the sandbox returns.
#[tokio::test]
async fn an_ignored_divergence_still_fails_the_run() {
    let runs = Arc::new(AtomicUsize::new(0));
    let journaled = Arc::new(tokio::sync::Notify::new());
    let crash = Arc::new(tokio::sync::Notify::new());
    let sandbox = Arc::new(SwallowedDivergenceSandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        crash,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "inner",
                Arc::new(ReplayTool {
                    runs: Arc::clone(&runs),
                    gate_key: None,
                    gate_open: AtomicBool::new(false),
                    started: None,
                    release: None,
                }),
            ),
        ])]
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    journaled.notified().await;

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("divergence did not settle")
    .expect("replay settled");

    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("different path")),
        "{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("swallowed")),
        "{results:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no nested call ran twice");
}

struct ConcurrentDivergenceSandbox {
    attempts: AtomicUsize,
    journaled: Arc<tokio::sync::Notify>,
    crash: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for ConcurrentDivergenceSandbox {
    fn fingerprint(&self) -> String {
        "concurrent-divergence-1".to_string()
    }

    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            host.call_tool("inner", JsonObject::new(), "c1").await?;
            self.journaled.notify_one();
            self.crash.notified().await;
            return Ok(CodemodeOutcome::output("unreachable"));
        }
        let different = serde_json::json!({ "different": true });
        let bad = host.call_tool(
            "inner",
            different.as_object().cloned().unwrap_or_default(),
            "c1",
        );
        let new = host.call_tool("inner", JsonObject::new(), "c2");
        let (bad, new) = tokio::join!(bad, new);
        bad?;
        new?;
        Ok(CodemodeOutcome::output("unreachable"))
    }
}

/// A divergence in one concurrent call wakes every other waiter instead of
/// leaving it blocked behind a journal that will never advance.
#[tokio::test]
async fn a_divergence_releases_concurrent_waiters() {
    let runs = Arc::new(AtomicUsize::new(0));
    let journaled = Arc::new(tokio::sync::Notify::new());
    let crash = Arc::new(tokio::sync::Notify::new());
    let sandbox = Arc::new(ConcurrentDivergenceSandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        crash,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "inner",
                Arc::new(ReplayTool {
                    runs: Arc::clone(&runs),
                    gate_key: None,
                    gate_open: AtomicBool::new(false),
                    started: None,
                    release: None,
                }),
            ),
        ])]
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    journaled.notified().await;

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("concurrent divergence deadlocked")
    .expect("replay settled");

    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("different path")),
        "{results:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no nested call ran twice");
}

struct FirstRunHoldTool {
    runs: Arc<AtomicUsize>,
    hold: Arc<tokio::sync::Notify>,
    first_run: Arc<AtomicBool>,
}

#[async_trait]
impl ToolExecute for FirstRunHoldTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.first_run.load(Ordering::SeqCst)
            && (api.call_id().ends_with("/c2") || api.call_id().ends_with("/c3"))
        {
            self.hold.notified().await;
        }
        Ok(ToolExecutionResult {
            structured_output: Some(serde_json::json!({ "ok": true })),
            ..Default::default()
        })
    }
}

struct IdleBoundarySandbox {
    attempts: AtomicUsize,
    journaled: Arc<tokio::sync::Notify>,
    crash: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CodemodeSandbox for IdleBoundarySandbox {
    fn fingerprint(&self) -> String {
        "idle-boundary-1".to_string()
    }

    async fn run(
        &self,
        _start: CodemodeStart,
        host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            host.call_tool("inner", JsonObject::new(), "c1").await?;
            let c2 = host.call_tool("inner", JsonObject::new(), "c2");
            tokio::pin!(c2);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut c2).await;
            host.idle(1, 2).await?;
            let c3 = host.call_tool("inner", JsonObject::new(), "c3");
            tokio::pin!(c3);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut c3).await;
            self.journaled.notify_one();
            self.crash.notified().await;
            return Ok(CodemodeOutcome::output("unreachable"));
        }
        host.call_tool("inner", JsonObject::new(), "c1").await?;
        let c2 = host.call_tool("inner", JsonObject::new(), "c2");
        tokio::pin!(c2);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut c2).await;
        host.idle(1, 2).await?;
        c2.await?;
        host.call_tool("inner", JsonObject::new(), "c3").await?;
        Ok(CodemodeOutcome::output("done"))
    }
}

/// An idle report is the authoritative count for its boundary; later
/// acknowledgements from newly issued calls must not overwrite it and create a
/// false replay divergence.
#[tokio::test]
async fn idle_counts_are_authoritative_for_their_boundary() {
    let runs = Arc::new(AtomicUsize::new(0));
    let first_run = Arc::new(AtomicBool::new(true));
    let hold = Arc::new(tokio::sync::Notify::new());
    let journaled = Arc::new(tokio::sync::Notify::new());
    let crash = Arc::new(tokio::sync::Notify::new());
    let sandbox = Arc::new(IdleBoundarySandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        crash,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "inner",
                Arc::new(FirstRunHoldTool {
                    runs: Arc::clone(&runs),
                    hold: Arc::clone(&hold),
                    first_run: Arc::clone(&first_run),
                }),
            ),
        ])]
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    journaled.notified().await;

    let parent = parent_task(&opened.harness).await;
    let journal = journal(&opened.harness, parent).await;
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].key, "c1");
    assert_eq!(journal.entries[0].issued_after, Some(2));
    first_run.store(false, Ordering::SeqCst);

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("replay did not settle")
    .expect("replay settled");

    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("done")),
        "{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("different path")),
        "{results:?}"
    );
    assert!(
        runs.load(Ordering::SeqCst) >= 3,
        "c1 and the replayed unrecorded calls must run"
    );
}
