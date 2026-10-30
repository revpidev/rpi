//! Durable code mode: a tool that runs a host-provided deterministic sandbox
//! whose nested tool calls survive a restart (port of
//! `packages/durable/src/codemode/codemode.ts` @ `4a080252f`).
//!
//! The sandbox is the host's: rpi supplies the durable half. A run stores its
//! start data in `pi.codemode.run` before the sandbox starts, the host's start
//! reply, and its outcome when it settles. Each nested tool result is recorded
//! in `pi.codemode.journal`, keyed by the call key, before the script receives
//! it; a rerun hands the stored results back instead of executing the call
//! again. The tool is replay-safe, so the task runtime reruns it after a crash.
//!
//! The script's `store()`/`load()` values live in `pi.codemode.store`, a
//! conversation-scoped rewindable document initialized as of a fork. The
//! sandbox's discovery globals (`searchTools()`, `ALL_TOOLS`, ...) answer from
//! the run's start data, so a replay sees what the first run saw. The `models.*`
//! catalog globals read [`rpi_ai::models::Models`]; the paid classifier and
//! image calls are hidden internal tools.

pub mod describe;
pub mod models;
pub mod output;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use rpi_ai::types::{Tool, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::chord::context::Context;
use crate::chord::json::{JsonObject, JsonValue};
use crate::documents::{DocDefinition, DocScope, DocToken, define_doc};
use crate::harness::define::{define_extension, section};
use crate::harness::types::{
    Agent, DiagnosticSeverity, Extension, NestedCallOptions, NestedToolExecutionResult,
    PromptRender, ToolCaller, ToolCommitFn, ToolDiagnostic, ToolExecute, ToolExecuteError,
    ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay, read_state, write_state,
};
use crate::ids::{ConversationId, TaskId};
use crate::session::{DocOwner, Transaction};
use crate::tools::tool_error;
use crate::types::{DocumentFork, DocumentHistory};

pub use describe::{
    CodemodeCatalog, CodemodeDeclaration, CodemodeGlobalInfo, CodemodeNamespace,
    CodemodeNamespaceInfo, CodemodeToolInfo, DEFAULT_CODEMODE_INLINE_BUDGET, DEFAULT_SEARCH_LIMIT,
    codemode_description, codemode_section, declaration_of, namespace_of, sample_of,
    schema_to_type, to_codemode_identifier,
};
pub use models::{
    CATALOG_GLOBALS, CLASSIFY_TOOL, CODEMODE_INTERNAL_PREFIX, GENERATE_IMAGES_TOOL,
    MAX_CONCURRENT_MODEL_CALLS, MODEL_CALLS, MODELS_DESCRIPTION, catalog_call, create_model_tools,
    model_call_arguments, model_tool_for,
};
pub use output::{
    CodemodeCallStatus, CodemodeNestedCall, CodemodeToolDetails, DEFAULT_MAX_OUTPUT_TOKENS,
    EnvOutputFile, FormattedOutput, OutputFileWriter, OutputLimits, format_outcome,
    format_with_limits,
};

/// Version of the stored script contract; a different version refuses replay.
pub const CODEMODE_MODULE_VERSION: u64 = 2;
/// Name of the tool.
pub const CODEMODE_TOOL_NAME: &str = "codemode";
/// Globals for finding tools, answered from start data.
pub const DISCOVERY_GLOBALS: [&str; 3] = ["searchTools", "describeTool", "describeNamespace"];

fn default_max_output_tokens() -> u64 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

fn default_store() -> JsonValue {
    Value::Object(JsonObject::new())
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Start data of one script run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStart {
    /// Module version that produced it.
    #[serde(default = "default_module_version")]
    pub version: u64,
    /// The script source, without the options line.
    #[serde(default)]
    pub code: String,
    /// The Harness clock when the run started.
    #[serde(default)]
    pub started_at: f64,
    /// The sandbox's deterministic seed.
    #[serde(default)]
    pub seed: u64,
    /// The sandbox's fingerprint at start.
    #[serde(default)]
    pub fingerprint: String,
    /// `started_at + timeout_ms` when the script set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<f64>,
    /// `max_output_tokens` of the script's options line, or the default.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    /// The conversation's `store()` document at start.
    #[serde(default = "default_store")]
    pub store: JsonValue,
    /// Tools the script can call.
    #[serde(default)]
    pub tools: Vec<CodemodeToolInfo>,
    /// Namespaces of those tools.
    #[serde(default)]
    pub namespaces: Vec<CodemodeNamespaceInfo>,
    /// Globals the sandbox exposes.
    #[serde(default)]
    pub globals: Vec<CodemodeGlobalInfo>,
}

fn default_module_version() -> u64 {
    CODEMODE_MODULE_VERSION
}

impl Default for CodemodeStart {
    fn default() -> Self {
        Self {
            version: CODEMODE_MODULE_VERSION,
            code: String::new(),
            started_at: 0.0,
            seed: 0,
            fingerprint: String::new(),
            deadline: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            store: default_store(),
            tools: Vec::new(),
            namespaces: Vec::new(),
            globals: Vec::new(),
        }
    }
}

/// One text item a script emitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeTextItem {
    /// Text.
    pub text: String,
    /// A `console.log(...)` line rather than the script's output.
    #[serde(default, skip_serializing_if = "is_false")]
    pub console: bool,
}

/// One image a script emitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeImage {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type, such as `image/png`.
    pub mime_type: String,
}

/// Result of one sandbox run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeOutcome {
    /// Text the model receives; used when `text_items` is empty.
    #[serde(default)]
    pub output: String,
    /// Structured text items, in emission order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_items: Vec<CodemodeTextItem>,
    /// Images the script emitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<CodemodeImage>,
    /// Value programs receive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<JsonValue>,
    /// Whether the script ended as an error.
    #[serde(default)]
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
            ..Default::default()
        }
    }

    /// Add text items.
    pub fn with_text_items(mut self, text_items: Vec<CodemodeTextItem>) -> Self {
        self.text_items = text_items;
        self
    }

    /// Add images.
    pub fn with_images(mut self, images: Vec<CodemodeImage>) -> Self {
        self.images = images;
        self
    }

    /// A simple tool result without formatting, used when nothing ran.
    pub fn into_result(self) -> ToolExecutionResult {
        let mut output = Vec::new();
        if !self.output.is_empty() {
            output.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                text: self.output.clone(),
                text_signature: None,
            }));
        }
        for item in &self.text_items {
            output.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                text: item.text.clone(),
                text_signature: None,
            }));
        }
        if output.is_empty() && self.is_error {
            let message = self
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !message.is_empty() {
                output.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: message,
                    text_signature: None,
                }));
            }
        }
        for image in &self.images {
            output.push(ToolResultContent::Image(rpi_ai::types::ImageContent {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            }));
        }
        ToolExecutionResult {
            output: (!output.is_empty()).then_some(output),
            structured_output: None,
            is_error: Some(self.is_error),
            diagnostics: Some(self.diagnostics),
            ..Default::default()
        }
    }
}

/// A global's journaled call and outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeGlobalOutcome {
    /// Global name.
    pub name: String,
    /// Arguments as the script gave them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<JsonValue>,
    /// The resolved value.
    pub outcome: JsonValue,
}

/// One journaled nested call result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournalEntry {
    /// Call key within this script run.
    pub key: String,
    /// Tool or global name.
    pub name: String,
    /// Deterministic fingerprint of the arguments, so a replay can detect a
    /// different call under the same key without storing its content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_fingerprint: Option<String>,
    /// The VM clock while it processed this result.
    #[serde(default)]
    pub at: f64,
    /// Calls the VM had issued after processing this result; absent until
    /// acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_after: Option<u64>,
    /// A global's call and outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<CodemodeGlobalOutcome>,
    /// A tool's nested result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<NestedToolExecutionResult>,
}

impl Default for CodemodeJournalEntry {
    fn default() -> Self {
        Self {
            key: String::new(),
            name: String::new(),
            args_fingerprint: None,
            at: 0.0,
            issued_after: None,
            global: None,
            result: None,
        }
    }
}

/// Why a journal could not accept an entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodemodeJournalError {
    /// The same key names another tool, global, or arguments.
    #[error("call {key} was {expected}, but is now {actual}")]
    Divergence {
        /// Call key.
        key: String,
        /// What the journal recorded.
        expected: String,
        /// What the replay made.
        actual: String,
    },
}

/// The replay journal of one code mode call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeJournal {
    /// Number of calls issued before the first result was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_issued: Option<u64>,
    /// Delivered results in delivery order.
    #[serde(default)]
    pub entries: Vec<CodemodeJournalEntry>,
}

/// Deterministic, order-independent JSON text.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&object[key])
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
    }
}

/// Fingerprint of a call's arguments; the journal never stores their content.
pub fn args_fingerprint(args: &JsonObject) -> String {
    let canonical = canonical_json(&Value::Object(args.clone()));
    let digest = Sha256::digest(canonical.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl CodemodeJournal {
    /// The entry for `key`, if any.
    pub fn entry(&self, key: &str) -> Option<&CodemodeJournalEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// The stored tool result for `key`, if the call already ran it.
    pub fn get(&self, key: &str) -> Option<&NestedToolExecutionResult> {
        self.entry(key).and_then(|entry| entry.result.as_ref())
    }

    /// The stored global outcome for `key`.
    pub fn get_global(&self, key: &str) -> Option<&CodemodeGlobalOutcome> {
        self.entry(key).and_then(|entry| entry.global.as_ref())
    }

    /// Reject a call whose key names another tool, global, or arguments.
    pub fn validate(
        &self,
        key: &str,
        name: &str,
        args: &JsonObject,
    ) -> Result<(), CodemodeJournalError> {
        let Some(entry) = self.entry(key) else {
            return Ok(());
        };
        let fingerprint = args_fingerprint(args);
        let actual = format!("{name}({})", canonical_json(&Value::Object(args.clone())));
        if entry.name != name
            || entry
                .args_fingerprint
                .as_ref()
                .is_some_and(|recorded| recorded != &fingerprint)
        {
            return Err(CodemodeJournalError::Divergence {
                key: key.to_string(),
                expected: entry.name.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// Record one tool result; an existing key keeps its first result. A
    /// different tool or arguments under the same key diverges.
    pub fn record(
        &mut self,
        key: &str,
        name: &str,
        args: &JsonObject,
        at: f64,
        result: NestedToolExecutionResult,
    ) -> Result<(), CodemodeJournalError> {
        self.validate(key, name, args)?;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == key) {
            if entry.result.is_none() && entry.global.is_none() {
                entry.result = Some(result);
                entry.at = at;
            }
            return Ok(());
        }
        self.entries.push(CodemodeJournalEntry {
            key: key.to_string(),
            name: name.to_string(),
            args_fingerprint: Some(args_fingerprint(args)),
            at,
            issued_after: None,
            global: None,
            result: Some(result),
        });
        Ok(())
    }

    /// Record one global outcome.
    pub fn record_global(
        &mut self,
        key: &str,
        name: &str,
        args: &Value,
        at: f64,
        outcome: JsonValue,
    ) -> Result<(), CodemodeJournalError> {
        let args_object = args.as_object().cloned().unwrap_or_default();
        self.validate(key, name, &args_object)?;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == key) {
            if entry.result.is_none() && entry.global.is_none() {
                entry.global = Some(CodemodeGlobalOutcome {
                    name: name.to_string(),
                    args: Some(args.clone()),
                    outcome,
                });
                entry.at = at;
            }
            return Ok(());
        }
        self.entries.push(CodemodeJournalEntry {
            key: key.to_string(),
            name: name.to_string(),
            args_fingerprint: Some(args_fingerprint(&args_object)),
            at,
            issued_after: None,
            global: Some(CodemodeGlobalOutcome {
                name: name.to_string(),
                args: Some(args.clone()),
                outcome,
            }),
            result: None,
        });
        Ok(())
    }

    /// Record the acknowledged issued-call count of `key`.
    pub fn record_issued_after(&mut self, key: &str, issued: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == key) {
            entry.issued_after = Some(entry.issued_after.unwrap_or(0).max(issued));
        }
    }
}

/// State of one code mode call, retired with its task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeRunState {
    /// Written once before the sandbox starts; a rerun starts from exactly this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<CodemodeStart>,
    /// Reply of the host's `start()` hook; reused on replay. A stored JSON
    /// `null` is a present reply, not an absent one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub start_reply: Option<JsonValue>,
    /// Written once when the call decides its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolExecutionResult>,
}

/// What scripts keep with `store()` and read with `load()`, per conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStoreState {
    /// Stored values.
    #[serde(default)]
    pub values: JsonObject,
}

fn empty_object(_seed: Option<&JsonValue>) -> JsonObject {
    JsonObject::new()
}

/// Deserialize any present JSON value, including `null`, as `Some`. The field's
/// `default` handles an absent field; this keeps a persisted `null` reply
/// distinguishable from "no reply yet".
fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<JsonValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(JsonValue::deserialize(deserializer)?))
}

fn store_initial(_seed: Option<&JsonValue>) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("values".to_string(), Value::Object(JsonObject::new()));
    object
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

/// The conversation's `store()` document (`pi.codemode.store`).
pub static CODEMODE_STORE_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    define_doc(DocDefinition {
        kind: "pi.codemode.store".to_string(),
        version: 1,
        scope: DocScope::Conversation,
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
        family: false,
        initial: Arc::new(store_initial),
        migrate: None,
        checkpoint_when: Some(Arc::new(|_value, _ops, revision| revision >= 64)),
    })
});

/// What the sandbox may use: nested tool calls, the clock, the store, and the
/// discovery and model globals.
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

    /// The sandbox has processed result `seq` (`0`: its initial drain) and is
    /// idle after issuing `issued` calls. Replay validates and persists the
    /// acknowledged count here, then releases calls not present in the journal.
    async fn idle(&self, seq: u64, issued: u64) -> Result<(), String>;

    /// The host's reply to the script start; persisted with the start data and
    /// reused on replay. The default replies `null`.
    async fn start(&self, _start: &CodemodeStart) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
    }

    /// The start reply this attempt runs with.
    fn start_reply(&self) -> JsonValue {
        JsonValue::Null
    }

    /// The conversation's stored values at start, plus writes of this attempt.
    fn store(&self) -> JsonObject {
        JsonObject::new()
    }

    /// Read one stored value.
    async fn load(&self, _key: &str) -> Result<Option<JsonValue>, String> {
        Ok(None)
    }

    /// Write one stored value.
    async fn set(&self, _key: &str, _value: JsonValue) -> Result<(), String> {
        Err("the host has no code mode store".to_string())
    }

    /// `searchTools()`: BM25 over the callable tools.
    async fn search_tools(
        &self,
        _query: &str,
        _limit: usize,
        _namespace: Option<&str>,
    ) -> Result<Vec<JsonValue>, String> {
        Ok(Vec::new())
    }

    /// `describeTool()`.
    async fn describe_tool(&self, _name: &str) -> Result<Option<JsonValue>, String> {
        Ok(None)
    }

    /// `describeNamespace()`.
    async fn describe_namespace(&self, _name: &str) -> Result<Option<JsonValue>, String> {
        Ok(None)
    }

    /// `ALL_TOOLS`.
    async fn all_tools(&self) -> Result<Vec<JsonValue>, String> {
        Ok(Vec::new())
    }

    /// Run a global such as `models.getModelsOfType`; journaled by `key`.
    async fn call_global(
        &self,
        _key: &str,
        name: &str,
        _args: &JsonValue,
    ) -> Result<JsonValue, String> {
        Err(format!("Unknown global {name}"))
    }

    /// Tool calls this attempt made, for the result's details.
    fn calls(&self) -> Vec<CodemodeNestedCall> {
        Vec::new()
    }
}

/// A deterministic sandbox: given the same start data, clock, and journaled
/// results in order, it makes the same calls with the same arguments.
#[async_trait]
pub trait CodemodeSandbox: Send + Sync {
    /// Identifies everything that turns stored results into script behavior; a
    /// changed fingerprint refuses replay.
    fn fingerprint(&self) -> String;
    /// Run the script from `start`. Call [`CodemodeHost::idle`] whenever the VM
    /// has processed everything delivered and waits again; replay requires that
    /// signal to validate counts and admit new calls.
    async fn run(
        &self,
        start: CodemodeStart,
        host: &dyn CodemodeHost,
        context: &Context,
    ) -> Result<CodemodeOutcome, String>;
}

/// A global a host exposes beyond the built-in discovery and model globals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeGlobalSpec {
    /// Global name.
    pub name: String,
    /// Whether the sandbox exposes it as a spread namespace object.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spread: bool,
}

/// Options of [`create_codemode_tool`] and [`create_codemode_extension`].
pub struct CodemodeToolOptions {
    /// The host's deterministic sandbox.
    pub sandbox: Arc<dyn CodemodeSandbox>,
    /// Groups of tools by name prefix, for the `codemode` section and search.
    pub namespaces: Vec<CodemodeNamespace>,
    /// Estimated tokens the tool declarations in the section may use.
    pub inline_budget: usize,
    /// Give scripts `models.*`.
    pub models: bool,
    /// Host functions scripts call as globals.
    pub globals: Vec<CodemodeGlobalSpec>,
}

struct MissingSandbox;

#[async_trait]
impl CodemodeSandbox for MissingSandbox {
    fn fingerprint(&self) -> String {
        "missing".to_string()
    }
    async fn run(
        &self,
        _start: CodemodeStart,
        _host: &dyn CodemodeHost,
        _context: &Context,
    ) -> Result<CodemodeOutcome, String> {
        Err("no code mode sandbox configured".to_string())
    }
}

impl Default for CodemodeToolOptions {
    fn default() -> Self {
        Self {
            sandbox: Arc::new(MissingSandbox),
            namespaces: Vec::new(),
            inline_budget: DEFAULT_CODEMODE_INLINE_BUDGET,
            models: true,
            globals: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CodemodeSourceOptions {
    max_output_tokens: Option<u64>,
    timeout_ms: Option<f64>,
}

/// Parse the optional `// @options: {...}` first line.
fn parse_source(source: &str) -> (String, CodemodeSourceOptions) {
    let Some((first, rest)) = source.split_once('\n') else {
        return (source.to_string(), CodemodeSourceOptions::default());
    };
    let Some(raw) = first.trim_start().strip_prefix("// @options:") else {
        return (source.to_string(), CodemodeSourceOptions::default());
    };
    let Ok(value) = serde_json::from_str::<Value>(raw.trim()) else {
        return (source.to_string(), CodemodeSourceOptions::default());
    };
    let options = CodemodeSourceOptions {
        max_output_tokens: value.get("max_output_tokens").and_then(Value::as_u64),
        timeout_ms: value.get("timeout_ms").and_then(Value::as_f64),
    };
    (rest.to_string(), options)
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

fn is_internal(name: &str) -> bool {
    name.starts_with(CODEMODE_INTERNAL_PREFIX)
}

fn agent_tools(agent: &Agent) -> Vec<ToolRegistration> {
    let mut seen = BTreeSet::new();
    let mut tools = Vec::new();
    for tool in agent.callable.iter().chain(agent.deferred.iter()) {
        if seen.insert(tool.name().to_string()) {
            tools.push(tool.clone());
        }
    }
    tools
}

fn namespaces_of(
    tools: &[ToolRegistration],
    namespaces: &[CodemodeNamespace],
) -> Vec<CodemodeNamespaceInfo> {
    let mut built: Vec<CodemodeNamespaceInfo> = Vec::new();
    for tool in tools {
        let Some(namespace) = namespace_of(tool.name(), namespaces) else {
            continue;
        };
        let index = built
            .iter()
            .position(|candidate| candidate.name == namespace.name);
        let index = match index {
            Some(index) => index,
            None => {
                built.push(CodemodeNamespaceInfo {
                    name: namespace.name.clone(),
                    description: namespace.description.clone(),
                    instructions: namespace.instructions.clone(),
                    tools: Vec::new(),
                });
                built.len() - 1
            }
        };
        built[index].tools.push(tool.name().to_string());
    }
    built
}

fn globals_of(options_models: bool, globals: &[CodemodeGlobalSpec]) -> Vec<CodemodeGlobalInfo> {
    let mut built: Vec<CodemodeGlobalInfo> = DISCOVERY_GLOBALS
        .iter()
        .map(|name| CodemodeGlobalInfo {
            name: (*name).to_string(),
            spread: true,
        })
        .collect();
    if options_models {
        for name in CATALOG_GLOBALS {
            built.push(CodemodeGlobalInfo {
                name: name.to_string(),
                spread: true,
            });
        }
        for (name, _) in MODEL_CALLS {
            built.push(CodemodeGlobalInfo {
                name: name.to_string(),
                spread: true,
            });
        }
    }
    for global in globals {
        built.push(CodemodeGlobalInfo {
            name: global.name.clone(),
            spread: global.spread,
        });
    }
    built
}

// The start snapshot is assembled from independent run inputs; a builder would
// only add indirection.
#[allow(clippy::too_many_arguments)]
fn start_for(
    code: String,
    now: f64,
    seed: u64,
    fingerprint: String,
    options: &CodemodeSourceOptions,
    store: JsonObject,
    tools: Vec<CodemodeToolInfo>,
    namespaces: Vec<CodemodeNamespaceInfo>,
    globals: Vec<CodemodeGlobalInfo>,
) -> CodemodeStart {
    CodemodeStart {
        version: CODEMODE_MODULE_VERSION,
        code,
        started_at: now,
        seed,
        fingerprint,
        deadline: options
            .timeout_ms
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
            .map(|timeout| now + timeout),
        max_output_tokens: options
            .max_output_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        store: Value::Object(store),
        tools,
        namespaces,
        globals,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodemodeLookup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    done: Option<ToolExecutionResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<CodemodeStart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_reply: Option<JsonValue>,
    #[serde(default)]
    journal: CodemodeJournal,
    #[serde(default)]
    rerun: bool,
}

struct CodemodeTool {
    sandbox: Arc<dyn CodemodeSandbox>,
    namespaces: Vec<CodemodeNamespace>,
    models: bool,
    globals: Vec<CodemodeGlobalSpec>,
}

#[derive(Default)]
struct ReplayState {
    issued: u64,
    next: usize,
    delivered: BTreeSet<String>,
    pending: BTreeSet<String>,
    released: bool,
    divergence: Option<String>,
}

struct PendingCall<'a> {
    host: &'a DurableHost<'a>,
    key: &'a str,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if let Ok(mut replay) = self.host.replay.lock() {
            replay.pending.remove(self.key);
            self.host.replay_changed.notify_waiters();
        }
    }
}

/// Host backed by one tool invocation, the run's journal, and the store.
struct DurableHost<'a> {
    api: &'a dyn ToolExecutionApi,
    journal: Mutex<CodemodeJournal>,
    journal_commit: tokio::sync::Mutex<()>,
    replay: Mutex<ReplayState>,
    replay_changed: tokio::sync::Notify,
    replay_len: usize,
    rerun: bool,
    store: Mutex<JsonObject>,
    store_writes: Mutex<BTreeMap<String, JsonValue>>,
    task_id: TaskId,
    context: Context,
    start_reply: Mutex<JsonValue>,
    rows: Mutex<Vec<(String, CodemodeNestedCall)>>,
    catalog: CodemodeCatalog,
}

impl<'a> DurableHost<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        api: &'a dyn ToolExecutionApi,
        task_id: TaskId,
        context: &Context,
        journal: CodemodeJournal,
        store: JsonObject,
        start: &CodemodeStart,
        start_reply: Option<JsonValue>,
        rerun: bool,
    ) -> Self {
        let replay_len = journal.entries.len();
        Self {
            api,
            journal: Mutex::new(journal),
            journal_commit: tokio::sync::Mutex::new(()),
            replay: Mutex::new(ReplayState {
                released: !rerun,
                ..Default::default()
            }),
            replay_changed: tokio::sync::Notify::new(),
            replay_len,
            rerun,
            store: Mutex::new(store),
            store_writes: Mutex::new(BTreeMap::new()),
            task_id,
            context: context.clone(),
            start_reply: Mutex::new(start_reply.unwrap_or(JsonValue::Null)),
            rows: Mutex::new(Vec::new()),
            catalog: CodemodeCatalog::new(start.tools.clone(), start.namespaces.clone()),
        }
    }

    fn set_start_reply(&self, reply: JsonValue) {
        if let Ok(mut stored) = self.start_reply.lock() {
            *stored = reply;
        }
    }

    fn take_store_writes(&self) -> BTreeMap<String, JsonValue> {
        self.store_writes
            .lock()
            .map(|mut writes| std::mem::take(&mut *writes))
            .unwrap_or_default()
    }

    fn issued(&self) -> u64 {
        self.replay.lock().map(|replay| replay.issued).unwrap_or(0)
    }

    fn replay_divergence(&self, message: String) -> String {
        if let Ok(mut replay) = self.replay.lock()
            && replay.divergence.is_none()
        {
            replay.divergence = Some(message.clone());
        }
        self.replay_changed.notify_waiters();
        message
    }

    fn advance_call(&self, key: &str, counted: &mut bool) -> Result<bool, String> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| "codemode journal lock poisoned".to_string())?
            .clone();
        let entries = &journal.entries;
        let mut replay = self
            .replay
            .lock()
            .map_err(|_| "codemode replay lock poisoned".to_string())?;
        if let Some(message) = &replay.divergence {
            return Err(message.clone());
        }
        if !*counted {
            replay.issued += 1;
            *counted = true;
        }
        if replay.delivered.contains(key) {
            return Ok(true);
        }
        if let Some(index) = entries.iter().position(|entry| entry.key == key) {
            if replay.next != index {
                return Ok(false);
            }
            replay.next += 1;
            replay.delivered.insert(key.to_string());
            drop(replay);
            self.replay_changed.notify_waiters();
            return Ok(true);
        }
        if replay.next != self.replay_len || !replay.released {
            return Ok(false);
        }
        Ok(true)
    }

    async fn wait_for_call(&self, key: &str) -> Result<(), String> {
        let _pending = PendingCall { host: self, key };
        let mut counted = false;
        loop {
            let changed = self.replay_changed.notified();
            if self.advance_call(key, &mut counted)? {
                return Ok(());
            }
            self.replay
                .lock()
                .map_err(|_| "codemode replay lock poisoned".to_string())?
                .pending
                .insert(key.to_string());
            if let Some(signal) = self.context.abort_signal() {
                tokio::select! {
                    _ = changed => {},
                    _ = signal.cancelled() => {
                        return Err("The code mode replay was aborted".to_string());
                    }
                }
            } else {
                changed.await;
            }
        }
    }

    fn mark_delivered(&self, key: &str) {
        if let Ok(mut replay) = self.replay.lock() {
            replay.delivered.insert(key.to_string());
        }
    }

    async fn acknowledge_delivered(&self) -> Result<(), String> {
        let issued = self.issued();
        self.record_journal(|journal| {
            if let Some(last) = journal.entries.last_mut() {
                last.issued_after = Some(last.issued_after.unwrap_or(0).max(issued));
            } else {
                journal.first_issued = Some(journal.first_issued.unwrap_or(0).max(issued));
            }
            Ok::<(), String>(())
        })
        .await
    }

    async fn idle(&self, seq: u64, issued: u64) -> Result<(), String> {
        let seq = usize::try_from(seq)
            .map_err(|_| "The code mode idle sequence is too large".to_string())?;
        let prepared = (|| {
            let journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?
                .clone();
            let replay = self
                .replay
                .lock()
                .map_err(|_| "codemode replay lock poisoned".to_string())?;
            if let Some(message) = &replay.divergence {
                return Err(message.clone());
            }
            if issued != replay.issued {
                return Err(format!(
                    "The script took a different path when resumed after a restart: the sandbox reported {issued} issued calls, but the host observed {}",
                    replay.issued
                ));
            }
            if self.rerun && seq < replay.next {
                return Ok((false, false));
            }
            if self.rerun && seq != replay.next && seq <= self.replay_len {
                return Err(
                    "The script took a different path when resumed after a restart: the sandbox reported idle at an unexpected result"
                        .to_string(),
                );
            }
            if self.rerun && seq <= self.replay_len {
                let recorded = if seq == 0 {
                    journal.first_issued
                } else {
                    journal.entries[seq - 1].issued_after
                };
                if recorded.is_some_and(|expected| expected != issued) {
                    let message = if seq == 0 {
                        format!(
                            "The script took a different path when resumed after a restart: its first run issued {} calls, but it has now issued {issued}",
                            recorded.unwrap_or(0)
                        )
                    } else {
                        format!(
                            "The script took a different path when resumed after a restart: after call {} it had issued {} calls, but it has now issued {issued}",
                            journal.entries[seq - 1].key,
                            recorded.unwrap_or(0)
                        )
                    };
                    return Err(message);
                }
                if let Some(next) = journal.entries.get(replay.next)
                    && replay.next < self.replay_len
                    && !replay.pending.contains(&next.key)
                {
                    return Err(format!(
                        "The script took a different path when resumed after a restart: it waits without making call {}, which it made before",
                        next.key
                    ));
                }
            }
            let acknowledge = if seq == 0 {
                journal.first_issued.is_none()
            } else {
                journal
                    .entries
                    .get(seq - 1)
                    .is_some_and(|entry| entry.issued_after.is_none())
            };
            let release = self.rerun && replay.next == self.replay_len && seq == self.replay_len;
            Ok((acknowledge, release))
        })();
        let (acknowledge, release) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => return Err(self.replay_divergence(message)),
        };
        if acknowledge {
            self.record_journal(|journal| {
                if seq == 0 {
                    if journal.first_issued.is_none() {
                        journal.first_issued = Some(issued);
                    }
                } else if let Some(entry) = journal.entries.get_mut(seq - 1)
                    && entry.issued_after.is_none()
                {
                    entry.issued_after = Some(issued);
                }
                Ok::<(), String>(())
            })
            .await?;
        }
        if release && let Ok(mut replay) = self.replay.lock() {
            replay.released = true;
        }
        self.replay_changed.notify_waiters();
        Ok(())
    }

    async fn finish_replay(&self) -> Result<(), String> {
        let (issued, complete, released) = self
            .replay
            .lock()
            .map(|replay| {
                (
                    replay.issued,
                    replay.next == self.replay_len,
                    replay.released,
                )
            })
            .unwrap_or((0, false, false));
        if !complete {
            return Err(
                "The script took a different path when resumed after a restart: it ended before consuming the journal"
                    .to_string(),
            );
        }
        if self.rerun && !released {
            return Err(
                "The code mode sandbox ended without reporting idle after the replay".to_string(),
            );
        }
        self.record_journal(|journal| {
            if self.rerun {
                if journal
                    .first_issued
                    .is_some_and(|expected| expected > issued)
                {
                    return Err(format!(
                        "The script took a different path when resumed after a restart: its first run issued {} calls, but it has now issued {issued}",
                        journal.first_issued.unwrap_or(0)
                    ));
                }
                if let Some(entry) = journal
                    .entries
                    .iter()
                    .find(|entry| entry.issued_after.is_some_and(|expected| expected > issued))
                {
                    return Err(format!(
                        "The script took a different path when resumed after a restart: after call {} it had issued {} calls, but it has now issued {issued}",
                        entry.key,
                        entry.issued_after.unwrap_or(0)
                    ));
                }
            }
            if let Some(last) = journal.entries.last_mut()
                && last.issued_after.is_none()
            {
                last.issued_after = Some(issued);
            }
            Ok::<(), String>(())
        })
        .await
    }

    fn begin_row(&self, key: &str, name: &str, args: &JsonObject) -> u64 {
        if let Some(id) = self.rows.lock().ok().and_then(|rows| {
            rows.iter()
                .find(|(stored, _)| stored == key)
                .map(|(_, row)| row.id)
        }) {
            return id;
        }
        let id = self
            .rows
            .lock()
            .map(|rows| rows.len() as u64 + 1)
            .unwrap_or(1);
        let args = serde_json::to_string(args).unwrap_or_default();
        let row = CodemodeNestedCall {
            id,
            name: name
                .strip_prefix(CODEMODE_INTERNAL_PREFIX)
                .unwrap_or(name)
                .to_string(),
            args: cut(&args, 200),
            status: CodemodeCallStatus::Running,
            duration_ms: None,
            error: None,
        };
        if let Ok(mut rows) = self.rows.lock() {
            rows.push((key.to_string(), row));
        }
        id
    }

    fn finish_row(&self, key: &str, result: &NestedToolExecutionResult) {
        if let Ok(mut rows) = self.rows.lock()
            && let Some((_, row)) = rows.iter_mut().find(|(stored, _)| stored == key)
        {
            row.status = if result.is_error {
                CodemodeCallStatus::Error
            } else {
                CodemodeCallStatus::Ok
            };
            row.duration_ms = result.duration_ms;
            if result.is_error {
                let error = result
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        matches!(
                            diagnostic.severity,
                            crate::harness::types::DiagnosticSeverity::Error
                        )
                    })
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !error.is_empty() {
                    row.error = Some(cut(&error, 500));
                }
            }
        }
    }

    async fn persist_journal(&self, state: &CodemodeJournal) -> Result<(), String> {
        let state = state.clone();
        let task_id = self.task_id;
        let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
            let state = state.clone();
            Box::pin(async move {
                let doc = tx
                    .doc(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task_id), None)
                    .await?;
                write_state(&doc, &state)?;
                Ok(JsonValue::Null)
            })
        });
        self.api
            .commit(commit, &self.context)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn record_journal<F, E>(&self, record: F) -> Result<(), String>
    where
        F: FnOnce(&mut CodemodeJournal) -> Result<(), E>,
        E: std::fmt::Display,
    {
        let _guard = self.journal_commit.lock().await;
        let issued = self.issued();
        let state = {
            let mut journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?;
            record(&mut journal).map_err(|error| error.to_string())?;
            let len = journal.entries.len();
            if len > 1 && journal.entries[len - 2].issued_after.is_none() {
                journal.entries[len - 2].issued_after = Some(issued);
            }
            journal.clone()
        };
        self.persist_journal(&state).await
    }

    fn divergence(key: &str, name: &str, entry: &CodemodeJournalEntry) -> String {
        format!(
            "The script took a different path when resumed after a restart: call {key} was {}, but is now {name}",
            entry.name
        )
    }

    async fn discover(&self, name: &str, args: &Value) -> Result<JsonValue, String> {
        let list = args.as_array().cloned().unwrap_or_default();
        match name {
            "describeTool" => {
                let Some(Value::String(name)) = list.first() else {
                    return Err("describeTool() expects a tool name".to_string());
                };
                Ok(self.catalog.describe_tool(name).unwrap_or(Value::Null))
            }
            "describeNamespace" => {
                let Some(Value::String(name)) = list.first() else {
                    return Err("describeNamespace() expects a namespace name".to_string());
                };
                Ok(self.catalog.describe_namespace(name).unwrap_or(Value::Null))
            }
            "searchTools" => {
                let Some(Value::String(query)) = list.first() else {
                    return Err("searchTools() expects a query string".to_string());
                };
                let options = list.get(1).and_then(Value::as_object);
                let limit = match options.and_then(|options| options.get("limit")) {
                    None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
                    Some(value) => match value.as_u64() {
                        Some(limit) if limit > 0 => limit as usize,
                        _ => {
                            return Err(
                                "searchTools() limit must be a positive integer".to_string()
                            );
                        }
                    },
                };
                let namespace = match options.and_then(|options| options.get("namespace")) {
                    None | Some(Value::Null) => None,
                    Some(Value::String(namespace)) => Some(namespace.as_str()),
                    Some(_) => {
                        return Err("searchTools() namespace must be a string".to_string());
                    }
                };
                Ok(Value::Array(self.catalog.search(query, limit, namespace)))
            }
            other => Err(format!("Unknown discovery global {other}")),
        }
    }
}

#[async_trait]
impl CodemodeHost for DurableHost<'_> {
    async fn call_tool(
        &self,
        name: &str,
        args: JsonObject,
        key: &str,
    ) -> Result<NestedToolExecutionResult, String> {
        let fingerprint = args_fingerprint(&args);
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?;
            if let Some(entry) = journal.entry(key)
                && (entry.name != name
                    || entry.args_fingerprint.as_ref() != Some(&fingerprint)
                    || entry.global.is_some())
            {
                return Err(Self::divergence(key, name, entry));
            }
        }
        self.wait_for_call(key).await?;
        self.begin_row(key, name, &args);
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?;
            if let Some(entry) = journal.entry(key) {
                let stored = entry.result.clone().ok_or_else(|| {
                    format!("The code mode journal has no delivered result for call {key}")
                })?;
                self.finish_row(key, &stored);
                self.context.telemetry().add_event(
                    "pi.durable.replay",
                    json!({ "tool.name": name })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                );
                return Ok(stored);
            }
        }
        self.acknowledge_delivered().await?;
        let result = self
            .api
            .execute_tool(
                name,
                args.clone(),
                &self.context,
                NestedCallOptions {
                    key: Some(key.to_string()),
                    progress: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.finish_row(key, &result);
        let at = self.now()?;
        let stored = result.clone();
        self.record_journal(|journal| journal.record(key, name, &args, at, stored))
            .await?;
        self.acknowledge_delivered().await?;
        self.mark_delivered(key);
        Ok(result)
    }

    fn now(&self) -> Result<f64, String> {
        self.api.now().map_err(|error| error.to_string())
    }

    async fn idle(&self, seq: u64, issued: u64) -> Result<(), String> {
        DurableHost::idle(self, seq, issued).await
    }

    fn start_reply(&self) -> JsonValue {
        self.start_reply
            .lock()
            .map(|reply| reply.clone())
            .unwrap_or(JsonValue::Null)
    }

    fn store(&self) -> JsonObject {
        self.store
            .lock()
            .map(|store| store.clone())
            .unwrap_or_default()
    }

    async fn load(&self, key: &str) -> Result<Option<JsonValue>, String> {
        self.store
            .lock()
            .map(|store| store.get(key).cloned())
            .map_err(|_| "codemode store lock poisoned".to_string())
    }

    async fn set(&self, key: &str, value: JsonValue) -> Result<(), String> {
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| "codemode store lock poisoned".to_string())?;
            store.insert(key.to_string(), value.clone());
        }
        self.store_writes
            .lock()
            .map_err(|_| "codemode store lock poisoned".to_string())?
            .insert(key.to_string(), value);
        Ok(())
    }

    async fn search_tools(
        &self,
        query: &str,
        limit: usize,
        namespace: Option<&str>,
    ) -> Result<Vec<JsonValue>, String> {
        Ok(self.catalog.search(query, limit, namespace))
    }

    async fn describe_tool(&self, name: &str) -> Result<Option<JsonValue>, String> {
        Ok(self.catalog.describe_tool(name))
    }

    async fn describe_namespace(&self, name: &str) -> Result<Option<JsonValue>, String> {
        Ok(self.catalog.describe_namespace(name))
    }

    async fn all_tools(&self) -> Result<Vec<JsonValue>, String> {
        Ok(self.catalog.all_tools())
    }

    async fn call_global(
        &self,
        key: &str,
        name: &str,
        args: &JsonValue,
    ) -> Result<JsonValue, String> {
        // A paid model global is an internal tool: `call_tool` owns its journal
        // entry, including a stored result on replay.
        if let Some(tool) = model_tool_for(name) {
            let nested = model_call_arguments(args);
            let result = self.call_tool(tool, nested, key).await?;
            if result.is_error {
                let message = result
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(if message.is_empty() {
                    format!("{name} failed")
                } else {
                    message
                });
            }
            return Ok(result.structured_output.unwrap_or(JsonValue::Null));
        }
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?;
            if let Some(entry) = journal.entry(key) {
                let different_args = entry.global.as_ref().is_none_or(|global| {
                    global
                        .args
                        .as_ref()
                        .is_none_or(|stored| canonical_json(stored) != canonical_json(args))
                });
                if entry.name != name || entry.result.is_some() || different_args {
                    return Err(Self::divergence(key, name, entry));
                }
            }
        }
        self.wait_for_call(key).await?;
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| "codemode journal lock poisoned".to_string())?;
            if let Some(entry) = journal.entry(key) {
                return entry
                    .global
                    .as_ref()
                    .map(|global| global.outcome.clone())
                    .ok_or_else(|| {
                        format!(
                            "The code mode journal has no delivered global outcome for call {key}"
                        )
                    });
            }
        }
        self.acknowledge_delivered().await?;
        let outcome = if DISCOVERY_GLOBALS.contains(&name) {
            self.discover(name, args).await?
        } else if CATALOG_GLOBALS.contains(&name) {
            let models = self.api.models();
            catalog_call(name, args, &models).await?
        } else {
            return Err(format!("Unknown global {name}"));
        };
        let at = self.now()?;
        let outcome_for_record = outcome.clone();
        let name_owned = name.to_string();
        let args = args.clone();
        self.record_journal(move |journal| {
            journal.record_global(key, &name_owned, &args, at, outcome_for_record)
        })
        .await?;
        self.acknowledge_delivered().await?;
        self.mark_delivered(key);
        Ok(outcome)
    }

    fn calls(&self) -> Vec<CodemodeNestedCall> {
        let mut rows: Vec<CodemodeNestedCall> = self
            .rows
            .lock()
            .map(|rows| rows.iter().map(|(_, row)| row.clone()).collect())
            .unwrap_or_default();
        rows.sort_by_key(|row| row.id);
        for row in &mut rows {
            if row.status == CodemodeCallStatus::Running {
                row.status = CodemodeCallStatus::Cancelled;
            }
        }
        rows
    }
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let mut cut: String = text.chars().take(max.saturating_sub(3)).collect();
        cut.push_str("...");
        cut
    } else {
        text.to_string()
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
        if context
            .abort_signal()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Ok(CodemodeOutcome::error("Script aborted before it started").into_result());
        }
        let source = args
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if source.trim().is_empty() {
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
        let (code, source_options) = parse_source(&source);
        let agent = api
            .agent(context)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        let tools = agent_tools(&agent);
        let declared: Vec<CodemodeToolInfo> = tools.iter().map(CodemodeToolInfo::of).collect();
        let namespaces = namespaces_of(&tools, &self.namespaces);
        let globals = globals_of(self.models, &self.globals);
        let task_id = api.task_id();
        let conversation_id = api.conversation_id();
        let now = api.now().map_err(|error| tool_error(error.to_string()))?;
        let seed = task_id.as_number();
        let fingerprint = self.sandbox.fingerprint();
        let source_options_for_commit = source_options.clone();
        let lookup: CodemodeLookup = {
            let code = code.clone();
            let fingerprint = fingerprint.clone();
            let declared = declared.clone();
            let namespaces = namespaces.clone();
            let globals = globals.clone();
            let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
                let code = code.clone();
                let fingerprint = fingerprint.clone();
                let declared = declared.clone();
                let namespaces = namespaces.clone();
                let globals = globals.clone();
                let source_options = source_options_for_commit.clone();
                Box::pin(async move {
                    let doc = tx
                        .doc(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), None)
                        .await?;
                    let mut state: CodemodeRunState = read_state(&doc)?;
                    if let Some(result) = state.result.clone() {
                        return Ok(serde_json::to_value(CodemodeLookup {
                            done: Some(result),
                            start: None,
                            start_reply: None,
                            journal: CodemodeJournal::default(),
                            rerun: true,
                        })
                        .unwrap_or(JsonValue::Null));
                    }
                    let rerun = state.start.is_some();
                    let start = match state.start.clone() {
                        Some(start) => start,
                        None => {
                            let store_doc = tx
                                .doc(
                                    &CODEMODE_STORE_DOC,
                                    DocOwner::Conversation(conversation_id),
                                    None,
                                )
                                .await?;
                            let store: CodemodeStoreState = read_state(&store_doc)?;
                            let start = start_for(
                                code,
                                now,
                                seed,
                                fingerprint,
                                &source_options,
                                store.values,
                                declared,
                                namespaces,
                                globals,
                            );
                            state.start = Some(start.clone());
                            write_state(&doc, &state)?;
                            start
                        }
                    };
                    let journal_doc = tx
                        .doc(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task_id), None)
                        .await?;
                    let journal: CodemodeJournal = read_state(&journal_doc)?;
                    Ok(serde_json::to_value(CodemodeLookup {
                        done: None,
                        start: Some(start),
                        start_reply: state.start_reply.clone(),
                        journal,
                        rerun,
                    })
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
        if let Some(done) = lookup.done {
            return Ok(done);
        }
        let start = lookup
            .start
            .ok_or_else(|| tool_error("code mode run has no start data"))?;
        if start.version != CODEMODE_MODULE_VERSION || start.fingerprint != fingerprint {
            let outcome = CodemodeOutcome::error(
                "The code mode sandbox or module changed; refusing to replay",
            );
            let result = outcome.clone().into_result();
            self.store_result(
                api,
                task_id,
                conversation_id,
                &result,
                &BTreeMap::new(),
                context,
            )
            .await?;
            return Ok(result);
        }
        let remaining = start.deadline.map(|deadline| deadline - now);
        if remaining.is_some_and(|remaining| remaining <= 0.0) {
            let outcome = CodemodeOutcome::error(
                "Script timed out: its deadline passed while it was interrupted.",
            );
            let result = outcome.clone().into_result();
            self.store_result(
                api,
                task_id,
                conversation_id,
                &result,
                &BTreeMap::new(),
                context,
            )
            .await?;
            return Ok(result);
        }
        let store = match start.store.as_object() {
            Some(store) => store.clone(),
            None => JsonObject::new(),
        };
        let host = DurableHost::new(
            api,
            task_id,
            context,
            lookup.journal,
            store,
            &start,
            lookup.start_reply.clone(),
            lookup.rerun,
        );
        if lookup.start_reply.is_none() {
            let reply = host
                .start(&start)
                .await
                .map_err(|error| tool_error(error.to_string()))?;
            host.set_start_reply(reply.clone());
            self.store_start_reply(api, task_id, reply, context).await?;
        }
        let outcome = match self.sandbox.run(start.clone(), &host, context).await {
            Ok(outcome) => match host.finish_replay().await {
                Ok(()) => outcome,
                Err(message) => CodemodeOutcome::error(message),
            },
            Err(message) => CodemodeOutcome::error(message),
        };
        let wall_ms = api.now().map(|now| now - start.started_at).unwrap_or(0.0);
        let writer = api
            .env()
            .map(|env| EnvOutputFile::new(env, context.clone()));
        let formatted = format_outcome(
            &outcome,
            wall_ms,
            start.max_output_tokens,
            writer
                .as_ref()
                .map(|writer| writer as &dyn OutputFileWriter),
        )
        .await;
        let details = CodemodeToolDetails {
            calls: host.calls(),
            full_output_path: formatted.full_output_path.clone(),
            structured_output: outcome.structured_output.clone(),
        };
        let result = ToolExecutionResult {
            output: Some(formatted.content),
            is_error: Some(outcome.is_error),
            details: Some(
                serde_json::to_value(&details).unwrap_or_else(|_| json!({ "calls": [] })),
            ),
            diagnostics: Some(outcome.diagnostics.clone()),
            ..Default::default()
        };
        // As upstream: a failed script does not commit its store writes.
        let writes = if outcome.is_error {
            BTreeMap::new()
        } else {
            host.take_store_writes()
        };
        self.store_result(api, task_id, conversation_id, &result, &writes, context)
            .await?;
        Ok(result)
    }
}

impl CodemodeTool {
    async fn store_start_reply(
        &self,
        api: &dyn ToolExecutionApi,
        task_id: TaskId,
        reply: JsonValue,
        context: &Context,
    ) -> Result<(), ToolExecuteError> {
        let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
            let reply = reply.clone();
            Box::pin(async move {
                let doc = tx
                    .doc(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), None)
                    .await?;
                let mut state: CodemodeRunState = read_state(&doc)?;
                state.start_reply = Some(reply);
                write_state(&doc, &state)?;
                Ok(JsonValue::Null)
            })
        });
        api.commit(commit, context)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        Ok(())
    }

    async fn store_result(
        &self,
        api: &dyn ToolExecutionApi,
        task_id: TaskId,
        conversation_id: ConversationId,
        result: &ToolExecutionResult,
        writes: &BTreeMap<String, JsonValue>,
        context: &Context,
    ) -> Result<(), ToolExecuteError> {
        let result = result.clone();
        let writes = writes.clone();
        let commit: ToolCommitFn = Arc::new(move |tx: &mut Transaction| {
            let result = result.clone();
            let writes = writes.clone();
            Box::pin(async move {
                let doc = tx
                    .doc(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), None)
                    .await?;
                let mut state: CodemodeRunState = read_state(&doc)?;
                state.result = Some(result);
                write_state(&doc, &state)?;
                if !writes.is_empty() {
                    let store_doc = tx
                        .doc(
                            &CODEMODE_STORE_DOC,
                            DocOwner::Conversation(conversation_id),
                            None,
                        )
                        .await?;
                    let mut store: CodemodeStoreState = read_state(&store_doc)?;
                    for (key, value) in writes {
                        store.values.insert(key, value);
                    }
                    write_state(&store_doc, &store)?;
                }
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
            description: codemode_description(options.models),
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
            namespaces: options.namespaces,
            models: options.models,
            globals: options.globals,
        }),
    }
}

/// `createCodemodeExtension`: the code mode tool (and the internal model tools),
/// plus the `codemode` prompt section listing the tools scripts can call.
pub fn create_codemode_extension(options: CodemodeToolOptions) -> Extension {
    let namespaces = options.namespaces.clone();
    let inline_budget = options.inline_budget;
    let models = options.models;
    let mut tools = vec![create_codemode_tool(options)];
    if models {
        tools.extend(create_model_tools());
    }
    let render: PromptRender = Arc::new(move |input, _context| {
        let namespaces = namespaces.clone();
        let tools: Vec<ToolRegistration> = input
            .agent
            .callable
            .iter()
            .filter(|tool| !is_internal(tool.name()) && !tool.deferred)
            .cloned()
            .collect();
        Box::pin(async move { codemode_section(&tools, &namespaces, inline_budget) })
    });
    define_extension(Extension {
        name: "codemode".to_string(),
        tools,
        sections: vec![section("codemode", render, None)],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(value: i64) -> NestedToolExecutionResult {
        NestedToolExecutionResult {
            task_id: TaskId::from_number(1),
            structured_output: Some(json!({ "value": value })),
            is_error: false,
            details: None,
            diagnostics: Vec::new(),
            usage: None,
            duration_ms: None,
        }
    }

    #[test]
    fn journal_keeps_the_first_result_and_rejects_another_call() {
        let mut journal = CodemodeJournal::default();
        let args = JsonObject::new();
        journal
            .record("1", "inner", &args, 0.0, result(1))
            .expect("record");
        journal
            .record("1", "inner", &args, 0.0, result(2))
            .expect("same call");
        assert_eq!(
            journal
                .get("1")
                .and_then(|result| result.structured_output.clone()),
            Some(json!({ "value": 1 }))
        );
        let other = json!({ "value": 1 })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(
            journal
                .record("1", "inner", &other, 0.0, result(3))
                .is_err()
        );
        assert!(journal.record("1", "other", &args, 0.0, result(3)).is_err());
    }

    #[test]
    fn journal_round_trips_and_corrupt_entries_are_rejected() {
        let mut journal = CodemodeJournal::default();
        journal
            .record("c1", "inner", &JsonObject::new(), 12.0, result(1))
            .expect("record");
        let encoded = serde_json::to_value(&journal).expect("encode");
        let decoded: CodemodeJournal = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, journal);
        assert!(serde_json::from_value::<CodemodeJournal>(json!({ "entries": "wrong" })).is_err());
        assert_eq!(CodemodeJournal::default().entries.len(), 0);
    }

    #[test]
    fn source_options_are_parsed_and_stripped() {
        let (body, options) = parse_source(
            "// @options: {\"max_output_tokens\": 25, \"timeout_ms\": 1000}\nreturn 1",
        );
        assert_eq!(body, "return 1");
        assert_eq!(options.max_output_tokens, Some(25));
        assert_eq!(options.timeout_ms, Some(1000.0));
    }
}
