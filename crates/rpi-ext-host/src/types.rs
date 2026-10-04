//! Extension-facing types @ pi 0.82.1 (2efa728).
//!
//! Port of `packages/coding-agent/src/core/extensions/types.ts`:
//! - the 33 event names + event payload/result shapes (:509-1124)
//! - tool/command/shortcut/flag/renderer registration records
//!   (:404-492, :1126-1165, :1502-1523)
//! - `ExtensionError` (:1689-1694)
//!
//! Design rule (T15 W1): L0 (native) and L1 (wasm) share one capability
//! surface, so every event payload/result crosses the dispatch core as
//! camelCase JSON (coding-standards §4.4). The structs below are the typed
//! native wrapper over that JSON; field names match upstream
//! `docs/extensions.md` event docs field-for-field.
//!
//! Intentional differences:
//! - `signal: AbortSignal` fields (`session_before_compact`,
//!   `session_before_tree`) are not serializable and are dropped from the
//!   JSON shape; native handlers observe cancellation through the tool
//!   execution `CancellationToken` instead.
//! - Heavy upstream payloads that the host never interprets
//!   (`CompactionPreparation`, `SessionEntry[]`, `CompactionEntry`,
//!   `Model`, `AssistantMessageEvent`, `BashOperations`, `BashResult`,
//!   `BuildSystemPromptOptions`) are carried as [`serde_json::Value`].
//! - `ToolCallEvent` / `ToolResultEvent` unions (types.ts:847-967) collapse
//!   into single structs with `tool_name: String`; the per-tool narrowing
//!   is a TS type-level feature with no runtime semantics.

use std::collections::HashMap;
use std::sync::Arc;

use rpi_agent::messages::AgentMessage;
use rpi_agent::types::{AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use rpi_ai::types::{ImageContent, ProviderHeaders, Usage, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::api::{BoxFuture, ExtensionContext};
use crate::error::ExtError;

// ============================================================================
// Event names (types.ts `ExtensionEvent` union, :1027-1053)
// ============================================================================

pub const EVENT_PROJECT_TRUST: &str = "project_trust";
pub const EVENT_RESOURCES_DISCOVER: &str = "resources_discover";
pub const EVENT_SESSION_START: &str = "session_start";
pub const EVENT_SESSION_INFO_CHANGED: &str = "session_info_changed";
pub const EVENT_SESSION_BEFORE_SWITCH: &str = "session_before_switch";
pub const EVENT_SESSION_BEFORE_FORK: &str = "session_before_fork";
pub const EVENT_SESSION_BEFORE_COMPACT: &str = "session_before_compact";
pub const EVENT_SESSION_COMPACT: &str = "session_compact";
pub const EVENT_SESSION_COMPACT_FAILED: &str = "session_compact_failed";
pub const EVENT_SESSION_SHUTDOWN: &str = "session_shutdown";
pub const EVENT_MCP_SERVERS_CHANGE: &str = "mcp_servers_change";
pub const EVENT_SESSION_BEFORE_TREE: &str = "session_before_tree";
pub const EVENT_SESSION_TREE: &str = "session_tree";
pub const EVENT_CONTEXT: &str = "context";
pub const EVENT_CONTEXT_WITH_SYSTEM: &str = "context_with_system";
pub const EVENT_CACHE_WARMING_DECISION: &str = "cache_warming_decision";
pub const EVENT_BEFORE_PROVIDER_REQUEST: &str = "before_provider_request";
pub const EVENT_BEFORE_PROVIDER_HEADERS: &str = "before_provider_headers";
pub const EVENT_AFTER_PROVIDER_RESPONSE: &str = "after_provider_response";
pub const EVENT_PROVIDER_STREAM_EVENT: &str = "provider_stream_event";
pub const EVENT_BEFORE_AGENT_START: &str = "before_agent_start";
pub const EVENT_AGENT_START: &str = "agent_start";
pub const EVENT_AGENT_END: &str = "agent_end";
pub const EVENT_AGENT_BEFORE_SETTLE: &str = "agent_before_settle";
pub const EVENT_AGENT_SETTLED: &str = "agent_settled";
pub const EVENT_UI_PROMPT_START: &str = "ui_prompt_start";
pub const EVENT_UI_PROMPT_END: &str = "ui_prompt_end";
pub const EVENT_TURN_START: &str = "turn_start";
pub const EVENT_TURN_END: &str = "turn_end";
pub const EVENT_MESSAGE_START: &str = "message_start";
pub const EVENT_MESSAGE_UPDATE: &str = "message_update";
pub const EVENT_MESSAGE_END: &str = "message_end";
pub const EVENT_TOOL_EXECUTION_START: &str = "tool_execution_start";
pub const EVENT_TOOL_EXECUTION_UPDATE: &str = "tool_execution_update";
pub const EVENT_TOOL_EXECUTION_END: &str = "tool_execution_end";
pub const EVENT_MODEL_SELECT: &str = "model_select";
pub const EVENT_THINKING_LEVEL_SELECT: &str = "thinking_level_select";
pub const EVENT_USER_BASH: &str = "user_bash";
pub const EVENT_INPUT: &str = "input";
pub const EVENT_TOOL_CALL: &str = "tool_call";
pub const EVENT_TOOL_RESULT: &str = "tool_result";

/// All 41 event names, in the upstream `ExtensionAPI.on()` overload order
/// (types.ts:1545-1625 @ a13d35a74 + #9668 `cache_warming_decision` —
/// inserted between `context_with_system` and `before_provider_request`).
/// V16-03 added `context_with_system` and `agent_before_settle`; V16-06 added
/// the remaining two (`provider_stream_event` after `after_provider_response`,
/// `mcp_servers_change` after `session_shutdown`) and the version-level ABI
/// minor bump (V16-06 FR-F/FR-H).
pub const ALL_EVENTS: [&str; 41] = [
    EVENT_PROJECT_TRUST,
    EVENT_RESOURCES_DISCOVER,
    EVENT_SESSION_START,
    EVENT_SESSION_INFO_CHANGED,
    EVENT_SESSION_BEFORE_SWITCH,
    EVENT_SESSION_BEFORE_FORK,
    EVENT_SESSION_BEFORE_COMPACT,
    EVENT_SESSION_COMPACT,
    EVENT_SESSION_COMPACT_FAILED,
    EVENT_SESSION_SHUTDOWN,
    EVENT_MCP_SERVERS_CHANGE,
    EVENT_SESSION_BEFORE_TREE,
    EVENT_SESSION_TREE,
    EVENT_CONTEXT,
    EVENT_CONTEXT_WITH_SYSTEM,
    EVENT_CACHE_WARMING_DECISION,
    EVENT_BEFORE_PROVIDER_REQUEST,
    EVENT_BEFORE_PROVIDER_HEADERS,
    EVENT_AFTER_PROVIDER_RESPONSE,
    EVENT_PROVIDER_STREAM_EVENT,
    EVENT_BEFORE_AGENT_START,
    EVENT_AGENT_START,
    EVENT_AGENT_END,
    EVENT_AGENT_BEFORE_SETTLE,
    EVENT_AGENT_SETTLED,
    EVENT_UI_PROMPT_START,
    EVENT_UI_PROMPT_END,
    EVENT_TURN_START,
    EVENT_TURN_END,
    EVENT_MESSAGE_START,
    EVENT_MESSAGE_UPDATE,
    EVENT_MESSAGE_END,
    EVENT_TOOL_EXECUTION_START,
    EVENT_TOOL_EXECUTION_UPDATE,
    EVENT_TOOL_EXECUTION_END,
    EVENT_MODEL_SELECT,
    EVENT_THINKING_LEVEL_SELECT,
    EVENT_TOOL_CALL,
    EVENT_TOOL_RESULT,
    EVENT_USER_BASH,
    EVENT_INPUT,
];

/// `session_before_*` events carry a `{ cancel?: boolean }` result and
/// short-circuit on `cancel: true` (runner.ts:138-147, 779-786).
pub fn is_session_before_event(event_type: &str) -> bool {
    matches!(
        event_type,
        EVENT_SESSION_BEFORE_SWITCH
            | EVENT_SESSION_BEFORE_FORK
            | EVENT_SESSION_BEFORE_COMPACT
            | EVENT_SESSION_BEFORE_TREE
    )
}

// ============================================================================
// Shared enums
// ============================================================================

/// `ExtensionMode` (types.ts:304). Note: the upstream *event* type is
/// `"tui"`, not `"interactive"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtensionMode {
    #[serde(rename = "tui")]
    Tui,
    #[serde(rename = "rpc")]
    Rpc,
    #[serde(rename = "json")]
    Json,
    #[serde(rename = "print")]
    Print,
}

/// `SessionStartEvent["reason"]` (types.ts:559).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStartReason {
    #[serde(rename = "startup")]
    Startup,
    #[serde(rename = "reload")]
    Reload,
    #[serde(rename = "new")]
    New,
    #[serde(rename = "resume")]
    Resume,
    #[serde(rename = "fork")]
    Fork,
}

/// `SessionShutdownEvent["reason"]` (types.ts:612).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionShutdownReason {
    #[serde(rename = "quit")]
    Quit,
    #[serde(rename = "reload")]
    Reload,
    #[serde(rename = "new")]
    New,
    #[serde(rename = "resume")]
    Resume,
    #[serde(rename = "fork")]
    Fork,
}

/// `SessionBeforeSwitchEvent["reason"]` (types.ts:574).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSwitchReason {
    #[serde(rename = "new")]
    New,
    #[serde(rename = "resume")]
    Resume,
}

/// `SessionBeforeForkEvent["position"]` (types.ts:582).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkPosition {
    #[serde(rename = "before")]
    Before,
    #[serde(rename = "at")]
    At,
}

/// Compaction trigger reason shared by `session_before_compact` /
/// `session_compact` (types.ts:592, :603).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionReason {
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "threshold")]
    Threshold,
    #[serde(rename = "overflow")]
    Overflow,
}

/// `ResourcesDiscoverEvent["reason"]` (types.ts:541).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourcesDiscoverReason {
    #[serde(rename = "startup")]
    Startup,
    #[serde(rename = "reload")]
    Reload,
}

/// `ModelSelectSource` (types.ts:785).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSelectSource {
    #[serde(rename = "set")]
    Set,
    #[serde(rename = "cycle")]
    Cycle,
    #[serde(rename = "restore")]
    Restore,
}

/// `InputSource` (types.ts:822). `Print` appears in `docs/extensions.md`
/// input examples; the rpi-side seam (`rpi::core::extensions::InputSource`)
/// already carries all four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputSource {
    #[serde(rename = "interactive")]
    Interactive,
    #[serde(rename = "print")]
    Print,
    #[serde(rename = "rpc")]
    Rpc,
    #[serde(rename = "extension")]
    Extension,
}

/// `streamingBehavior` (types.ts:834).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

/// `ProjectTrustEventDecision` (types.ts:518).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectTrustDecision {
    #[serde(rename = "yes")]
    Yes,
    #[serde(rename = "no")]
    No,
    #[serde(rename = "undecided")]
    Undecided,
}

// ============================================================================
// Startup / resource events (types.ts:509-549)
// ============================================================================

/// `ProjectTrustEvent` (types.ts:513-516).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEvent {
    pub cwd: String,
}

/// `ProjectTrustEventResult` (types.ts:520-523).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEventResult {
    pub trusted: ProjectTrustDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
}

/// `ResourcesDiscoverEvent` (types.ts:538-542).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverEvent {
    pub cwd: String,
    pub reason: ResourcesDiscoverReason,
}

/// `ResourcesDiscoverResult` (types.ts:545-549).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_paths: Option<Vec<String>>,
}

// ============================================================================
// Session events (types.ts:551-657)
// ============================================================================

/// `SessionStartEvent` (types.ts:556-562).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartEvent {
    pub reason: SessionStartReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_session_file: Option<String>,
}

/// `SessionInfoChangedEvent` (types.ts:565-569). `None` = name cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoChangedEvent {
    pub name: Option<String>,
}

/// `SessionBeforeSwitchEvent` (types.ts:572-576).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchEvent {
    pub reason: SessionSwitchReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// `SessionBeforeSwitchResult` (types.ts:1097-1099).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
}

/// `SessionBeforeForkEvent` (types.ts:579-583).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkEvent {
    pub entry_id: String,
    pub position: ForkPosition,
}

/// `SessionBeforeForkResult` (types.ts:1101-1104).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_conversation_restore: Option<bool>,
}

/// `SessionBeforeCompactEvent` (types.ts:586-596). `signal` dropped (see
/// header); `preparation` / `branch_entries` carried as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactEvent {
    pub preparation: Value,
    pub branch_entries: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

/// `SessionBeforeCompactResult` (types.ts:1106-1109). `compaction` is the
/// upstream `CompactionResult` JSON (rpi-agent's `CompactionResult`
/// serializes to that shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Value>,
}

/// `SessionCompactEvent` (types.ts:599-607).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactEvent {
    pub compaction_entry: Value,
    pub from_extension: bool,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

/// `SessionCompactFailedEvent` (types.ts:619-631, exported from the package
/// entry by #9642).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactFailedEvent {
    pub reason: CompactionReason,
    /// Error text when compaction failed for a non-abort reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// True when compaction was cancelled or aborted.
    pub aborted: bool,
    /// True when the aborted turn would have been retried after this
    /// compaction (overflow recovery).
    pub will_retry: bool,
    /// True when the failing compaction content came from a
    /// `session_before_compact` handler.
    pub from_extension: bool,
}

/// `SessionShutdownEvent` (types.ts:610-615).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionShutdownEvent {
    pub reason: SessionShutdownReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// `TreePreparation` (types.ts:618-630). `entries_to_summarize` is a
/// `SessionEntry[]` upstream; carried as JSON here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreePreparation {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    pub common_ancestor_id: Option<String>,
    pub entries_to_summarize: Value,
    pub user_wants_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// `RegisteredMcpServer` (core/mcp-servers.ts:265) is carried as JSON: the shape is
// owned by the MCP server registration surface (V16-08).

/// `RegisteredMcpServer` (core/mcp-servers.ts:283): a server an extension
/// registered with `pi.registerMcpServer()`. `config` stays JSON here; the
/// MCP extension validates it into its typed config (V16-08).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredMcpServer {
    pub name: String,
    pub config: Value,
    pub extension_path: String,
}

/// Host→extension route callback for a registered virtual model (V16-12,
/// R3.11; `virtual-models.ts:84-102` + `loader.ts:493-507`).
///
/// The upstream `route(request, ctx)` is an in-process TS callback; the rpi
/// extension boundary is JSON, so the registration carries this callback
/// separately — created where the guest's dispatch handle lives (the wasm
/// L1 / native L0 host-call layer). It receives the `ModelRouteRequest` JSON
/// (`virtual-models.ts:52-70`) and resolves to the `ModelRoute` JSON
/// (`{model, thinkingLevel, state?}`; `virtual-models.ts:73-82`). The
/// `ExtensionContext` second argument is host-side only and does not cross
/// the boundary.
pub type VirtualModelRouteFn =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

/// `mcpNamespace` (mcp-servers.ts:133): `mcp__<server>` with `-` replaced.
fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// `McpServerRegistry` (core/mcp-servers.ts:290): servers registered by the
/// extensions of one runtime, in registration order. The registration
/// checks mirror `loader.ts:464-479`: ownership by the same extension is
/// allowed (replacement), another extension's name and `-`/`_` namespace
/// clashes are rejected.
#[derive(Default)]
pub struct McpServerRegistry {
    servers: Vec<RegisteredMcpServer>,
    change_listener: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl McpServerRegistry {
    /// `register` (mcp-servers.ts:296 + loader.ts:464-479).
    pub fn register(&mut self, server: RegisteredMcpServer) -> Result<(), String> {
        if let Some(existing) = self
            .servers
            .iter()
            .find(|existing| existing.name == server.name)
            && existing.extension_path != server.extension_path
        {
            return Err(format!(
                "MCP server \"{}\" is already registered by extension \"{}\"",
                server.name, existing.extension_path
            ));
        }
        if let Some(clash) = self.servers.iter().find(|existing| {
            existing.name != server.name
                && mcp_namespace(&existing.name) == mcp_namespace(&server.name)
        }) {
            return Err(format!(
                "MCP server \"{}\" conflicts with registered server \"{}\"",
                server.name, clash.name
            ));
        }
        match self
            .servers
            .iter_mut()
            .find(|existing| existing.name == server.name)
        {
            Some(existing) => *existing = server,
            None => self.servers.push(server),
        }
        self.notify();
        Ok(())
    }

    /// `unregister` (mcp-servers.ts:302): remove a server registered by
    /// `extension_path`.
    pub fn unregister(&mut self, name: &str, extension_path: &str) {
        let before = self.servers.len();
        self.servers
            .retain(|server| !(server.name == name && server.extension_path == extension_path));
        if self.servers.len() != before {
            self.notify();
        }
    }

    /// Remove every server owned by one extension (failed factory rollback).
    pub fn unregister_all_for(&mut self, extension_path: &str) {
        let before = self.servers.len();
        self.servers
            .retain(|server| server.extension_path != extension_path);
        if self.servers.len() != before {
            self.notify();
        }
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredMcpServer> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// `list` (mcp-servers.ts:314): copies in registration order.
    pub fn list(&self) -> Vec<RegisteredMcpServer> {
        self.servers.clone()
    }

    /// `setChangeListener` (mcp-servers.ts:320).
    pub fn set_change_listener(&mut self, listener: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.change_listener = listener;
    }

    fn notify(&self) {
        if let Some(listener) = &self.change_listener {
            listener();
        }
    }

    /// `RegisteredMcpServer[]` JSON (`getMcpServers`).
    pub fn to_json(&self) -> Vec<Value> {
        self.servers
            .iter()
            .map(|server| {
                serde_json::json!({
                    "name": server.name,
                    "config": server.config,
                    "extensionPath": server.extension_path,
                })
            })
            .collect()
    }
}

/// `McpServersChangeEvent` (types.ts:710-714 @ a13d35a74): fired when an
/// extension registers or unregisters an MCP server after the extensions are
/// bound. Registered while loading → read with `pi.getMcpServers()` on
/// `session_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersChangeEvent {
    /// Every registered server after the change (`RegisteredMcpServer[]`
    /// JSON).
    pub servers: Vec<Value>,
}

/// `SessionBeforeTreeEvent` (types.ts:633-637). `signal` dropped (see
/// header).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent {
    pub preparation: TreePreparation,
}

/// Extension-provided branch summary (`SessionBeforeTreeResult["summary"]`,
/// types.ts:1113-1117).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionBranchSummary {
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// `SessionBeforeTreeResult` (types.ts:1111-1124).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ExtensionBranchSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `SessionTreeEvent` (types.ts:640-646).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEvent {
    pub new_leaf_id: Option<String>,
    pub old_leaf_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_entry: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_extension: Option<bool>,
}

// ============================================================================
// Agent events (types.ts:659-779)
// ============================================================================

/// `ContextEvent` (types.ts:664-669 @ a13d35a74).
///
/// `messages` holds the conversation without system messages. The prompt
/// and tool state belong to pi: it restores them after each handler, so a
/// handler cannot drop them and does not need to preserve them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEvent {
    pub messages: Vec<AgentMessage>,
}

/// `ContextWithSystemEvent` (types.ts:671-677 @ a13d35a74): fired before
/// each LLM call, after every `context` handler has run and pi has restored
/// the prompt and tool state. `messages` is the full transcript including
/// system messages, and the result is sent as returned: the handler owns
/// the prompt and tool declarations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextWithSystemEvent {
    pub messages: Vec<AgentMessage>,
}

/// `ContextEventResult` (types.ts:1059-1061).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<AgentMessage>>,
}

/// `CacheWarmingAction` (cache-warmer.ts:88, #9668/c596d09d9): pi's
/// warm-or-stop decision; extensions may override it per refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheWarmingAction {
    #[serde(rename = "warm")]
    Warm,
    #[serde(rename = "stop")]
    Stop,
}

/// `CacheWarmingDecisionEvent` (cache-warmer.ts:112-116, #9668): fired
/// before each prompt-cache refresh with pi's decision filled in. The event
/// carries only the cost estimates; everything else an extension might want
/// (model, idle state, context size) is on the context (extensions.md
/// `cache_warming_decision`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingDecisionEvent {
    /// Price of this refresh: a cache read of the prompt plus one output token.
    pub warm_cost: f64,
    /// Extra price of the next real request if the cache entry is lost.
    pub miss_cost: f64,
    /// Estimated chance that a real request arrives before the entry expires.
    pub continuation_probability: f64,
    /// Pi's decision: `"warm"` when expected savings reach the threshold.
    pub action: CacheWarmingAction,
}

/// `CacheWarmingDecisionEventResult` (cache-warmer.ts:117-119): override
/// whether this refresh is sent; `"stop"` ends warming until the next real
/// request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingDecisionEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<CacheWarmingAction>,
}

/// `BeforeProviderRequestEvent` (types.ts:670-673). The handler *result* is
/// an arbitrary payload replacement (types.ts:1063): any non-undefined value
/// replaces the whole payload (runner.ts:1018-1020).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderRequestEvent {
    pub payload: Value,
}

/// `BeforeProviderHeadersEvent` (types.ts:680-683). Handlers mutate headers
/// in place; a `null` header value deletes it. See `runner.rs` for the JSON
/// core deviation (mutated headers are returned instead of mutated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderHeadersEvent {
    pub headers: ProviderHeaders,
}

/// `AfterProviderResponseEvent` (types.ts:686-690).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterProviderResponseEvent {
    pub status: u32,
    pub headers: HashMap<String, String>,
}

/// `ProviderStreamEvent` (types.ts:890-896 @ a13d35a74): a parsed provider
/// stream event before Pi normalizes it. Notification only — never
/// persisted; handlers run in stream order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStreamEvent {
    /// Provider id (`ProviderId` upstream).
    pub provider: String,
    /// API id (`Api` upstream).
    pub api: String,
    /// Model id.
    pub model: String,
    /// The raw parsed stream event (`unknown` upstream).
    pub data: Value,
}

/// `BeforeAgentStartEvent` (types.ts:693-703). `system_prompt_options` is
/// the upstream `BuildSystemPromptOptions` JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEvent {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    pub system_prompt: String,
    pub system_prompt_options: Value,
}

/// `BeforeAgentStartEventResult["message"]` — `Pick<CustomMessage,
/// "customType" | "content" | "display" | "details">` (types.ts:1092).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartMessage {
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<UserContent>,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// `BeforeAgentStartEventResult` (types.ts:1091-1095).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<BeforeAgentStartMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// Combined `before_agent_start` result across all handlers (runner.ts
/// `BeforeAgentStartCombinedResult`, :113-117).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartCombinedResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<BeforeAgentStartMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// `AgentEndEvent` (types.ts:711-714). `agent_start` / `agent_settled`
/// carry no fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEndEvent {
    pub messages: Vec<AgentMessage>,
}

/// `TurnStartEvent` (types.ts:722-726). `timestamp` is unix milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartEvent {
    pub turn_index: u32,
    pub timestamp: i64,
}

/// `AgentActivityOutcome` (types.ts:922 @ a13d35a74).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentActivityOutcome {
    Completed,
    Aborted,
    Error,
}

/// `SessionBoundaryDraft` (types.ts:924-957 @ a13d35a74): the structural
/// entries an `{entries, continue}` boundary result may append. Drafts cross
/// the JSON ABI untyped; the host parses this shape when applying them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionBoundaryDraft {
    /// `CustomEntryDraft` — display/state entry, not model context.
    Custom {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// `CustomMessageEntryDraft` — participates in model context.
    CustomMessage {
        custom_type: String,
        content: Value,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// `ContextEditEntryDraft` — `null` replacement omits the target.
    ContextEdit {
        target_id: String,
        replacement: Option<Value>,
    },
    /// `CompactionEntryDraft` — null `firstKeptEntryId` is the retain-none
    /// form (stores the compaction's own id).
    Compaction {
        summary: String,
        first_kept_entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

/// `BoundaryResult` (types.ts:975-978 @ a13d35a74): what a `turn_end` /
/// `agent_before_settle` handler may return. Entries stay raw JSON until the
/// host applies them (invalid previews are repaired, not silently dropped).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<Value>>,
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
}

/// `BoundaryDispatchResult` (runner.ts:214-219 @ a13d35a74). `valid` is
/// false when the final preview request failed: the caller then discards
/// the drafts and the continuation.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryDispatchResult {
    pub entries: Vec<Value>,
    pub continue_: bool,
    pub context: Value,
    pub valid: bool,
}

/// `TurnEndEvent` (types.ts:1015-1022 @ a13d35a74, `BoundaryState` included
/// at dispatch time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndEvent {
    pub turn_index: u32,
    pub message: AgentMessage,
    pub tool_results: Vec<rpi_ai::types::ToolResultMessage>,
    pub message_entry_id: String,
    pub tool_result_entry_ids: Vec<String>,
    pub outcome: AgentActivityOutcome,
}

/// `AgentBeforeSettleEvent` (types.ts:980-983 @ a13d35a74): fired before
/// final settlement. May append entries and ensure one next provider request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBeforeSettleEvent {
    pub outcome: AgentActivityOutcome,
}

/// `MessageStartEvent` / `MessageEndEvent` shared shape (types.ts:737-740,
/// :749-753).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    pub message: AgentMessage,
}

/// `MessageUpdateEvent` (types.ts:743-747). `assistant_message_event` is the
/// upstream `AssistantMessageEvent` JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdateEvent {
    pub message: AgentMessage,
    pub assistant_message_event: Value,
}

/// `ctx.sessionFile()` result (rpi additive host-call, ADR-0022): the
/// authoritative identity of the CURRENT session — `path` is `None` for
/// in-memory sessions. Replaces directory-heuristic transcript
/// derivation in consumers (rpi-statusline FR-I / TE-D34 §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFileInfo {
    pub path: Option<String>,
    pub id: String,
}

/// `ctx.sessionEntries` item (rpi additive host-call, ADR-0027): one
/// `type:"custom"` entry of the active branch, projected read-only from
/// the stored entry — `{id, parentId, timestamp, customType, data}`.
/// `data` is the entry's payload verbatim (`null` when the entry carries
/// none); message/tool_result conversation entries are never returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntryInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub custom_type: String,
    pub data: Value,
}

/// Host-side cap on the `limit` argument of `ctx.sessionEntries`
/// (ADR-0027: the host MAY bound `limit`; values above are truncated to
/// the cap — V14-25 §8.3 open question 1, resolved at 10_000, comfortably
/// ≥ the suggested 1000 while bounding per-call response size). Applies
/// to explicit limits only; the unfiltered result set is the branch's
/// custom entries, which are already bounded by the session itself.
/// ADR-0030 reuses this cap verbatim for `ctx.sessionToolResults`
/// (V15-14): same semantics, same constant — one cap for the session
/// read family.
pub const SESSION_ENTRIES_MAX_LIMIT: u64 = 10_000;

/// `ctx.sessionToolResults` item (rpi additive host-call, ADR-0030):
/// one `type:"message"` entry of the active branch whose
/// `message.role == "toolResult"` and whose `toolName` matched the
/// request exactly — `{id, parentId, timestamp, toolName, isError,
/// details}`. `details` is the tool's structured payload verbatim
/// (`null` when the result carries none); the `content` text blocks are
/// NEVER projected across the boundary (ADR-0030 decision 3 — the
/// guest-visible face is the named tool's details snapshot plus the
/// judging metadata only, not conversation text).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolResultInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub tool_name: String,
    pub is_error: bool,
    pub details: Value,
}

/// `MessageEndEventResult` (types.ts:1086-1089). The replacement must keep
/// the original role (enforced in runner.rs, runner.ts:837-844).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEndEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AgentMessage>,
}

/// `ToolExecutionStartEvent` (types.ts:1044-1050 @ a13d35a74).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStartEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    /// Set when another tool (for example a codemode script) made this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// `ToolExecutionUpdateEvent` (types.ts:1054-1061 @ a13d35a74).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdateEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub partial_result: Value,
    /// Set when another tool (for example a codemode script) made this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// `ToolExecutionEndEvent` (types.ts:1065-1072 @ a13d35a74).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEndEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Value,
    pub is_error: bool,
    /// Set when another tool (for example a codemode script) made this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

// ============================================================================
// Model events (types.ts:781-800)
// ============================================================================

/// `ModelSelectEvent` (types.ts:788-793). `model` / `previous_model` are the
/// upstream `Model` JSON (rpi-ai `Model` serializes to that shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelectEvent {
    pub model: Value,
    pub previous_model: Option<Value>,
    pub source: ModelSelectSource,
}

/// `ThinkingLevelSelectEvent` (types.ts:796-800). Levels are the upstream
/// `ThinkingLevel` strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelSelectEvent {
    pub level: String,
    pub previous_level: String,
}

// ============================================================================
// User bash events (types.ts:802-815)
// ============================================================================

/// `UserBashEvent` (types.ts:807-815).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBashEvent {
    pub command: String,
    pub exclude_from_context: bool,
    pub cwd: String,
}

/// `UserBashEventResult` (types.ts:1072-1077). `operations` /
/// `result` are the upstream `BashOperations` / `BashResult` JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBashEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

// ============================================================================
// Input events (types.ts:817-841)
// ============================================================================

/// `InputEvent` (types.ts:825-835).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputEvent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    pub source: InputSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming_behavior: Option<StreamingBehavior>,
}

/// `InputEventResult` (types.ts:838-841) — tagged on `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all_fields = "camelCase")]
pub enum InputEventResult {
    #[serde(rename = "continue")]
    Continue,
    #[serde(rename = "transform")]
    Transform {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    #[serde(rename = "handled")]
    Handled,
}

// ============================================================================
// Tool events (types.ts:843-967)
// ============================================================================

/// `ToolCallEvent` (types.ts:847-906; union collapsed, see header).
/// `input` is mutable in upstream handlers; the JSON core re-reads it from
/// each handler's mutated copy (W2 wires the mutation flow).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
    /// Set when another tool (for example a codemode script) issued this
    /// call. Nested ids are `<parent id>/<n>` and never appear as tool calls
    /// or tool results in the transcript (types.ts:1141-1151 @ a13d35a74).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// `ToolCallEventResult` (types.ts:1071-1080 @ 4181f66). `terminate` added
/// in #7715 (v0.84): hint to stop after the current tool batch when this call
/// is blocked; only effective when every finalized result in the batch sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

/// `ToolResultEvent` (types.ts:908-967; union collapsed, see header).
/// `content` is `(TextContent | ImageContent)[]` upstream; carried as JSON
/// blocks here. `details` shape depends on the tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
    pub content: Vec<Value>,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Machine-readable result for tools that declare an `outputSchema`.
    /// Handlers that redact `content` should also replace this; replacing
    /// `content` alone drops it (runner.ts:1185-1193 @ a13d35a74).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Set when another tool (for example a codemode script) issued this
    /// call (types.ts:1215-1230 @ a13d35a74).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// `ToolResultEventResult` (types.ts:1079-1084) — a partial patch; each
/// present field replaces the corresponding event field (runner.ts:878-893).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Replacing `content` without this field drops the structured content
    /// (types.ts:1425-1437 @ a13d35a74).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

// ============================================================================
// Tool registration (types.ts:404-507, wrapper shapes)
// ============================================================================

/// Declarative component tree returned by extension renderers.
///
/// Placeholder for the render protocol: both native (L0) render closures
/// and wasm (L1) guests produce this JSON component tree
/// ([`COMPONENT_TREE_SCHEMA_V1`], design §13 open item 3). W4 maps it onto
/// rpi-tui components (`rpi::modes::interactive::component_tree`).
pub type ComponentTree = Value;

/// ComponentTree wire schema v1 (T15 W4 freeze; design §13 open item 3).
///
/// Every node is `{"type": ..., "props": {...}, "children": [...]}`:
/// - `text`: `props.text` (string, required); style props `fg` / `bg`
///   (theme color names, e.g. `"accent"`, `"muted"`, `"border"`, or
///   `"#rrggbb"`), `bold` / `italic` / `underline` / `dim` (booleans);
///   `paddingX` / `paddingY` (uint, default 0); `truncate` (bool, default
///   false — TE11 FR-E.2 additive: clip oversized lines to the render
///   width, ANSI-preserving with `...`, instead of word-wrapping; a
///   truncating text renders as a single line).
/// - `spacer`: `props.lines` (uint, default 1).
/// - `box`: bordered container; props `paddingX` / `paddingY`, `borderColor`
///   (theme color name); `children` stacked vertically.
/// - `column`: unbordered vertical stack; `children`.
/// - `visualPreview` (additive v1 node, V16-09 @ 0582d9c11): props `text`
///   (string, may contain newlines; wraps at the render width), `fg` (theme
///   color name, applied per line), `maxVisualLines` (uint, default 5),
///   `keep` (`"start"` default | `"end"` — which visual lines survive
///   truncation), `hint` (string, `{hidden}` replaced by the skipped
///   visual-line count; rendered muted). Collapsed output is limited to
///   wrapped lines, so one long line such as minified JSON cannot fill the
///   screen.
///
/// v1 deviations from the design sketch (§13): `row` is deferred — rpi-tui
/// has no horizontal container; treat horizontal composition as out of
/// scope for v1. Unknown `type` values render as a text node containing the
/// JSON (fail-visible, not silent).
pub const COMPONENT_TREE_SCHEMA_V1: &str = r#"{
  "$id": "rpi.component-tree.v1",
  "node": {
    "type": {"enum": ["text", "spacer", "box", "column", "visualPreview"]},
    "props": {
      "text": "string (text only, required)",
      "fg": "theme color name | #rrggbb (text)",
      "bg": "theme color name | #rrggbb (text)",
      "bold|italic|underline|dim": "bool (text)",
      "truncate": "bool (text, default false — clip to render width instead of wrapping)",
      "paddingX|paddingY": "uint (text/box)",
      "lines": "uint (spacer, default 1)",
      "borderColor": "theme color name (box)"
    },
    "children": "node[] (box/column)"
  }
}"#;

/// Serializable subset of `ToolRenderContext` (types.ts:413-438). The
/// non-serializable fields (`invalidate`, `lastComponent`, `state`, theme)
/// are TUI-internal and land with the W4 render bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderContext {
    pub args: Value,
    pub tool_call_id: String,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
    /// Viewport width in terminal columns (rpi extension over upstream's
    /// types.ts:413-438, which carries no width): lets renderers size bars
    /// and columns to the actual terminal instead of a fixed assumption.
    /// Omitted on the wire when unknown (the renderer falls back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_width: Option<u64>,
}

/// `renderCall` placeholder (types.ts:483): returns a declarative
/// [`ComponentTree`] instead of a rpi-tui `Component` (see [`ComponentTree`]).
pub type RenderCallFn =
    Arc<dyn Fn(ToolRenderContext) -> Result<ComponentTree, String> + Send + Sync>;

/// `renderResult` placeholder (types.ts:486-491): result + options + context.
pub type RenderResultFn = Arc<
    dyn Fn(
            AgentToolResult,
            ToolRenderResultOptions,
            ToolRenderContext,
        ) -> Result<ComponentTree, String>
        + Send
        + Sync,
>;

/// `ToolRenderResultOptions` (types.ts:405-410).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderResultOptions {
    pub expanded: bool,
    pub is_partial: bool,
}

/// Input to an extension tool's `execute` (types.ts:474-480). `signal`
/// mirrors the upstream `AbortSignal`; `on_update` streams partial results.
pub struct ToolExecuteRequest {
    pub tool_call_id: String,
    pub params: Value,
    pub signal: CancellationToken,
    pub on_update: Option<AgentToolUpdateCallback>,
}

/// `ToolDefinition.execute` for native extensions. `Err` mirrors an upstream
/// throw (the agent runtime turns it into an error tool result).
pub type ToolExecuteFn = Arc<
    dyn Fn(
            ToolExecuteRequest,
            ExtensionContext,
        ) -> BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// `prepareArguments` shim (types.ts:462).
pub type PrepareArgumentsFn = Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>;

/// `ToolExposure` (types.ts:494-509 @ a13d35a74): how the model reaches a
/// tool. Default: `direct`.
///
/// `direct` and `model-only` tools are activated when registered; the others
/// are not. The active tool set (`getActiveTools`/`setActiveTools`) is the
/// set declared to the model; explicitly naming a `codemode`/`deferred` tool
/// activates it, `hidden` stays unreachable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolExposure {
    /// Declared to the model and callable.
    #[default]
    #[serde(rename = "direct")]
    Direct,
    /// Declared to the model but not callable (invocation is rejected).
    #[serde(rename = "model-only")]
    ModelOnly,
    /// Callable whenever registered, not declared unless explicitly activated.
    /// `codemode-deferred` is the v0.99.2 alias for this variant.
    #[serde(rename = "codemode", alias = "codemode-deferred")]
    Codemode,
    /// Like `codemode`, but codemode tools do not list it; tool search can.
    #[serde(rename = "deferred")]
    Deferred,
    /// Registered but unreachable; activating it has no effect (the only way
    /// to retract a tool).
    #[serde(rename = "hidden")]
    Hidden,
}

/// `ToolAnnotations` (types.ts:511-525 @ a13d35a74): MCP-style hints from
/// the tool author, not verified. Permission extensions may use them to
/// decide which calls to confirm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// The tool does not modify its environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may delete or overwrite data. Meaningful when not read-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeating a call with the same arguments has no further effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool reaches an open world of external entities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// `ToolNamespace` (types.ts:527-537 @ a13d35a74): a group of related tools,
/// such as the tools of one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNamespace {
    /// For example `mcp__docs`.
    pub name: String,
    /// Short summary shown with the group in model-facing listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer usage guidance (such as MCP server instructions); not part of
    /// tool listings, returned by `describeNamespace()`-style queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// One tool as [`ToolLoadout`] exposes it (`AgentTool[]` upstream): the
/// fields a `prepareLoadout` hook reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLoadoutEntry {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// `outputSchema` — the schema of `structuredContent` in successful
    /// results (rpi additive for `prepareLoadout` renderers such as
    /// codemode; upstream `AgentTool` carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// `ToolLoadout` (types.ts:539-550 @ a13d35a74): the tools of a session as a
/// `prepareLoadout` hook sees them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLoadout {
    /// Tools declared to the model (the active tools), in order, with their
    /// original descriptions.
    pub declared: Vec<ToolLoadoutEntry>,
    /// Tools callable through `ctx.executeTool()`.
    pub callable: Vec<ToolLoadoutEntry>,
    /// Every registered tool.
    pub registered: Vec<ToolLoadoutEntry>,
    /// Exposure by tool name (`getExposure` upstream).
    pub exposures: HashMap<String, ToolExposure>,
    /// Namespace by tool name (`getNamespace` upstream).
    pub namespaces: HashMap<String, ToolNamespace>,
}

impl ToolLoadout {
    /// `loadout.getExposure(name)`.
    pub fn get_exposure(&self, name: &str) -> ToolExposure {
        self.exposures
            .get(name)
            .copied()
            .unwrap_or(ToolExposure::Direct)
    }

    /// `loadout.getNamespace(name)`.
    pub fn get_namespace(&self, name: &str) -> Option<&ToolNamespace> {
        self.namespaces.get(name)
    }
}

/// `ToolLoadoutChanges` (types.ts:551-562 @ a13d35a74): what a
/// `prepareLoadout` hook changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLoadoutChanges {
    /// Model-facing descriptions of declared tools, by tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptions: Option<HashMap<String, String>>,
    /// Declared tools whose declarations requests leave out. They stay active
    /// and callable, and the transcript still declares them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_declarations: Option<Vec<String>>,
}

/// `ToolDefinition.prepareLoadout` (types.ts:613-615 @ a13d35a74): called
/// whenever the active tool set changes; returns replacement descriptions
/// and hidden declarations. A `Err` is reported like an upstream handler
/// throw (extension error, loadout keeps the previous values).
pub type PrepareLoadoutFn =
    Arc<dyn Fn(&ToolLoadout) -> Result<Option<ToolLoadoutChanges>, String> + Send + Sync>;

/// `ToolDefinition` (types.ts:443-492).
///
/// Render placeholder decision: `render_call` / `render_result` are typed
/// native closures returning [`ComponentTree`] JSON — the same declarative
/// component description wasm guests produce — rather than rpi-tui
/// component trait objects (see [`ComponentTree`] for the rationale).
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Option<Vec<String>>,
    /// JSON Schema (TypeBox upstream) for the tool parameters.
    pub parameters: Value,
    /// `false | ConstrainedSamplingConfig` upstream; `Some(Value::Bool(false))`
    /// explicitly disables, `None` leaves the provider default.
    pub constrained_sampling: Option<Value>,
    /// JSON Schema of `structuredContent` in successful results (types.ts:590).
    pub output_schema: Option<Value>,
    /// How the model reaches the tool (types.ts:595).
    pub exposure: ToolExposure,
    /// Group the tool belongs to, for example its MCP server (types.ts:598).
    pub namespace: Option<ToolNamespace>,
    /// Hints about what the tool does, for example from an MCP server
    /// (types.ts:601).
    pub annotations: Option<ToolAnnotations>,
    /// Whether registering the tool activates it (types.ts:605). `None`
    /// defaults to true for `direct`/`model-only`; other exposures are never
    /// activated on registration; `Some(false)` defers activation until the
    /// tool is named.
    pub default_active: Option<bool>,
    /// Adjust how the loadout is presented to the model while this tool is
    /// active (types.ts:615).
    pub prepare_loadout: Option<PrepareLoadoutFn>,
    /// `"default" | "self"` (types.ts:458).
    pub render_shell: Option<String>,
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    pub execution_mode: Option<ToolExecutionMode>,
    pub execute: ToolExecuteFn,
    pub render_call: Option<RenderCallFn>,
    pub render_result: Option<RenderResultFn>,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Message / entry rendering (types.ts:1126-1150)
// ============================================================================

/// `MessageRenderOptions` (types.ts:1130-1134).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRenderOptions {
    pub expanded: bool,
    pub output_pad: u32,
}

/// `EntryRenderOptions` (types.ts:1136-1138).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRenderOptions {
    pub expanded: bool,
}

/// `MessageRenderer` (types.ts:1140-1144). `message` is the `CustomMessage`
/// JSON; returns a [`ComponentTree`] or `None` to fall back to the default
/// renderer.
pub type MessageRenderFn =
    Arc<dyn Fn(Value, MessageRenderOptions) -> Result<Option<ComponentTree>, String> + Send + Sync>;

/// `EntryRenderer` (types.ts:1146-1150). `entry` is the `CustomEntry` JSON.
pub type EntryRenderFn =
    Arc<dyn Fn(Value, EntryRenderOptions) -> Result<Option<ComponentTree>, String> + Send + Sync>;

// ============================================================================
// Command / shortcut / flag registration (types.ts:1152-1165, :1502-1523)
// ============================================================================

/// `SourceScope` (source-info.ts:3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceScope {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "temporary")]
    Temporary,
}

/// `SourceOrigin` (source-info.ts:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceOrigin {
    #[serde(rename = "package")]
    Package,
    #[serde(rename = "top-level")]
    TopLevel,
}

/// `SourceInfo` (source-info.ts:6-12) — provenance of an extension-owned
/// item. Duplicated here (rather than reusing rpi's) because the dependency
/// direction is `rpi` → `rpi-ext-host`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtSourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl ExtSourceInfo {
    /// `createSyntheticSourceInfo` (source-info.ts:22-35).
    pub fn synthetic(path: &str, source: &str, base_dir: Option<String>) -> Self {
        ExtSourceInfo {
            path: path.to_owned(),
            source: source.to_owned(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir,
        }
    }
}

/// `RegisteredTool` (types.ts:1505-1508).
#[derive(Clone)]
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub source_info: ExtSourceInfo,
}

/// Command handler (`RegisteredCommand.handler`, types.ts:1161). Receives
/// the command context (session-control methods); the base context alone is
/// used for event handlers.
pub type CommandHandlerFn = Arc<
    dyn Fn(String, crate::api::ExtensionCommandContext) -> BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// `getArgumentCompletions` (types.ts:1160): returns autocomplete items
/// (JSON; the rpi-tui `AutocompleteItem` mapping lands in W4) or `None`.
pub type ArgumentCompletionsFn =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<Value>, String>> + Send + Sync>;

/// `RegisteredCommand` (types.ts:1156-1162).
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub source_info: ExtSourceInfo,
    pub description: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
    pub handler: CommandHandlerFn,
}

/// `ResolvedCommand` (types.ts:1164-1166) — after conflict resolution.
pub struct ResolvedCommand {
    pub name: String,
    pub invocation_name: String,
    pub source_info: ExtSourceInfo,
    pub description: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
    pub handler: CommandHandlerFn,
}

/// CLI flag value (`boolean | string`, types.ts:1258).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    Boolean(bool),
    String(String),
}

/// `ExtensionFlag["type"]` (types.ts:1513).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlagType {
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "string")]
    String,
}

/// `ExtensionFlag` (types.ts:1510-1516).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionFlag {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub flag_type: FlagType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<FlagValue>,
    pub extension_path: String,
}

/// Shortcut handler (`ExtensionShortcut.handler`, types.ts:1521).
pub type ShortcutHandlerFn =
    Arc<dyn Fn(ExtensionContext) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// `ExtensionShortcut` (types.ts:1518-1523). `shortcut` is a normalized
/// (lowercase) `KeyId`.
#[derive(Clone)]
pub struct ExtensionShortcut {
    pub shortcut: String,
    pub description: Option<String>,
    pub handler: ShortcutHandlerFn,
    pub extension_path: String,
}

impl std::fmt::Debug for ExtensionShortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionShortcut")
            .field("shortcut", &self.shortcut)
            .field("description", &self.description)
            .field("extension_path", &self.extension_path)
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Diagnostics + errors
// ============================================================================

/// `ResourceDiagnostic["type"]` subset used by the extension host
/// (diagnostics.ts); shortcuts use `warning`, prompt-style name collisions
/// use `collision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticKind {
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "collision")]
    Collision,
}

/// `ResourceDiagnostic` (diagnostics.ts) as produced by the extension host:
/// shortcut conflicts (runner.ts:495-499) and tool/flag conflicts
/// (resource-loader.ts:1003-1038).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDiagnostic {
    #[serde(rename = "type")]
    pub kind: DiagnosticKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `ExtensionError` (types.ts:1689-1694) — payload of `extension_error`
/// events and `onError` listeners. Rust handlers report only a message, so
/// `stack` is always `None` for native extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl ExtensionError {
    pub fn new(extension_path: &str, event: &str, error: String) -> Self {
        ExtensionError {
            extension_path: extension_path.to_owned(),
            event: event.to_owned(),
            error,
            stack: None,
        }
    }
}

/// `{ path, error }` load-failure record (`LoadExtensionsResult["errors"]`,
/// types.ts:1680).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionLoadError {
    pub path: String,
    pub error: String,
}

/// `{ path, warning }` load-warning record
/// (`LoadExtensionsResult["warnings"]`, resource-loader.ts:100-107) — the
/// #10174 built-in replacement notice and future package warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionLoadWarning {
    pub path: String,
    pub warning: String,
}

// ============================================================================
// Helpers
// ============================================================================

/// Build the JSON event object handed to the dispatch core: serialize
/// `payload` and stamp the upstream `type` tag onto it (every upstream event
/// object carries `type`, e.g. types.ts:514).
pub fn event_payload<T: Serialize>(event_type: &str, payload: &T) -> Result<Value, ExtError> {
    let mut value = serde_json::to_value(payload)?;
    stamp_event_type(&mut value, event_type);
    Ok(value)
}

/// Stamp `{"type": event_type}` onto a JSON object payload. Non-object
/// payloads (none today) pass through untouched.
pub fn stamp_event_type(value: &mut Value, event_type: &str) {
    if let Value::Object(map) = value {
        map.insert("type".to_owned(), Value::String(event_type.to_owned()));
    }
}

// ============================================================================
// v0.11 additions (types.ts @ 4181f66)
// ============================================================================

/// `MarkdownTransformContext` (types.ts:1147-1153 @ 4181f66).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownTransformContext {
    /// `"user" | "assistant" | "assistant-thinking"`
    pub message_type: String,
    pub is_streaming: bool,
    pub available_width: usize,
}

/// `MarkdownTransformer` (types.ts:1153 @ 4181f66): chained, width-aware
/// Markdown source transformer. Host dispatches as a `render` kind message;
/// the guest returns the transformed string. TUI rendering wiring is T29.
pub type MarkdownTransformerFn =
    Arc<dyn Fn(String, MarkdownTransformContext) -> String + Send + Sync>;

/// `ResolvedRequestAuth` (model-registry.ts:17-25 @ 4181f66) — return type
/// of `getApiKeyAndHeaders`. Upstream is a discriminated union on `ok: true |
/// false`; Rust carries it as a struct since serde's `tag` attribute cannot
/// emit boolean tag values. The wire shape is `{ ok: true, apiKey?, headers?,
/// baseUrl?, env? } | { ok: false, error: "..." }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRequestAuth {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ResolvedRequestAuth {
    pub fn ok(
        api_key: Option<String>,
        headers: Option<HashMap<String, Option<String>>>,
        base_url: Option<String>,
        env: Option<HashMap<String, String>>,
    ) -> Self {
        ResolvedRequestAuth {
            ok: true,
            api_key,
            headers,
            base_url,
            env,
            error: None,
        }
    }

    pub fn err(error: String) -> Self {
        ResolvedRequestAuth {
            ok: false,
            api_key: None,
            headers: None,
            base_url: None,
            env: None,
            error: Some(error),
        }
    }
}

/// `ScopedModel` (model-resolver.ts:63-66 @ 4181f66) — resolved model scope
/// entry exposed via `ctx.scopedModels`. Carried as JSON (`Model` + optional
/// `thinkingLevel`) because the host does not interpret the model body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedModel {
    pub model: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// `AuthOperationOptions` (packages/ai/src/auth/types.ts:46-48 @ 4181f66).
#[derive(Debug, Clone, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<CancellationToken>,
}

// ============================================================================
// TUI type placeholders (types.ts @ 4181f66; wiring deferred to T28/T32)
// ============================================================================

/// `TuiMode` (pi-tui tui.ts:284 @ 4181f66): `"regular" | "fullscreen"`.
/// Placeholder type — TUI trait wiring is T28.
pub type TuiMode = str;

/// `TuiStopOptions` (pi-tui tui.ts @ 4181f66): `{ preserveScreen?: boolean }`.
/// Placeholder — TUI wiring in T28.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiStopOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_screen: Option<bool>,
}

/// `TuiMainScreen` — placeholder for the trait-object type that T28 will
/// define. Extensions receive `ctx.ui.theme()` etc.; the TUI type surface
/// is not directly callable from extensions yet (T28/T32).
pub type TuiMainScreenRef = Value;
