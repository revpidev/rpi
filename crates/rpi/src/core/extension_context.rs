//! Session/runtime-backed context actions (T15 W5) — the rpi-side
//! implementations of `ExtensionContextActions` (types.ts:1612-1628) and
//! `ExtensionCommandContextActions` (types.ts:1630-1654), bound through
//! `runner.bindCore` / `bindCommandContext` equivalents
//! (agent-session.ts:2405-2431).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use rpi_ext_host::api::{
    CommandContextActions, CompactOptions, ContextActions, ContextUsage, NavigateTreeOptions,
    ReplacedSessionContext, WithSessionFn,
};
use serde_json::Value;

use crate::core::agent_session::{AgentSession, WeakAgentSession};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::extension_host_adapter::host_of_runner;
use crate::core::usage_totals::usage_totals_from_entries;

/// `ExtensionContextActions` backed by the bound session (weak — the
/// session owns the host through the runner slot).
pub struct SessionContextActions {
    session: WeakAgentSession,
}

impl SessionContextActions {
    pub fn new(session: &AgentSession) -> Self {
        SessionContextActions {
            session: session.downgrade(),
        }
    }

    fn session(&self) -> Option<AgentSession> {
        self.session.upgrade()
    }
}

#[async_trait]
impl ContextActions for SessionContextActions {
    fn get_model(&self) -> Option<Value> {
        self.session()
            .and_then(|session| session.model())
            .and_then(|model| serde_json::to_value(model).ok())
    }

    fn is_idle(&self) -> bool {
        self.session().is_none_or(|session| session.is_idle())
    }

    fn is_project_trusted(&self) -> bool {
        self.session().is_none_or(|session| {
            session.settings_manager(|settings| settings.is_project_trusted())
        })
    }

    fn get_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.session().and_then(|session| session.agent().signal())
    }

    /// `abort` (agent-session.ts:2413-2418: the mode's abort handler wins,
    /// else `agent.abort()`).
    fn abort(&self) {
        if let Some(session) = self.session() {
            tokio::spawn(async move {
                session.abort().await;
            });
        }
    }

    fn has_pending_messages(&self) -> bool {
        self.session()
            .is_some_and(|session| session.pending_message_count() > 0)
    }

    /// `shutdown` (agent-session.ts:2421-2422): the mode-provided handler
    /// bound via `ExtensionBindings::shutdown`.
    fn shutdown(&self) {
        if let Some(session) = self.session() {
            session.extension_shutdown();
        }
    }

    fn get_context_usage(&self) -> Option<ContextUsage> {
        self.session()
            .and_then(|session| session.get_context_usage())
            .map(|usage| ContextUsage {
                tokens: usage.tokens,
                context_window: usage.context_window,
                percent: usage.percent,
            })
    }

    /// `ctx.sessionFile()` (rpi additive, ADR-0022): the bound
    /// `SessionManager` is the authoritative source — `path: None` for
    /// in-memory sessions (FR-I, rpi-statusline 03-realtime-token-count
    /// §1.3 HR-E: kills the same-cwd dual-instance transcript mixup,
    /// TE-D34 §1).
    fn get_session_file(&self) -> Option<rpi_ext_host::types::SessionFileInfo> {
        let session = self.session()?;
        Some(rpi_ext_host::types::SessionFileInfo {
            path: session
                .session_file()
                .map(|path| path.display().to_string()),
            id: session.session_id().to_owned(),
        })
    }

    /// `ctx.sessionEntries` (rpi additive, ADR-0027): the active branch
    /// (root→leaf) `type:"custom"` entries of the bound session, read-only
    /// via `SessionManager::get_branch` — the same in-memory structure for
    /// file-backed and `--no-session` hosts (upstream
    /// `ctx.sessionManager.getBranch()`, index.ts:217-229 @ 928c30c).
    /// Projection walks the RAW stored entry (the persistence source of
    /// truth), so malformed-but-navigable custom entries round-trip
    /// verbatim instead of being dropped by the typed view. Filtering
    /// order per ADR-0027: `type:"custom"` → exact `customType` → tail
    /// `limit`. Fail-closed `[]` when the session is gone or no branch is
    /// active (`leaf_id = None`).
    fn get_session_entries(
        &self,
        custom_type: Option<&str>,
        limit: Option<u64>,
    ) -> Vec<rpi_ext_host::types::SessionEntryInfo> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        let manager = session.session_manager();
        let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<rpi_ext_host::types::SessionEntryInfo> = manager
            .get_branch(None)
            .iter()
            .filter(|entry| entry.type_tag() == "custom")
            .filter(|entry| {
                custom_type.is_none_or(|want| {
                    entry.raw_value().get("customType").and_then(Value::as_str) == Some(want)
                })
            })
            .map(|entry| {
                let raw = entry.raw_value();
                rpi_ext_host::types::SessionEntryInfo {
                    id: raw
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    parent_id: raw
                        .get("parentId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    timestamp: raw
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    custom_type: raw
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    // "data" is the entry payload verbatim (null when the
                    // entry carries none — the raw record omits the key
                    // rather than storing null).
                    data: raw.get("data").cloned().unwrap_or(Value::Null),
                }
            })
            .collect();
        if let Some(limit) = limit {
            let keep = (limit as usize).min(entries.len());
            entries.drain(..entries.len() - keep);
        }
        entries
    }

    /// `ctx.sessionToolResults` (rpi additive, ADR-0030): the active
    /// branch (root→leaf) `type:"message"` entries of the bound session
    /// whose `message.role == "toolResult"` and whose `message.toolName`
    /// matches `tool_name` exactly — the cross-ABI equivalent of upstream
    /// `ctx.sessionManager.getBranch()` replay reads (rpi-todo
    /// `state/replay.ts` @ 338b264, same filter chain).
    /// Like `get_session_entries`, the projection walks the RAW stored
    /// entry so malformed-but-navigable results round-trip verbatim.
    /// Filter order per ADR-0030: `type:"message"` → `role:"toolResult"`
    /// → exact `toolName` → tail `limit`. The projection is exactly the
    /// six ADR-0030 fields — `message.content` blocks never cross the
    /// boundary. Fail-closed `[]` when the session is gone or no branch
    /// is active.
    fn get_session_tool_results(
        &self,
        tool_name: &str,
        limit: Option<u64>,
    ) -> Vec<rpi_ext_host::types::SessionToolResultInfo> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        let manager = session.session_manager();
        let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        let mut results: Vec<rpi_ext_host::types::SessionToolResultInfo> = manager
            .get_branch(None)
            .iter()
            .filter(|entry| entry.type_tag() == "message")
            .filter_map(|entry| {
                let raw = entry.raw_value();
                let message = raw.get("message")?;
                if message.get("role").and_then(Value::as_str) != Some("toolResult") {
                    return None;
                }
                if message.get("toolName").and_then(Value::as_str) != Some(tool_name) {
                    return None;
                }
                Some(rpi_ext_host::types::SessionToolResultInfo {
                    id: raw
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    parent_id: raw
                        .get("parentId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    timestamp: raw
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    tool_name: tool_name.to_owned(),
                    // Malformed-but-navigable results default the flag to
                    // false (ToolResultMessage always serializes `isError`).
                    is_error: message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    // `details` is the structured payload verbatim (null
                    // when the result carries none — the raw record omits
                    // the key rather than storing null).
                    details: message.get("details").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        if let Some(limit) = limit {
            let keep = (limit as usize).min(results.len());
            results.drain(..results.len() - keep);
        }
        results
    }

    /// `ctx.sessionStats()` (rpi additive, ADR-0040 / V18-02): one absolute
    /// snapshot under a single `SessionManager` lock. `entryCount` excludes
    /// the header and is not branch-filtered; `startedAt` comes from the
    /// session header timestamp; the four token components and cost are
    /// accumulated from the same five usage-bearing entry kinds as the
    /// built-in footer. The raw provider `totalTokens` is never read.
    ///
    /// The snapshot is O(entries) under the session lock (it clones the entry
    /// list to aggregate); callers such as the statusline must refresh at
    /// their own cadence rather than on every streaming delta.
    fn session_stats(&self) -> Option<rpi_ext_host::types::SessionStatsSnapshot> {
        let session = self.session()?;
        let manager = session.session_manager();
        let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        let header = manager.get_header()?;
        let entries = manager.get_entries();
        let totals = usage_totals_from_entries(&entries);
        Some(rpi_ext_host::types::SessionStatsSnapshot {
            session_id: manager.get_session_id().to_owned(),
            session_file: manager
                .get_session_file()
                .map(|path| path.to_string_lossy().into_owned()),
            started_at: header.timestamp.clone(),
            entry_count: entries.len() as u64,
            tokens: rpi_ext_host::types::SessionTokenStats {
                input: totals.input,
                output: totals.output,
                cache_read: totals.cache_read,
                cache_write: totals.cache_write,
            },
            cost: totals.cost,
        })
    }

    /// `compact` — fire-and-forget with the callback pair
    /// (agent-session.ts:2423-2433).
    fn compact(&self, options: CompactOptions) {
        let Some(session) = self.session() else {
            return;
        };
        tokio::spawn(async move {
            match session
                .compact(options.custom_instructions.as_deref())
                .await
            {
                Ok(result) => {
                    if let Some(on_complete) = options.on_complete {
                        match serde_json::to_value(&result) {
                            Ok(value) => on_complete(value),
                            Err(error) => {
                                tracing::warn!("compact onComplete payload: {error}")
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(on_error) = options.on_error {
                        on_error(error.to_string());
                    }
                }
            }
        });
    }

    fn get_system_prompt(&self) -> String {
        self.session()
            .map(|session| session.system_prompt())
            .unwrap_or_default()
    }

    fn get_system_prompt_options(&self) -> Value {
        self.session()
            .map(|session| session.system_prompt_options_json())
            .unwrap_or_default()
    }

    // -- v0.11 additions (types.ts @ 4181f66) -------------------------------

    /// `getScopedModels` (runner.ts:706-709 @ 4181f66): read-only snapshot
    /// of the resolved model scope. The session holds `scoped_models:
    /// Vec<ScopedModel>` from the model resolver.
    fn get_scoped_models(&self) -> Vec<rpi_ext_host::types::ScopedModel> {
        self.session()
            .map(|session| {
                session
                    .scoped_models()
                    .iter()
                    .map(|sm| rpi_ext_host::types::ScopedModel {
                        model: serde_json::to_value(&sm.model).unwrap_or(Value::Null),
                        thinking_level: sm
                            .thinking_level
                            .as_ref()
                            .and_then(|tl| serde_json::to_value(tl).ok())
                            .and_then(|v| v.as_str().map(str::to_owned)),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `getSystemPromptSource` (resource-loader.ts:327-329, T24).
    fn get_system_prompt_source_path(&self) -> Option<String> {
        self.session().and_then(|session| {
            session
                .resource_loader()
                .lock()
                .ok()?
                .get_system_prompt_source()
                .map(|p| p.to_string_lossy().into_owned())
        })
    }

    /// `getAppendSystemPromptSources` (resource-loader.ts:335-337, T24).
    fn get_append_system_prompt_source_paths(&self) -> Vec<String> {
        self.session()
            .and_then(|session| {
                Some(
                    session
                        .resource_loader()
                        .lock()
                        .ok()?
                        .get_append_system_prompt_sources()
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                )
            })
            .unwrap_or_default()
    }
}

/// `ExtensionCommandContextActions` backed by the shared runtime handle
/// (rpc mode binds this; the methods take `&mut self` upstream semantics
/// through the mutex).
///
/// The handle is a `Weak`: a strong one cycles runtime → session → runner
/// → host → command actions → runtime and leaks the whole graph (the
/// RPC-mode shutdown path hangs on it).
pub struct RuntimeCommandActions {
    runtime: Weak<tokio::sync::Mutex<AgentSessionRuntime>>,
}

impl RuntimeCommandActions {
    pub fn new(runtime: &Arc<tokio::sync::Mutex<AgentSessionRuntime>>) -> Self {
        RuntimeCommandActions {
            runtime: Arc::downgrade(runtime),
        }
    }

    fn runtime(&self) -> Option<Arc<tokio::sync::Mutex<AgentSessionRuntime>>> {
        self.runtime.upgrade()
    }

    /// Build the extension-facing replaced-session context for the runtime's
    /// current session (agent-session.ts:3304-3312 `createReplacedSessionContext`).
    fn replaced_context(session: &AgentSession) -> Option<ReplacedSessionContext> {
        let host = host_of_runner(&session.extension_runner())?;
        let ctx = host.create_command_context();
        Some(ReplacedSessionContext::new(ctx, host.runtime()))
    }
}

/// Adapt an extension `withSession` closure into the runtime's internal
/// callback shape (a borrow over the call). Shared with the interactive
/// `ctx.command.*` bridge (V17-11 FR-B R2).
pub(crate) fn map_with_session(
    with_session: Option<WithSessionFn>,
) -> Option<
    impl Fn(
        crate::core::agent_session_runtime::ReplacedSessionContext,
    ) -> futures::future::BoxFuture<'static, ()>
    + Send
    + Sync,
> {
    with_session.map(|with_session| {
        move |replaced: crate::core::agent_session_runtime::ReplacedSessionContext| {
            let with_session = with_session.clone();
            Box::pin(async move {
                if let Some(ctx) = RuntimeCommandActions::replaced_context(replaced.session()) {
                    with_session(ctx).await;
                }
            }) as futures::future::BoxFuture<'static, ()>
        }
    })
}

#[async_trait]
impl CommandContextActions for RuntimeCommandActions {
    async fn wait_for_idle(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let session = runtime.lock().await.session().clone();
        session.wait_for_idle().await;
    }

    async fn new_session(
        &self,
        parent_session: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .new_session(
                parent_session.as_deref(),
                None,
                map_with_session(with_session).map(|f| Box::new(f) as _),
            )
            .await
        {
            Ok(cancelled) => cancelled,
            Err(error) => {
                tracing::warn!("extension newSession failed: {error}");
                false
            }
        }
    }

    async fn fork(
        &self,
        entry_id: &str,
        position: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let position = match position.as_deref() {
            Some("at") => crate::core::agent_session_runtime::ForkPosition::At,
            _ => crate::core::agent_session_runtime::ForkPosition::Before,
        };
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .fork(
                entry_id,
                position,
                map_with_session(with_session).map(|f| Box::new(f) as _),
            )
            .await
        {
            Ok(result) => result.cancelled,
            Err(error) => {
                tracing::warn!("extension fork failed: {error}");
                false
            }
        }
    }

    async fn navigate_tree(&self, target_id: &str, options: NavigateTreeOptions) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let session = runtime.lock().await.session().clone();
        match session
            .navigate_tree(
                target_id,
                crate::core::agent_session::NavigateTreeOptions {
                    summarize: options.summarize.unwrap_or(false),
                    custom_instructions: options.custom_instructions,
                    replace_instructions: options.replace_instructions.unwrap_or(false),
                    label: options.label,
                },
            )
            .await
        {
            Ok(result) => result.cancelled,
            Err(error) => {
                tracing::warn!("extension navigateTree failed: {error}");
                false
            }
        }
    }

    async fn switch_session(
        &self,
        session_path: &str,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .switch_session(
                session_path,
                None,
                map_with_session(with_session).map(|f| Box::new(f) as _),
                None,
            )
            .await
        {
            Ok(cancelled) => cancelled,
            Err(error) => {
                tracing::warn!("extension switchSession failed: {error}");
                false
            }
        }
    }

    /// `reload` — the session+host reload flow (agent-session.ts:2600-2628).
    async fn reload(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let session = runtime.lock().await.session().clone();
        session.reload().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::messages::AgentMessage;
    use rpi_ai::types::{
        ApiKind, AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent,
        ToolResultContent, ToolResultMessage, ToolResultRole, Usage, UsageCost,
    };

    use crate::core::agent_session::AgentSession;

    fn usage(
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: f64,
        raw_total: u64,
    ) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write1h: None,
            reasoning: None,
            // Deliberately inconsistent with the four components: the
            // snapshot must never read the provider's raw total.
            total_tokens: raw_total,
            cost: UsageCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
            },
        }
    }

    fn assistant_message(usage: Usage) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(TextContent {
                text: "ok".to_owned(),
                text_signature: None,
            })],
            api: ApiKind("openai-completions".into()),
            provider: "custom".to_owned(),
            model: "m1".to_owned(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 1_700_000_000_000,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
            duration_ms: None,
        })
    }

    fn tool_result_message(usage: Usage) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".to_owned(),
            tool_name: "read".to_owned(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "ok".to_owned(),
                text_signature: None,
            })],
            details: None,
            usage: Some(usage),
            nested_calls: None,
            is_error: false,
            timestamp: 1_700_000_000_000,
            duration_ms: None,
        })
    }

    fn actions(session: &AgentSession) -> SessionContextActions {
        SessionContextActions::new(session)
    }

    /// ADR-0040: the snapshot aggregates exactly the five usage-bearing
    /// entry kinds, ignores the raw `totalTokens`, uses the header
    /// timestamp and counts entries without branch filtering.
    #[tokio::test]
    async fn session_stats_snapshot_has_five_usage_kinds_and_ignores_raw_total() {
        let fixture = crate::modes::interactive::test_support::build_test_session().await;
        let (started_at, base_entries) = {
            let manager = fixture.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            (
                manager
                    .get_header()
                    .expect("session header")
                    .timestamp
                    .clone(),
                manager.get_entries().len(),
            )
        };
        {
            let manager = fixture.session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(assistant_message(usage(1, 2, 3, 4, 0.1, 999_999)))
                .expect("assistant");
            manager
                .append_message(tool_result_message(usage(10, 20, 30, 40, 1.0, 0)))
                .expect("tool result");
            manager
                .branch_with_summary(
                    None,
                    "branch",
                    None,
                    None,
                    Some(usage(100, 200, 300, 400, 10.0, 7)),
                )
                .expect("branch summary");
            manager
                .append_compaction(
                    "compact",
                    None,
                    0,
                    None,
                    None,
                    Some(usage(1000, 2000, 3000, 4000, 100.0, 1)),
                )
                .expect("compaction");
            manager
                .append_usage(
                    "test",
                    "custom",
                    "m1",
                    usage(10_000, 20_000, 30_000, 40_000, 1000.0, 0),
                    None,
                )
                .expect("usage entry");
        }

        let stats = actions(&fixture.session)
            .session_stats()
            .expect("bound snapshot");
        assert_eq!(stats.session_id, fixture.session.session_id());
        assert_eq!(stats.session_file, None, "in-memory sessions omit the file");
        assert_eq!(stats.started_at, started_at);
        assert_eq!(stats.entry_count, (base_entries + 5) as u64);
        assert_eq!(stats.tokens.input, 11_111);
        assert_eq!(stats.tokens.output, 22_222);
        assert_eq!(stats.tokens.cache_read, 33_333);
        assert_eq!(stats.tokens.cache_write, 44_444);
        assert!((stats.cost - 1111.1).abs() < 1e-9, "cost: {}", stats.cost);

        // Same aggregation as the built-in footer/session statistics: the
        // snapshot and `AgentSession::get_session_stats` must agree on the
        // four components and cost for the same entries.
        let footer_stats = fixture.session.get_session_stats();
        assert_eq!(stats.tokens.input, footer_stats.tokens.input);
        assert_eq!(stats.tokens.output, footer_stats.tokens.output);
        assert_eq!(stats.tokens.cache_read, footer_stats.tokens.cache_read);
        assert_eq!(stats.tokens.cache_write, footer_stats.tokens.cache_write);
        assert_eq!(stats.cost, footer_stats.cost);
    }

    /// A fresh session has zeroed components, `entryCount = 0` and the
    /// header timestamp; an in-memory session omits `sessionFile`.
    #[tokio::test]
    async fn session_stats_snapshot_of_a_fresh_session() {
        let fixture = crate::modes::interactive::test_support::build_test_session().await;
        let base_entries = {
            let manager = fixture.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager.get_entries().len()
        };
        let stats = actions(&fixture.session)
            .session_stats()
            .expect("bound snapshot");
        assert_eq!(stats.entry_count, base_entries as u64);
        assert_eq!(stats.tokens.input, 0);
        assert_eq!(stats.tokens.output, 0);
        assert_eq!(stats.tokens.cache_read, 0);
        assert_eq!(stats.tokens.cache_write, 0);
        assert_eq!(stats.cost, 0.0);
        assert!(stats.session_file.is_none());
        assert!(!stats.started_at.is_empty());
    }
}
