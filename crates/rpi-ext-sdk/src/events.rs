//! Hook event/result types for wasm guests (#9642, 4c2d91339 — V15-09).
//!
//! Upstream completed the export of the event-hook type surface from the
//! package entry; the wasm-guest equivalent is this module: typed views of
//! the event payloads/results guests receive in `on(...)` handlers (the
//! dispatch boundary is JSON — see `crate::lib` dispatch docs — so
//! handlers that prefer typed payloads deserialize with these).
//!
//! Shape rule (mirrors `rpi-ext-host::types`, which is parity-locked to
//! upstream `extensions/types.ts`): fields upstream types as `AgentMessage`,
//! `AssistantMessage`, `Usage`, `ImageContent`, `UserContent`,
//! `ProviderHeaders`, `ToolResultMessage`, or entry arrays are carried as
//! [`Value`] — the heavy payloads cross the JSON boundary verbatim and the
//! SDK does not depend on `rpi-ai`. String enums are duplicated here with
//! identical serde renames.
//!
//! Not represented (documented deltas, see V15-09 §7):
//! - `session_compact_failed` / `ui_prompt_start` / `ui_prompt_end` /
//!   `agent_start` / `agent_settled` payloads travel as raw JSON (no host
//!   structs exist — pre-existing shape rule, unchanged by #9642);
//! - upstream's per-tool `ToolCallEvent` union (`ReadToolCallEvent`, …)
//!   collapses to [`ToolCallEvent`] (`tool_name` discriminator) — the
//!   TS-level narrowing has no runtime effect;
//! - `BeforeProviderRequestEventResult` is `unknown` upstream — any
//!   non-null JSON replaces the payload (no view needed).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// String enums (serde-identical to `rpi-ext-host::types`)
// ============================================================================

/// `ProjectTrustEventResult["trusted"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectTrustDecision {
    #[serde(rename = "yes")]
    Yes,
    #[serde(rename = "no")]
    No,
    #[serde(rename = "undecided")]
    Undecided,
}

/// `ResourcesDiscoverEvent["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourcesDiscoverReason {
    #[serde(rename = "startup")]
    Startup,
    #[serde(rename = "reload")]
    Reload,
}

/// `SessionStartEvent["reason"]`.
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

/// `SessionShutdownEvent["reason"]`.
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

/// `SessionBeforeSwitchEvent["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSwitchReason {
    #[serde(rename = "new")]
    New,
    #[serde(rename = "resume")]
    Resume,
}

/// `SessionBeforeForkEvent["position"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkPosition {
    #[serde(rename = "before")]
    Before,
    #[serde(rename = "at")]
    At,
}

/// Compaction trigger reason (`session_before_compact` / `session_compact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionReason {
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "threshold")]
    Threshold,
    #[serde(rename = "overflow")]
    Overflow,
}

/// `ModelSelectEvent["source"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSelectSource {
    #[serde(rename = "set")]
    Set,
    #[serde(rename = "cycle")]
    Cycle,
    #[serde(rename = "restore")]
    Restore,
}

/// `InputEvent["source"]`.
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

/// `InputEvent["streamingBehavior"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

/// `cache_warming_decision` action (#9668).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheWarmingAction {
    #[serde(rename = "warm")]
    Warm,
    #[serde(rename = "stop")]
    Stop,
}

// ============================================================================
// Startup / resource events
// ============================================================================

/// `project_trust` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEvent {
    pub cwd: String,
}

/// `project_trust` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEventResult {
    pub trusted: ProjectTrustDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
}

/// `resources_discover` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverEvent {
    pub cwd: String,
    pub reason: ResourcesDiscoverReason,
}

/// `resources_discover` result.
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
// Session events
// ============================================================================

/// `session_start` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartEvent {
    pub reason: SessionStartReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_session_file: Option<String>,
}

/// `session_info_changed` payload (`None` = name cleared).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoChangedEvent {
    pub name: Option<String>,
}

/// `session_before_switch` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchEvent {
    pub reason: SessionSwitchReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// `session_before_switch` result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
}

/// `session_before_fork` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkEvent {
    pub entry_id: String,
    pub position: ForkPosition,
}

/// `session_before_fork` result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_conversation_restore: Option<bool>,
}

/// `session_before_compact` payload (`preparation`/`branchEntries` as JSON).
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

/// `session_before_compact` result (`compaction` = upstream
/// `CompactionResult` JSON).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Value>,
}

/// `session_compact` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactEvent {
    pub compaction_entry: Value,
    pub from_extension: bool,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

/// `session_shutdown` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionShutdownEvent {
    pub reason: SessionShutdownReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// `TreePreparation` (`entries_to_summarize` as JSON).
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

/// `session_before_tree` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent {
    pub preparation: TreePreparation,
}

/// Extension-provided branch summary (`usage` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionBranchSummary {
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

/// `session_before_tree` result.
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

/// `session_tree` payload.
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
// Agent events
// ============================================================================

/// `context` payload (`messages` = `AgentMessage[]` JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEvent {
    pub messages: Value,
}

/// `context` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Value>,
}

/// `cache_warming_decision` payload (#9668).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingDecisionEvent {
    pub warm_cost: f64,
    pub miss_cost: f64,
    pub continuation_probability: f64,
    pub action: CacheWarmingAction,
}

/// `cache_warming_decision` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheWarmingDecisionEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<CacheWarmingAction>,
}

/// `before_provider_request` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderRequestEvent {
    pub payload: Value,
}

/// `before_provider_headers` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderHeadersEvent {
    pub headers: Value,
}

/// `after_provider_response` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterProviderResponseEvent {
    pub status: u32,
    pub headers: std::collections::HashMap<String, String>,
}

/// `before_agent_start` payload (`systemPromptOptions` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEvent {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Value>,
    pub system_prompt: String,
    pub system_prompt_options: Value,
}

/// `before_agent_start` result message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartMessage {
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// `before_agent_start` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<BeforeAgentStartMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// Combined `before_agent_start` result across handlers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartCombinedResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<BeforeAgentStartMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// `agent_end` payload (`messages` = `AgentMessage[]` JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEndEvent {
    pub messages: Value,
}

/// `turn_start` payload (unix-ms timestamp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartEvent {
    pub turn_index: u32,
    pub timestamp: i64,
}

/// `turn_end` payload (`message`/`toolResults` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndEvent {
    pub turn_index: u32,
    pub message: Value,
    pub tool_results: Value,
}

/// `message_start` / `message_end` shared payload (`message` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    pub message: Value,
}

/// `message_update` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdateEvent {
    pub message: Value,
    pub assistant_message_event: Value,
}

/// `message_end` result (replacement message must keep the original role).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEndEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
}

// ============================================================================
// Tool execution events
// ============================================================================

/// `tool_execution_start` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStartEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
}

/// `tool_execution_update` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdateEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub partial_result: Value,
}

/// `tool_execution_end` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEndEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Value,
    pub is_error: bool,
}

// ============================================================================
// Model / thinking events
// ============================================================================

/// `model_select` payload (`model`/`previousModel` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelectEvent {
    pub model: Value,
    pub previous_model: Option<Value>,
    pub source: ModelSelectSource,
}

/// `thinking_level_select` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelSelectEvent {
    pub level: String,
    pub previous_level: String,
}

// ============================================================================
// User bash / input events
// ============================================================================

/// `user_bash` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBashEvent {
    pub command: String,
    pub exclude_from_context: bool,
    pub cwd: String,
}

/// `user_bash` result (`operations`/`result` as JSON).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBashEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// `input` payload (`images` as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputEvent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Value>,
    pub source: InputSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming_behavior: Option<StreamingBehavior>,
}

/// `input` result — tagged on `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all_fields = "camelCase")]
pub enum InputEventResult {
    #[serde(rename = "continue")]
    Continue,
    #[serde(rename = "transform")]
    Transform {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Value>,
    },
    #[serde(rename = "handled")]
    Handled,
}

// ============================================================================
// Tool events
// ============================================================================

/// `tool_call` payload (per-tool union collapsed; `input` mutable upstream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
}

/// `tool_call` result (#7715 `terminate` included).
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

/// `tool_result` payload (`usage` as JSON).
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

/// `tool_result` result — a partial patch; each present field replaces.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

// ============================================================================
// Tests: compile-time export existence (#9642's fix, as a test) + a
// serialization corpus for the structured subset (Value-carrying fields
// round-trip the wire shape; the enums byte-match the host renames).
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time existence of the full exported surface — the rpi
    /// equivalent of upstream's package-entry re-export test intent
    /// (#9642: `import type { X } from "@earendil-works/pi-coding-agent"`).
    #[test]
    fn event_hook_type_surface_is_exported() {
        fn exported<T: Sized>() {}

        exported::<ProjectTrustEvent>();
        exported::<ProjectTrustEventResult>();
        exported::<ProjectTrustDecision>();
        exported::<ResourcesDiscoverEvent>();
        exported::<ResourcesDiscoverResult>();
        exported::<ResourcesDiscoverReason>();
        exported::<SessionStartEvent>();
        exported::<SessionStartReason>();
        exported::<SessionInfoChangedEvent>();
        exported::<SessionBeforeSwitchEvent>();
        exported::<SessionBeforeSwitchResult>();
        exported::<SessionSwitchReason>();
        exported::<SessionBeforeForkEvent>();
        exported::<SessionBeforeForkResult>();
        exported::<ForkPosition>();
        exported::<SessionBeforeCompactEvent>();
        exported::<SessionBeforeCompactResult>();
        exported::<CompactionReason>();
        exported::<SessionCompactEvent>();
        exported::<SessionShutdownEvent>();
        exported::<SessionShutdownReason>();
        exported::<TreePreparation>();
        exported::<SessionBeforeTreeEvent>();
        exported::<SessionBeforeTreeResult>();
        exported::<SessionTreeEvent>();
        exported::<ExtensionBranchSummary>();
        exported::<ContextEvent>();
        exported::<ContextEventResult>();
        exported::<CacheWarmingDecisionEvent>();
        exported::<CacheWarmingDecisionEventResult>();
        exported::<CacheWarmingAction>();
        exported::<BeforeProviderRequestEvent>();
        exported::<BeforeProviderHeadersEvent>();
        exported::<AfterProviderResponseEvent>();
        exported::<BeforeAgentStartEvent>();
        exported::<BeforeAgentStartEventResult>();
        exported::<BeforeAgentStartMessage>();
        exported::<BeforeAgentStartCombinedResult>();
        exported::<AgentEndEvent>();
        exported::<TurnStartEvent>();
        exported::<TurnEndEvent>();
        exported::<MessageEvent>();
        exported::<MessageUpdateEvent>();
        exported::<MessageEndEventResult>();
        exported::<ToolExecutionStartEvent>();
        exported::<ToolExecutionUpdateEvent>();
        exported::<ToolExecutionEndEvent>();
        exported::<ModelSelectEvent>();
        exported::<ModelSelectSource>();
        exported::<ThinkingLevelSelectEvent>();
        exported::<UserBashEvent>();
        exported::<UserBashEventResult>();
        exported::<InputEvent>();
        exported::<InputEventResult>();
        exported::<InputSource>();
        exported::<StreamingBehavior>();
        exported::<ToolCallEvent>();
        exported::<ToolCallEventResult>();
        exported::<ToolResultEvent>();
        exported::<ToolResultEventResult>();
    }

    /// Wire-shape corpus: each structured type deserializes the payload the
    /// host emits (camelCase + optional-omitted) and re-serializes
    /// byte-identically.
    #[test]
    fn structured_event_views_round_trip_the_wire_shape() {
        fn round_trip<T>(payload: serde_json::Value)
        where
            T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        {
            let parsed: T = serde_json::from_value(payload.clone())
                .unwrap_or_else(|error| panic!("deserialize {payload}: {error}"));
            let back = serde_json::to_value(&parsed).expect("serialize");
            assert_eq!(back, payload, "round trip changed the wire shape");
        }

        round_trip::<ProjectTrustEvent>(serde_json::json!({"cwd": "/w"}));
        round_trip::<ProjectTrustEventResult>(
            serde_json::json!({"trusted": "yes", "remember": true}),
        );
        round_trip::<ResourcesDiscoverEvent>(serde_json::json!({"cwd": "/w", "reason": "startup"}));
        round_trip::<ResourcesDiscoverResult>(serde_json::json!({"skillPaths": ["a"]}));
        round_trip::<SessionStartEvent>(
            serde_json::json!({"reason": "resume", "previousSessionFile": "s.jsonl"}),
        );
        round_trip::<SessionInfoChangedEvent>(serde_json::json!({"name": null}));
        round_trip::<SessionBeforeSwitchEvent>(
            serde_json::json!({"reason": "new", "targetSessionFile": "n.jsonl"}),
        );
        round_trip::<SessionBeforeForkEvent>(
            serde_json::json!({"entryId": "e1", "position": "before"}),
        );
        round_trip::<SessionBeforeCompactEvent>(serde_json::json!({
            "preparation": {"tokens": 100},
            "branchEntries": [],
            "reason": "threshold",
            "willRetry": false,
        }));
        round_trip::<SessionCompactEvent>(serde_json::json!({
            "compactionEntry": {"id": "c1"},
            "fromExtension": false,
            "reason": "manual",
            "willRetry": false,
        }));
        round_trip::<SessionShutdownEvent>(serde_json::json!({"reason": "quit"}));
        round_trip::<SessionBeforeTreeEvent>(serde_json::json!({
            "preparation": {
                "targetId": "t1",
                "oldLeafId": null,
                "commonAncestorId": null,
                "entriesToSummarize": [],
                "userWantsSummary": true,
            },
        }));
        round_trip::<SessionTreeEvent>(serde_json::json!({
            "newLeafId": "n1",
            "oldLeafId": "o1",
            "summaryEntry": {"id": "s"},
        }));
        round_trip::<CacheWarmingDecisionEvent>(serde_json::json!({
            "warmCost": 0.05,
            "missCost": 0.5,
            "continuationProbability": 0.5,
            "action": "warm",
        }));
        round_trip::<CacheWarmingDecisionEventResult>(serde_json::json!({"action": "stop"}));
        round_trip::<BeforeProviderRequestEvent>(serde_json::json!({"payload": {"model": "m"}}));
        round_trip::<AfterProviderResponseEvent>(
            serde_json::json!({"status": 200, "headers": {"x": "y"}}),
        );
        round_trip::<BeforeAgentStartEvent>(serde_json::json!({
            "prompt": "hi",
            "systemPrompt": "base",
            "systemPromptOptions": {"cwd": "/w"},
        }));
        round_trip::<BeforeAgentStartEventResult>(serde_json::json!({
            "message": {"customType": "note", "display": false},
            "systemPrompt": "forced",
        }));
        round_trip::<TurnStartEvent>(serde_json::json!({"turnIndex": 0, "timestamp": 1}));
        round_trip::<MessageUpdateEvent>(serde_json::json!({
            "message": {"role": "assistant"},
            "assistantMessageEvent": {"type": "text_delta", "delta": "x"},
        }));
        round_trip::<ToolExecutionStartEvent>(
            serde_json::json!({"toolCallId": "c1", "toolName": "read", "args": {"path": "p"}}),
        );
        round_trip::<ToolExecutionEndEvent>(serde_json::json!({
            "toolCallId": "c1",
            "toolName": "bash",
            "result": {"content": []},
            "isError": false,
        }));
        round_trip::<ModelSelectEvent>(serde_json::json!({
            "model": {"id": "m"},
            "previousModel": null,
            "source": "set",
        }));
        round_trip::<ThinkingLevelSelectEvent>(
            serde_json::json!({"level": "high", "previousLevel": "off"}),
        );
        round_trip::<UserBashEvent>(serde_json::json!({
            "command": "ls",
            "excludeFromContext": false,
            "cwd": "/w",
        }));
        round_trip::<InputEvent>(serde_json::json!({
            "text": "hi",
            "source": "rpc",
            "streamingBehavior": "steer",
        }));
        round_trip::<ToolCallEvent>(
            serde_json::json!({"toolCallId": "c1", "toolName": "edit", "input": {"path": "p"}}),
        );
        round_trip::<ToolCallEventResult>(
            serde_json::json!({"block": true, "reason": "no", "terminate": false}),
        );
        round_trip::<ToolResultEvent>(serde_json::json!({
            "toolCallId": "c1",
            "toolName": "read",
            "input": {},
            "content": [{"type": "text", "text": "ok"}],
            "isError": false,
        }));
        round_trip::<ToolResultEventResult>(serde_json::json!({"isError": true}));
    }

    /// The `input` result tag vocabulary (`continue` / `transform` /
    /// `handled`) — the runner contract agents rely on (#8718 tests it).
    #[test]
    fn input_event_result_tags_match_the_runner_contract() {
        let handled: InputEventResult =
            serde_json::from_value(serde_json::json!({"action": "handled"})).expect("handled");
        assert!(matches!(handled, InputEventResult::Handled));
        let cont: InputEventResult =
            serde_json::from_value(serde_json::json!({"action": "continue"})).expect("continue");
        assert!(matches!(cont, InputEventResult::Continue));
        let transform: InputEventResult = serde_json::from_value(
            serde_json::json!({"action": "transform", "text": "transformed: x"}),
        )
        .expect("transform");
        assert!(matches!(
            transform,
            InputEventResult::Transform { ref text, .. } if text == "transformed: x"
        ));
    }
}
