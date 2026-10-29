//! Durable code mode: a tool that runs a host-provided deterministic sandbox
//! whose nested tool calls survive a restart (port of the durable replay
//! contract in `packages/durable/src/codemode/codemode.ts` @ `4a080252f`).
//!
//! The sandbox is the host's: rpi supplies the durable half. A run stores its
//! start data in `pi.codemode.run` before the sandbox starts and its outcome
//! when it settles. Each nested tool result is recorded in
//! `pi.codemode.journal`, keyed by the call key, before the script receives it;
//! a rerun hands the stored results back instead of executing the call again.
//! The tool is replay-safe, so the task runtime reruns it after a crash.

use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use rpi_ai::types::{Tool, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::chord::context::Context;
use crate::chord::json::{JsonObject, JsonValue};
use crate::documents::{DocDefinition, DocScope, DocToken, define_doc};
use crate::harness::types::{
    DiagnosticSeverity, NestedCallOptions, NestedToolExecutionResult, ToolCaller, ToolCommitFn,
    ToolDiagnostic, ToolExecute, ToolExecuteError, ToolExecutionApi, ToolExecutionResult,
    ToolRegistration, ToolReplay, read_state, write_state,
};
use crate::session::{DocOwner, Transaction};
use crate::tools::tool_error;

/// Version of the stored script contract; a different version refuses replay.
pub const CODEMODE_MODULE_VERSION: u64 = 2;
/// Name of the tool.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Start data of one script run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStart {
    /// Module version that produced it.
    pub version: u64,
    /// The script source.
    pub code: String,
    /// The sandbox's fingerprint at start.
    pub fingerprint: String,
}

/// State of one code mode call, retired with its task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeRunState {
    /// Written once before the sandbox starts; a rerun starts from exactly this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<CodemodeStart>,
    /// Written once when the call decides its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CodemodeOutcome>,
}

/// One journaled nested call result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournalEntry {
    /// Call key within this script run.
    pub key: String,
    /// Tool name.
    pub name: String,
    /// The nested call's result.
    pub result: NestedToolExecutionResult,
}

/// The replay journal of one code mode call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournal {
    /// Delivered results in delivery order.
    #[serde(default)]
    pub entries: Vec<CodemodeJournalEntry>,
}

impl CodemodeJournal {
    /// The stored result for `key`, if the call already ran it.
    pub fn get(&self, key: &str) -> Option<&NestedToolExecutionResult> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.result)
    }

    /// Record one result; an existing key keeps its first result.
    pub fn record(&mut self, key: String, name: String, result: NestedToolExecutionResult) {
        if self.get(&key).is_none() {
            self.entries
                .push(CodemodeJournalEntry { key, name, result });
        }
    }
}

fn empty_object(_seed: Option<&JsonValue>) -> JsonObject {
    JsonObject::new()
}

/// The task-scoped run document (`pi.codemode.run`).
pub static CODEMODE_RUN_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    define_doc(DocDefinition {
        kind: "pi.codemode.run".to_string(),
        version: 1,
        scope: DocScope::Task,
        history: None,
        fork: None,
        family: false,
        initial: Arc::new(empty_object),
        migrate: None,
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
    })
});

/// The task-scoped journal (`pi.codemode.journal`).
pub static CODEMODE_JOURNAL_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    define_doc(DocDefinition {
        kind: "pi.codemode.journal".to_string(),
        version: 1,
        scope: DocScope::Task,
        history: None,
        fork: None,
        family: false,
        initial: Arc::new(empty_object),
        migrate: None,
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
    })
});

/// What the sandbox may use: nested tool calls and the clock.
#[async_trait]
pub trait CodemodeHost: Send + Sync {
    /// Run `name` as a nested call keyed by `key`; a replay returns the stored
    /// result without executing it again.
    async fn call_tool(
        &self,
        name: &str,
        args: JsonObject,
        key: &str,
    ) -> Result<NestedToolExecutionResult, String>;
    /// The Harness clock.
    fn now(&self) -> Result<f64, String>;
}

/// Result of one sandbox run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeOutcome {
    /// Text the model receives.
    pub output: String,
    /// Value programs receive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<JsonValue>,
    /// Whether the script ended as an error.
    pub is_error: bool,
    /// Diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<ToolDiagnostic>,
}

impl CodemodeOutcome {
    /// A successful run with `output`.
    pub fn output(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Default::default()
        }
    }

    /// An error run with one error diagnostic.
    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            output: String::new(),
            structured_output: None,
            is_error: true,
            diagnostics: vec![ToolDiagnostic {
                severity: DiagnosticSeverity::Error,
                message,
                code: Some("codemode".to_string()),
            }],
        }
    }

    fn into_result(self) -> ToolExecutionResult {
        let output = if self.output.is_empty() {
            Vec::new()
        } else {
            vec![ToolResultContent::Text(rpi_ai::types::TextContent {
                text: self.output,
                text_signature: None,
            })]
        };
        ToolExecutionResult {
            output: Some(output),
            structured_output: self.structured_output,
            is_error: Some(self.is_error),
            diagnostics: Some(self.diagnostics),
            ..Default::default()
        }
    }
}

/// A deterministic sandbox: given the same start data, clock, and journaled
/// results in order, it makes the same calls with the same arguments.
#[async_trait]
pub trait CodemodeSandbox: Send + Sync {
    /// Identifies everything that turns stored results into script behavior; a
    /// changed fingerprint refuses replay.
    fn fingerprint(&self) -> String;
    /// Run the script from `start`.
    async fn run(
        &self,
        start: CodemodeStart,
        host: &dyn CodemodeHost,
        context: &Context,
    ) -> Result<CodemodeOutcome, String>;
}

/// Options of [`create_codemode_tool`].
pub struct CodemodeToolOptions {
    /// The host's deterministic sandbox.
    pub sandbox: Arc<dyn CodemodeSandbox>,
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "description": "JavaScript to run in the sandbox." }
        },
        "required": ["code"]
    })
}

struct CodemodeTool {
    sandbox: Arc<dyn CodemodeSandbox>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum CodemodeLookup {
    Finished(CodemodeOutcome),
    Start(CodemodeStart),
}

/// Host backed by one tool invocation and the run's journal.
struct DurableHost<'a> {
    api: &'a dyn ToolExecutionApi,
    journal: Mutex<CodemodeJournal>,
    task_id: crate::ids::TaskId,
    context: Context,
}

#[async_trait]
impl CodemodeHost for DurableHost<'_> {
    async fn call_tool(
        &self,
        name: &str,
        args: JsonObject,
        key: &str,
    ) -> Result<NestedToolExecutionResult, String> {
        if let Some(stored) = self
            .journal
            .lock()
            .map_err(|_| "codemode journal lock poisoned".to_string())?
            .get(key)
            .cloned()
        {
            self.context.telemetry().add_event(
                "pi.durable.replay",
                json!({ "tool.name": name })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            return Ok(stored);
        }
        let result = self
            .api
            .execute_tool(
                name,
                args,
                &self.context,
                NestedCallOptions {
                    key: Some(key.to_string()),
                    progress: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let task_id = self.task_id;
        let key_owned = key.to_string();
        let name_owned = name.to_string();
        let stored = result.clone();
        let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
            let key = key_owned.clone();
            let name = name_owned.clone();
            let stored = stored.clone();
            Box::pin(async move {
                let doc = tx
                    .doc(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task_id), None)
                    .await?;
                let mut state: CodemodeJournal = read_state(&doc)?;
                state.record(key, name, stored);
                write_state(&doc, &state)?;
                Ok(JsonValue::Null)
            })
        });
        self.api
            .commit(commit, &self.context)
            .await
            .map_err(|error| error.to_string())?;
        if let Ok(mut journal) = self.journal.lock() {
            journal.record(key.to_string(), name.to_string(), result.clone());
        }
        Ok(result)
    }

    fn now(&self) -> Result<f64, String> {
        self.api.now().map_err(|error| error.to_string())
    }
}

#[async_trait]
impl ToolExecute for CodemodeTool {
    async fn execute(
        &self,
        args: JsonObject,
        api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if code.trim().is_empty() {
            return Ok(ToolExecutionResult {
                output: Some(Vec::new()),
                is_error: Some(true),
                diagnostics: Some(vec![ToolDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    message: "code must not be empty".to_string(),
                    code: Some("invalid_arguments".to_string()),
                }]),
                ..Default::default()
            });
        }
        let task_id = api.task_id();
        let fingerprint = self.sandbox.fingerprint();
        // Read the run document and store the start before the sandbox starts.
        let lookup: CodemodeLookup = {
            let fingerprint = fingerprint.clone();
            let code = code.clone();
            let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
                let code = code.clone();
                let fingerprint = fingerprint.clone();
                Box::pin(async move {
                    let doc = tx
                        .doc(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), None)
                        .await?;
                    let mut state: CodemodeRunState = read_state(&doc)?;
                    if let Some(result) = state.result.clone() {
                        return Ok(serde_json::to_value(CodemodeLookup::Finished(result))
                            .unwrap_or(JsonValue::Null));
                    }
                    let start = match state.start.clone() {
                        Some(start) => start,
                        None => {
                            let start = CodemodeStart {
                                version: CODEMODE_MODULE_VERSION,
                                code,
                                fingerprint,
                            };
                            state.start = Some(start.clone());
                            write_state(&doc, &state)?;
                            start
                        }
                    };
                    Ok(serde_json::to_value(CodemodeLookup::Start(start))
                        .unwrap_or(JsonValue::Null))
                })
            });
            let value = api
                .commit(commit, context)
                .await
                .map_err(|error| tool_error(error.to_string()))?;
            serde_json::from_value::<CodemodeLookup>(value)
                .map_err(|error| tool_error(error.to_string()))?
        };
        let start = match lookup {
            CodemodeLookup::Finished(outcome) => return Ok(outcome.into_result()),
            CodemodeLookup::Start(start) => start,
        };
        if start.version != CODEMODE_MODULE_VERSION || start.fingerprint != fingerprint {
            let outcome = CodemodeOutcome::error(
                "The code mode sandbox or module changed; refusing to replay",
            );
            self.store_result(api, task_id, &outcome, context).await?;
            return Ok(outcome.into_result());
        }
        // The journal, read once; every delivered result is persisted before
        // the script sees it.
        let journal = {
            let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
                Box::pin(async move {
                    let doc = tx
                        .doc(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task_id), None)
                        .await?;
                    let state: CodemodeJournal = read_state(&doc)?;
                    Ok(serde_json::to_value(state).unwrap_or(JsonValue::Null))
                })
            });
            let value = api
                .commit(commit, context)
                .await
                .map_err(|error| tool_error(error.to_string()))?;
            serde_json::from_value::<CodemodeJournal>(value)
                .map_err(|error| tool_error(error.to_string()))?
        };
        let host = DurableHost {
            api,
            journal: Mutex::new(journal),
            task_id,
            context: context.clone(),
        };
        let outcome = match self.sandbox.run(start, &host, context).await {
            Ok(outcome) => outcome,
            Err(message) => CodemodeOutcome::error(message),
        };
        self.store_result(api, task_id, &outcome, context).await?;
        Ok(outcome.into_result())
    }
}

impl CodemodeTool {
    async fn store_result(
        &self,
        api: &dyn ToolExecutionApi,
        task_id: crate::ids::TaskId,
        outcome: &CodemodeOutcome,
        context: &Context,
    ) -> Result<(), ToolExecuteError> {
        let outcome = outcome.clone();
        let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
            let outcome = outcome.clone();
            Box::pin(async move {
                let doc = tx
                    .doc(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), None)
                    .await?;
                let mut state: CodemodeRunState = read_state(&doc)?;
                state.result = Some(outcome);
                write_state(&doc, &state)?;
                Ok(JsonValue::Null)
            })
        });
        api.commit(commit, context)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        Ok(())
    }
}

/// `createCodemodeTool`.
pub fn create_codemode_tool(options: CodemodeToolOptions) -> ToolRegistration {
    ToolRegistration {
        tool: Tool {
            name: CODEMODE_TOOL_NAME.to_string(),
            description: "# Code mode\n\nRuns a script in a deterministic sandbox. Nested tool calls are durable and replay-safe.".to_string(),
            parameters: parameters(),
            constrained_sampling: None,
        },
        replay: ToolReplay::Safe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: None,
        callers: Some(vec![ToolCaller::Model]),
        deferred: false,
        execute: Arc::new(CodemodeTool {
            sandbox: options.sandbox,
        }),
    }
}
