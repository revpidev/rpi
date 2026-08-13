//! Adapter: `rpi_ext_host::NativeExtensionHost` → the session-facing
//! [`ExtensionRunner`] seam (T15 W1).
//!
//! Upstream counterpart: `AgentSession` holds the `ExtensionRunner`
//! (extensions/runner.ts) directly; here the host is a separate crate and
//! the session talks to it through this thin translation layer. All heavy
//! semantics (conflict rules, dispatch order, error isolation) live in
//! `rpi-ext-host`; this module only converts types between the rpi-side
//! seam ([`crate::core::extensions`]) and the host's camelCase JSON event
//! model.
//!
//! Known gaps (tracked in docs/plan/v0.1/T15-extension-host.md):
//! - `emit_project_trust` handler errors are routed to `emit_error`
//!   listeners; upstream reports them through a mode callback as a
//!   formatted string (project-trust.ts:60-62).
//! - `user_bash` custom `operations` (a closure bundle upstream) cannot
//!   cross the JSON dispatch boundary and are dropped with a warning;
//!   the full-replacement `result` branch works (candidate deviation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::messages::AgentMessage;
use rpi_ai::types::{ImageContent, ProviderHeaders};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::types::{self as ext, ExtensionError};
use serde_json::Value;

use crate::cli::args::UnknownFlagValue;
use crate::core::extensions::{
    BeforeAgentStartMessage, BeforeAgentStartResult, ExtensionCommand, ExtensionErrorInfo,
    ExtensionErrorListener, ExtensionRunner, ExtensionToolEntry, InputEventResult, InputSource,
    ProjectTrustEventResult, SessionBeforeCompactEvent, SessionBeforeCompactResult,
    SessionBeforeTreeResult, SessionCancelResult, StreamingBehavior, ToolCallOutcome,
    ToolResultPatch,
};
use crate::core::resource_loader::{ResourceExtensionPath, ResourceExtensionPaths};
use crate::core::skills::{SourceInfo, SourceOrigin, SourceScope};

/// Thin [`ExtensionRunner`] implementation backed by a
/// [`NativeExtensionHost`]. Cheap to clone (shares the host).
#[derive(Clone)]
pub struct ExtensionHostAdapter {
    host: Arc<NativeExtensionHost>,
}

impl ExtensionHostAdapter {
    pub fn new(host: Arc<NativeExtensionHost>) -> Self {
        ExtensionHostAdapter { host }
    }

    pub fn host(&self) -> &Arc<NativeExtensionHost> {
        &self.host
    }

    /// Deserialize a host JSON result into `T`; malformed extension output
    /// degrades to `None` (the emit already isolated the handler error).
    fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Option<T> {
        serde_json::from_value(value).ok()
    }
}

/// Reach the host behind a runner (the interactive/RPC/print bridges and
/// renderer plumbing go through this; `None` for the no-op runner).
pub fn host_of_runner(runner: &Arc<dyn ExtensionRunner>) -> Option<Arc<NativeExtensionHost>> {
    runner
        .as_any()
        .and_then(|any| any.downcast_ref::<ExtensionHostAdapter>())
        .map(|adapter| adapter.host().clone())
}

/// Parse a host `project_trust` result JSON into the rpi-side result type.
/// Shared by the adapter and the startup pipeline (app.rs emits through the
/// host directly before the adapter exists).
pub fn parse_project_trust_result(result: &Value) -> ProjectTrustEventResult {
    let trusted = match result.get("trusted").and_then(Value::as_str) {
        Some("yes") => crate::core::extensions::ProjectTrustEventDecision::Yes,
        Some("no") => crate::core::extensions::ProjectTrustEventDecision::No,
        _ => crate::core::extensions::ProjectTrustEventDecision::Undecided,
    };
    let remember = result.get("remember").and_then(Value::as_bool);
    ProjectTrustEventResult { trusted, remember }
}

/// Convert the host's `ExtSourceInfo` (source-info.ts shape) into rpi's
/// `SourceInfo` (skills.rs).
fn convert_source_info(info: &ext::ExtSourceInfo) -> SourceInfo {
    SourceInfo {
        path: Path::new(&info.path).to_path_buf(),
        source: info.source.clone(),
        scope: match info.scope {
            ext::SourceScope::User => SourceScope::User,
            ext::SourceScope::Project => SourceScope::Project,
            ext::SourceScope::Temporary => SourceScope::Temporary,
        },
        origin: match info.origin {
            ext::SourceOrigin::Package => SourceOrigin::Package,
            ext::SourceOrigin::TopLevel => SourceOrigin::TopLevel,
        },
        base_dir: info.base_dir.as_ref().map(PathBuf::from),
    }
}

/// `wrapRegisteredTool` (wrapper.ts:17-37): adapt a host-registered tool
/// into an `AgentTool` — execution runs the extension's `execute` with the
/// host context, and tools activated during execution surface via
/// `addedToolNames` (pure additions only; a removal falls back to the full
/// next-turn set upstream computes elsewhere).
pub struct HostToolAdapter {
    host: Arc<NativeExtensionHost>,
    definition: ext::ToolDefinition,
}

#[async_trait]
impl rpi_agent::types::AgentTool for HostToolAdapter {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn label(&self) -> &str {
        &self.definition.label
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters(&self) -> &Value {
        &self.definition.parameters
    }

    fn execution_mode(&self) -> Option<rpi_agent::types::ToolExecutionMode> {
        self.definition.execution_mode
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        match &self.definition.prepare_arguments {
            Some(prepare) => match prepare(args.clone()) {
                Ok(prepared) => prepared,
                Err(error) => {
                    tracing::warn!(
                        "extension tool {} prepareArguments failed: {error}",
                        self.definition.name
                    );
                    args
                }
            },
            None => args,
        }
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: tokio_util::sync::CancellationToken,
        on_update: Option<rpi_agent::types::AgentToolUpdateCallback>,
    ) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::AgentError> {
        // wrapper.ts:23-34 — snapshot active tools around execution; only a
        // pure addition set is attached as `addedToolNames`.
        let actions = self.host.runtime().actions();
        let active_before = actions.as_ref().map(|a| a.get_active_tools());
        let request = ext::ToolExecuteRequest {
            tool_call_id: tool_call_id.to_owned(),
            params,
            signal,
            on_update,
        };
        let ctx = self.host.core().create_context();
        let mut result = (self.definition.execute)(request, ctx)
            .await
            .map_err(rpi_agent::AgentError::Tool)?;
        let (Some(actions), Some(before)) = (actions, active_before) else {
            return Ok(result);
        };
        let after = actions.get_active_tools();
        if !before.iter().all(|name| after.contains(name)) {
            return Ok(result);
        }
        let mut added: Vec<String> = after
            .into_iter()
            .filter(|name| !before.contains(name))
            .collect();
        if added.is_empty() {
            return Ok(result);
        }
        // `[...new Set([...(result.addedToolNames ?? []), ...added])]`.
        let mut merged: Vec<String> = result.added_tool_names.take().unwrap_or_default();
        for name in added.drain(..) {
            if !merged.contains(&name) {
                merged.push(name);
            }
        }
        result.added_tool_names = Some(merged);
        Ok(result)
    }
}

/// `getExtensionSourceLabel` (agent-session.ts:2298-2304).
fn extension_source_label(extension_path: &str) -> String {
    if let Some(stripped) = extension_path.strip_prefix('<') {
        return format!("extension:{}", stripped.replace(['<', '>'], ""));
    }
    let base = Path::new(extension_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| extension_path.to_owned());
    // `.wasm` (and legacy `.ts`/`.js`) suffixes are stripped.
    let name = base
        .trim_end_matches(".wasm")
        .trim_end_matches(".ts")
        .trim_end_matches(".js");
    format!("extension:{name}")
}

/// `buildExtensionResourcePaths` (agent-session.ts:2276-2296).
fn build_extension_resource_paths(entries: Vec<(String, String)>) -> Vec<ResourceExtensionPath> {
    entries
        .into_iter()
        .map(|(path, extension_path)| {
            let source = extension_source_label(&extension_path);
            let base_dir = if extension_path.starts_with('<') {
                None
            } else {
                Path::new(&extension_path).parent().map(|p| p.to_path_buf())
            };
            ResourceExtensionPath {
                path: Path::new(&path).to_path_buf(),
                source_info: SourceInfo {
                    path: Path::new(&path).to_path_buf(),
                    source,
                    scope: SourceScope::Temporary,
                    origin: SourceOrigin::TopLevel,
                    base_dir,
                },
            }
        })
        .collect()
}

/// Extract `{ path, extensionPath }` pairs from a `resources_discover`
/// result list (runner.ts:1157-1165).
fn path_entries(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("path")?.as_str()?.to_owned(),
                        entry.get("extensionPath")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl ExtensionRunner for ExtensionHostAdapter {
    fn has_handlers(&self, event_type: &str) -> bool {
        self.host.has_handlers(event_type)
    }

    async fn emit_cancelable(&self, event_type: &str) -> Option<SessionCancelResult> {
        let payload = serde_json::json!({ "type": event_type });
        let result = self.host.emit(event_type, payload).await?;
        let cancel = result
            .get("cancel")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(SessionCancelResult { cancel })
    }

    async fn emit_cancelable_with(
        &self,
        event_type: &str,
        payload: Value,
    ) -> Option<SessionCancelResult> {
        let result = self.host.emit(event_type, payload).await?;
        let cancel = result
            .get("cancel")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(SessionCancelResult { cancel })
    }

    async fn emit_session_before_compact(
        &self,
        event: &SessionBeforeCompactEvent,
    ) -> Option<SessionBeforeCompactResult> {
        // `signal` is not serializable; the event otherwise matches
        // types.ts:586-596.
        let payload = serde_json::json!({
            "type": ext::EVENT_SESSION_BEFORE_COMPACT,
            "preparation": event.preparation,
            "branchEntries": event.branch_entries,
            "customInstructions": event.custom_instructions,
            "reason": event.reason,
            "willRetry": event.will_retry,
        });
        let result = self
            .host
            .emit(ext::EVENT_SESSION_BEFORE_COMPACT, payload)
            .await?;
        let cancel = result.get("cancel").and_then(Value::as_bool);
        let compaction = result
            .get("compaction")
            .cloned()
            .filter(|c| !c.is_null())
            .and_then(Self::parse);
        Some(SessionBeforeCompactResult { cancel, compaction })
    }

    async fn emit_session_compact(
        &self,
        compaction_entry: Value,
        from_extension: bool,
        reason: &str,
        will_retry: bool,
    ) {
        let payload = serde_json::json!({
            "type": ext::EVENT_SESSION_COMPACT,
            "compactionEntry": compaction_entry,
            "fromExtension": from_extension,
            "reason": reason,
            "willRetry": will_retry,
        });
        self.host.emit(ext::EVENT_SESSION_COMPACT, payload).await;
    }

    async fn emit_event(&self, event_type: &str, payload: Value) {
        self.host.emit(event_type, payload).await;
    }

    async fn emit_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: Value,
    ) -> Option<ToolCallOutcome> {
        let payload = serde_json::json!({
            "type": ext::EVENT_TOOL_CALL,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "input": input,
        });
        match self.host.emit_tool_call(payload).await {
            Ok(Some(result)) => Some(ToolCallOutcome {
                block: result.get("block").and_then(Value::as_bool),
                reason: result
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                input: result.get("input").cloned().filter(|i| !i.is_null()),
                terminate: result.get("terminate").and_then(Value::as_bool),
            }),
            Ok(None) => None,
            // Upstream rethrows (agent-session.ts:478-484), aborting the run;
            // the infallible hook shape cannot, so mirror the harness
            // precedent: fail-safe block + surface the error to listeners.
            Err(error) => {
                let reason = format!("Extension failed, blocking execution: {}", error.error);
                self.host.emit_error(error);
                Some(ToolCallOutcome {
                    block: Some(true),
                    reason: Some(reason),
                    input: None,
                    terminate: None,
                })
            }
        }
    }

    async fn emit_tool_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: Value,
        content: &[rpi_ai::types::ToolResultContent],
        details: &Value,
        is_error: bool,
        usage: Option<&rpi_ai::types::Usage>,
    ) -> Option<ToolResultPatch> {
        let payload = serde_json::json!({
            "type": ext::EVENT_TOOL_RESULT,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "input": input,
            "content": content,
            "details": details,
            "isError": is_error,
            "usage": usage,
        });
        let result = self.host.emit_tool_result(payload).await?;
        Some(ToolResultPatch {
            content: result
                .get("content")
                .cloned()
                .filter(|c| !c.is_null())
                .and_then(Self::parse),
            details: result.get("details").cloned().filter(|d| !d.is_null()),
            is_error: result.get("isError").and_then(Value::as_bool),
            usage: result
                .get("usage")
                .cloned()
                .filter(|u| !u.is_null())
                .and_then(Self::parse),
        })
    }

    async fn emit_user_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        cwd: &str,
    ) -> Option<crate::tools::bash_executor::BashResult> {
        let payload = serde_json::json!({
            "type": ext::EVENT_USER_BASH,
            "command": command,
            "excludeFromContext": exclude_from_context,
            "cwd": cwd,
        });
        let result = self.host.emit_user_bash(payload).await?;
        if result.get("operations").is_some_and(|o| !o.is_null()) {
            // Custom BashOperations are closure bundles upstream; they cannot
            // cross the JSON dispatch boundary (candidate deviation, T15 W2).
            tracing::warn!(
                "user_bash extension returned custom operations; not supported by the rpi host, ignoring"
            );
        }
        let replacement = result.get("result").cloned().filter(|r| !r.is_null())?;
        Some(crate::tools::bash_executor::BashResult {
            output: replacement
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            exit_code: replacement
                .get("exitCode")
                .and_then(Value::as_i64)
                .map(|code| code as i32),
            cancelled: replacement
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            truncated: replacement
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            full_output_path: replacement
                .get("fullOutputPath")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from),
        })
    }

    async fn emit_session_before_tree(&self) -> Option<SessionBeforeTreeResult> {
        // The `preparation` payload (types.ts:633-637) is assembled by the
        // W2 tree-navigation wiring.
        let payload = serde_json::json!({ "type": ext::EVENT_SESSION_BEFORE_TREE });
        let result = self
            .host
            .emit(ext::EVENT_SESSION_BEFORE_TREE, payload)
            .await?;
        Some(SessionBeforeTreeResult {
            cancel: result.get("cancel").and_then(Value::as_bool),
            summary: result
                .get("summary")
                .cloned()
                .filter(|s| !s.is_null())
                .and_then(|s| {
                    Self::parse::<ext::ExtensionBranchSummary>(s).map(|summary| {
                        crate::core::extensions::ExtensionBranchSummary {
                            summary: summary.summary,
                            details: summary.details,
                            usage: summary.usage,
                        }
                    })
                }),
            custom_instructions: result
                .get("customInstructions")
                .and_then(Value::as_str)
                .map(str::to_owned),
            replace_instructions: result.get("replaceInstructions").and_then(Value::as_bool),
            label: result
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    async fn emit(&self, event_type: &str) {
        let payload = serde_json::json!({ "type": event_type });
        self.host.emit(event_type, payload).await;
    }

    async fn emit_input(
        &self,
        text: &str,
        images: Option<&[ImageContent]>,
        source: InputSource,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> InputEventResult {
        let payload = serde_json::json!({
            "type": ext::EVENT_INPUT,
            "text": text,
            "images": images,
            "source": source.as_str(),
            "streamingBehavior": streaming_behavior.map(|b| b.as_str()),
        });
        let result = self.host.emit_input(payload).await;
        match result.get("action").and_then(Value::as_str) {
            Some("handled") => InputEventResult::Handled,
            Some("transform") => InputEventResult::Transform {
                text: result
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                images: result
                    .get("images")
                    .cloned()
                    .filter(|i| !i.is_null())
                    .and_then(Self::parse),
            },
            _ => InputEventResult::Continue,
        }
    }

    async fn emit_before_agent_start(
        &self,
        text: &str,
        images: Option<&[ImageContent]>,
        system_prompt: &str,
        system_prompt_options: Value,
    ) -> Option<BeforeAgentStartResult> {
        let payload = serde_json::json!({
            "type": ext::EVENT_BEFORE_AGENT_START,
            "prompt": text,
            "images": images,
            "systemPrompt": system_prompt,
            "systemPromptOptions": system_prompt_options,
        });
        let result = self.host.emit_before_agent_start(payload).await?;
        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|m| Self::parse::<ext::BeforeAgentStartMessage>(m.clone()))
                    .map(|m| BeforeAgentStartMessage {
                        custom_type: m.custom_type,
                        content: m.content,
                        display: m.display,
                        details: m.details,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let system_prompt = result
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Some(BeforeAgentStartResult {
            messages,
            system_prompt,
        })
    }

    async fn emit_message_end(&self, message: &AgentMessage) -> Option<AgentMessage> {
        let payload = serde_json::json!({
            "type": ext::EVENT_MESSAGE_END,
            "message": message,
        });
        let result = self.host.emit_message_end(payload).await?;
        Self::parse(result)
    }

    async fn emit_context(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        let Ok(payload) = serde_json::to_value(&messages) else {
            return messages;
        };
        let result = self.host.emit_context(payload).await;
        serde_json::from_value(result).unwrap_or(messages)
    }

    async fn emit_before_provider_request(&self, payload: Value) -> Value {
        self.host.emit_before_provider_request(payload).await
    }

    async fn emit_before_provider_headers(&self, headers: ProviderHeaders) -> ProviderHeaders {
        let Ok(payload) = serde_json::to_value(&headers) else {
            return headers;
        };
        let result = self.host.emit_before_provider_headers(payload).await;
        serde_json::from_value(result).unwrap_or(headers)
    }

    async fn emit_resources_discover(&self, cwd: &str, reason: &str) -> ResourceExtensionPaths {
        let payload = serde_json::json!({
            "type": ext::EVENT_RESOURCES_DISCOVER,
            "cwd": cwd,
            "reason": reason,
        });
        let result = self.host.emit_resources_discover(payload).await;
        ResourceExtensionPaths {
            skill_paths: build_extension_resource_paths(path_entries(result.get("skillPaths"))),
            prompt_paths: build_extension_resource_paths(path_entries(result.get("promptPaths"))),
            theme_paths: build_extension_resource_paths(path_entries(result.get("themePaths"))),
        }
    }

    async fn emit_project_trust(&self, cwd: &Path) -> Option<ProjectTrustEventResult> {
        let payload = serde_json::json!({
            "type": ext::EVENT_PROJECT_TRUST,
            "cwd": cwd.to_string_lossy(),
        });
        let (result, errors) = self.host.emit_project_trust(payload).await;
        // Upstream reports these via a mode callback (project-trust.ts:60-62);
        // the closest seam here is the extension error listeners.
        for error in errors {
            self.host.emit_error(error);
        }
        let result = result?;
        Some(parse_project_trust_result(&result))
    }

    fn get_command(&self, name: &str) -> Option<ExtensionCommand> {
        self.host.get_command(name).map(|command| ExtensionCommand {
            invocation_name: command.invocation_name,
            description: command.description,
            source_info: Some(convert_source_info(&command.source_info)),
        })
    }

    fn get_markdown_transformers(&self) -> Vec<ext::MarkdownTransformerFn> {
        self.host.get_markdown_transformers()
    }

    fn registered_commands(&self) -> Vec<ExtensionCommand> {
        self.host
            .get_registered_commands()
            .into_iter()
            .map(|command| ExtensionCommand {
                invocation_name: command.invocation_name,
                description: command.description,
                source_info: Some(convert_source_info(&command.source_info)),
            })
            .collect()
    }

    async fn execute_extension_command(&self, name: &str, args: &str) -> bool {
        // `_tryExecuteExtensionCommand` (agent-session.ts:1267-1294).
        // Command handlers get the command context (session-control
        // methods, runner.ts:740-777).
        let Some(command) = self.host.get_command(name) else {
            return false;
        };
        let ctx = self.host.create_command_context();
        if let Err(error) = (command.handler)(args.to_owned(), ctx).await {
            self.host.emit_error(ExtensionError::new(
                &format!("command:{name}"),
                "command",
                error,
            ));
        }
        true
    }

    fn extension_tool_entries(&self) -> Vec<ExtensionToolEntry> {
        self.host
            .get_all_registered_tools()
            .into_iter()
            .map(|registered| {
                let definition = registered.definition;
                ExtensionToolEntry {
                    name: definition.name.clone(),
                    description: definition.description.clone(),
                    parameters: definition.parameters.clone(),
                    prompt_snippet: definition.prompt_snippet.clone(),
                    prompt_guidelines: definition.prompt_guidelines.clone(),
                    source_info: convert_source_info(&registered.source_info),
                    tool: Arc::new(HostToolAdapter {
                        host: self.host.clone(),
                        definition,
                    }),
                }
            })
            .collect()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn flag_values(&self) -> HashMap<String, UnknownFlagValue> {
        self.host
            .runtime()
            .flag_values()
            .into_iter()
            .map(|(name, value)| {
                let value = match value {
                    ext::FlagValue::Boolean(b) => UnknownFlagValue::Boolean(b),
                    ext::FlagValue::String(s) => UnknownFlagValue::String(s),
                };
                (name, value)
            })
            .collect()
    }

    fn set_flag_value(&self, name: &str, value: UnknownFlagValue) {
        let value = match value {
            UnknownFlagValue::Boolean(b) => ext::FlagValue::Boolean(b),
            UnknownFlagValue::String(s) => ext::FlagValue::String(s),
        };
        self.host.runtime().set_flag_value(name, value);
    }

    fn invalidate(&self, message: &str) {
        self.host.invalidate(Some(message.to_owned()));
    }

    fn on_error(&self, listener: ExtensionErrorListener) -> Option<Box<dyn FnOnce() + Send>> {
        let host_listener = Arc::new(move |error: ExtensionError| {
            listener(ExtensionErrorInfo {
                extension_path: error.extension_path,
                event: error.event,
                error: error.error,
            });
        });
        Some(self.host.on_error(host_listener))
    }

    fn emit_error(&self, error: ExtensionErrorInfo) {
        self.host.emit_error(ExtensionError::new(
            &error.extension_path,
            &error.event,
            error.error,
        ));
    }
}
