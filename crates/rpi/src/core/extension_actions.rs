//! `HostActions` implementation bound to an `AgentSession` (T15 W3) — the
//! action half of upstream `runner.bindCore(...)` (agent-session.ts:2356-2439)
//! plus `exec` (exec.ts `execCommand`) and provider registration
//! (agent-session.ts:2433-2438).
//!
//! The actions hold a [`WeakAgentSession`] to break the session → runner
//! ref → host → actions → session Arc cycle; after the session drops,
//! value-returning methods degrade to empty defaults and the rest no-op.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::types::{ImageContent, UserContent};
use rpi_ext_host::api::{
    DeliverAs, ExecOptions, ExecResult, HostActions, SendMessageOptions, SendUserMessageOptions,
};
use rpi_ext_host::error::ExtError;
use serde_json::Value;

use crate::core::agent_session::{AgentSession, CustomDeliverAs, WeakAgentSession};
use crate::core::extensions::StreamingBehavior;

/// Build and bind the session-backed host actions
/// (`runner.bindCore(actions, ...)`, agent-session.ts:2356).
pub async fn bind_session_actions(
    host: &Arc<rpi_ext_host::host::NativeExtensionHost>,
    session: &AgentSession,
) {
    let actions = Arc::new(SessionHostActions {
        session: session.downgrade(),
    });
    host.bind_actions(actions).await;
    // `ExtensionContextActions` half (agent-session.ts:2405-2431,
    // runner.ts:336-347).
    host.runtime().set_context_actions(Some(Arc::new(
        crate::core::extension_context::SessionContextActions::new(session),
    )));
}

struct SessionHostActions {
    session: WeakAgentSession,
}

impl SessionHostActions {
    fn session(&self) -> Option<AgentSession> {
        self.session.upgrade()
    }

    /// Fire-and-forget with the upstream `.catch(emitError)` mapping
    /// (agent-session.ts:2357-2374).
    fn spawn_reporting(
        &self,
        event: &'static str,
        future: impl std::future::Future<Output = Result<(), crate::error::RpiError>> + Send + 'static,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = future.await {
                session.extension_runner().emit_error(
                    crate::core::extensions::ExtensionErrorInfo {
                        extension_path: "<runtime>".to_owned(),
                        event: event.to_owned(),
                        error: error.to_string(),
                    },
                );
            }
        });
    }
}

#[async_trait]
impl HostActions for SessionHostActions {
    /// `sendMessage` → `sendCustomMessage` (agent-session.ts:2357-2365).
    fn send_message(&self, message: Value, options: Option<SendMessageOptions>) {
        let Some(session) = self.session() else {
            return;
        };
        let custom_type = message
            .get("customType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let content: Option<UserContent> = message
            .get("content")
            .cloned()
            .filter(|c| !c.is_null())
            .and_then(|c| serde_json::from_value(c).ok());
        let display = message
            .get("display")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let details = message.get("details").cloned().filter(|d| !d.is_null());
        let options = options.unwrap_or_default();
        let deliver_as = options.deliver_as.map(|deliver| match deliver {
            DeliverAs::Steer => CustomDeliverAs::Steer,
            DeliverAs::FollowUp => CustomDeliverAs::FollowUp,
            DeliverAs::NextTurn => CustomDeliverAs::NextTurn,
        });
        self.spawn_reporting("send_message", async move {
            session
                .send_custom_message(
                    &custom_type,
                    content,
                    display,
                    details,
                    options.trigger_turn.unwrap_or(false),
                    deliver_as,
                )
                .await
        });
    }

    /// `sendUserMessage` → `sendUserMessage` (agent-session.ts:2366-2373);
    /// content normalization at agent-session.ts:1476-1492.
    fn send_user_message(&self, content: Value, options: Option<SendUserMessageOptions>) {
        let Some(session) = self.session() else {
            return;
        };
        let (text, images) = normalize_user_message_content(content);
        let deliver_as = options
            .and_then(|o| o.deliver_as)
            .map(|deliver| match deliver {
                DeliverAs::FollowUp => StreamingBehavior::FollowUp,
                // `sendUserMessage` has no `nextTurn` upstream (types.ts:1292).
                DeliverAs::Steer | DeliverAs::NextTurn => StreamingBehavior::Steer,
            });
        self.spawn_reporting("send_user_message", async move {
            session.send_user_message(&text, images, deliver_as).await
        });
    }

    /// `appendEntry` (agent-session.ts:2375-2382).
    fn append_entry(&self, custom_type: &str, data: Option<Value>) {
        if let Some(session) = self.session() {
            session.append_entry(custom_type, data);
        }
    }

    /// `setSessionName` (agent-session.ts:2383-2385) — fires
    /// `session_info_changed` inside.
    fn set_session_name(&self, name: &str) {
        if let Some(session) = self.session() {
            session.set_session_name(name);
        }
    }

    /// `getSessionName` (agent-session.ts:2386-2388).
    fn get_session_name(&self) -> Option<String> {
        self.session().and_then(|session| session.session_name())
    }

    /// `setLabel` (agent-session.ts:2389-2391); `None` clears.
    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        if let Some(session) = self.session() {
            let result = session
                .session_manager()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_label_change(entry_id, label);
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
        }
    }

    /// `exec` (loader.ts:334-337 → exec.ts `execCommand`): the default cwd
    /// is the extension's load-time cwd, which the host sets to the session
    /// cwd (`resolvePath(cwd)`, loader.ts:493).
    async fn exec(
        &self,
        command: &str,
        args: &[String],
        options: Option<ExecOptions>,
    ) -> Result<ExecResult, ExtError> {
        let cwd = options
            .as_ref()
            .and_then(|o| o.cwd.clone())
            .unwrap_or_else(|| {
                self.session()
                    .map(|session| session.cwd().to_owned())
                    .unwrap_or_else(|| "/".to_owned())
            });
        let timeout_ms = options.as_ref().and_then(|o| o.timeout);
        Ok(exec_command(command, args, &cwd, timeout_ms).await)
    }

    /// `getActiveTools` (agent-session.ts:2393).
    fn get_active_tools(&self) -> Vec<String> {
        self.session()
            .map(|session| session.get_active_tool_names())
            .unwrap_or_default()
    }

    /// `getAllTools` (agent-session.ts:2394) — `ToolInfo[]` JSON.
    fn get_all_tools(&self) -> Vec<Value> {
        self.session()
            .map(|session| session.get_all_tools())
            .unwrap_or_default()
    }

    /// `setActiveTools` (agent-session.ts:2395 → :926-941): unknown names
    /// are silently ignored inside `set_active_tools_by_name`.
    fn set_active_tools(&self, tool_names: Vec<String>) {
        if let Some(session) = self.session() {
            session.set_active_tools_by_name(tool_names);
        }
    }

    /// `refreshTools` (agent-session.ts:2395 → `_refreshToolRegistry`).
    fn refresh_tools(&self) {
        if let Some(session) = self.session() {
            session.refresh_extension_tools();
        }
    }

    /// `getCommands` (agent-session.ts:2396 → :2332-2355).
    fn get_commands(&self) -> Vec<Value> {
        self.session()
            .map(|session| session.get_commands_info())
            .unwrap_or_default()
    }

    /// `setModel` (agent-session.ts:2397-2400): no configured auth → false,
    /// no switch. Thinking-level re-clamping happens inside `set_model`.
    async fn set_model(&self, model: Value) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let model: rpi_ai::types::Model = match serde_json::from_value(model) {
            Ok(model) => model,
            Err(error) => {
                tracing::warn!("extension setModel with malformed model: {error}");
                return false;
            }
        };
        if !session.model_runtime().has_configured_auth(&model.provider) {
            return false;
        }
        session.set_model(model).await.is_ok()
    }

    /// `getThinkingLevel` (agent-session.ts:2401).
    fn get_thinking_level(&self) -> String {
        self.session()
            .map(|session| thinking_level_str(session.thinking_level()).to_owned())
            .unwrap_or_else(|| "off".to_owned())
    }

    /// `setThinkingLevel` (agent-session.ts:2402): clamps inside
    /// `set_thinking_level`, fires `thinking_level_select` on change.
    fn set_thinking_level(&self, level: &str) {
        if let Some(session) = self.session() {
            match crate::cli::args::parse_thinking_level(level) {
                Some(level) => session.set_thinking_level(level),
                None => tracing::warn!("extension setThinkingLevel with invalid level: {level}"),
            }
        }
    }

    /// `registerProvider(name, config)` (agent-session.ts:2433-2436 +
    /// runner.ts:387-393). Closure-bearing `ProviderConfig` fields
    /// (`streamSimple` / `oauth` / `refreshModels`) cannot cross the JSON
    /// boundary — they are rejected loudly, not silently dropped.
    async fn register_provider(&self, name: &str, config: Value) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Err("session is gone".to_owned());
        };
        for key in ["streamSimple", "oauth", "refreshModels"] {
            if config.get(key).is_some_and(|v| !v.is_null()) {
                return Err(format!(
                    "ProviderConfig.{key} is not supported by the rpi host (T15 candidate deviation)"
                ));
            }
        }
        let input: crate::core::model_runtime::ProviderConfigInput =
            serde_json::from_value(config).map_err(|error| error.to_string())?;
        session
            .model_runtime()
            .register_provider(name, input)
            .await?;
        Ok(())
    }

    /// `registerProvider(provider)` — native overload
    /// (agent-session.ts:2437-2439).
    async fn register_native_provider(
        &self,
        provider: Arc<dyn rpi_ai::models::Provider>,
    ) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Err("session is gone".to_owned());
        };
        session
            .model_runtime()
            .register_native_provider(provider)
            .await
    }

    /// `unregisterProvider` (agent-session.ts:2440-2442 →
    /// custom-provider.md:190-217): the runtime recomposes the provider and
    /// restores built-in models.
    async fn unregister_provider(&self, name: &str) {
        if let Some(session) = self.session() {
            session.model_runtime().unregister_provider(name).await;
        }
    }

    // -- v0.11 model-registry actions (model-registry.ts @ 4181f66) ---------

    /// `ctx.modelRegistry.complete(model, context, options?)`
    /// (model-registry.ts:138-142 @ 4181f66). `options` is accepted but not
    /// deserialized — the extension host call path passes `None` (the full
    /// `StreamOptions` includes non-serializable callback fields).
    async fn model_registry_complete(
        &self,
        model: Value,
        context: Value,
        _options: Option<Value>,
    ) -> Option<Value> {
        let session = self.session()?;
        let model: rpi_ai::types::Model = serde_json::from_value(model).ok()?;
        let context: rpi_ai::types::Context = serde_json::from_value(context).ok()?;
        let message = session
            .model_runtime()
            .complete(&model, &context, None)
            .await?;
        serde_json::to_value(message).ok()
    }

    /// `ctx.modelRegistry.find(provider, modelId)` (model-registry.ts:70).
    fn model_registry_find(&self, provider: &str, model_id: &str) -> Option<Value> {
        let session = self.session()?;
        let model = session.model_runtime().find_model(provider, model_id)?;
        serde_json::to_value(model).ok()
    }

    /// `ctx.modelRegistry.hasConfiguredAuth(providerId)`
    /// (model-registry.ts:76).
    fn model_registry_has_configured_auth(&self, provider_id: &str) -> bool {
        self.session()
            .map(|session| session.model_runtime().has_configured_auth(provider_id))
            .unwrap_or(false)
    }

    /// `ctx.modelRegistry.getApiKeyAndHeaders(model)` (model-registry.ts:64-93
    /// @ 4181f66). **#7030**: null header deletion markers MUST pass through
    /// unchanged — we serialize `AuthResult` without stripping `None`/`null`
    /// from the `ProviderHeaders` map.
    async fn get_api_key_and_headers(&self, model: Value) -> Value {
        let Some(session) = self.session() else {
            return serde_json::json!({"ok": false, "error": "session is gone"});
        };
        let model: rpi_ai::types::Model = match serde_json::from_value(model) {
            Ok(m) => m,
            Err(e) => {
                return serde_json::json!({"ok": false, "error": e.to_string()});
            }
        };
        let runtime = session.model_runtime();
        match runtime.get_auth(&model, None).await {
            Ok(Some(auth_result)) => {
                let auth = &auth_result.auth;
                // #7030: Preserve null header deletion markers exactly.
                // ProviderHeaders = HashMap<String, Option<String>>; None
                // values are the deletion markers — they must serialize as
                // JSON `null`, not be stripped.
                let headers_map: std::collections::HashMap<String, Option<String>> =
                    auth.headers.clone().unwrap_or_default();
                let mut result = serde_json::json!({
                    "ok": true,
                    "apiKey": auth.api_key,
                    "headers": headers_map,
                });
                if let Some(base_url) = &auth.base_url {
                    result["baseUrl"] = serde_json::json!(base_url);
                }
                if let Some(env) = &auth_result.env {
                    result["env"] = serde_json::json!(env);
                }
                result
            }
            Ok(None) => {
                // No auth resolved: check if provider requires an auth header
                let compat = runtime.get_compatibility_request_config(&model);
                if compat.auth_header {
                    serde_json::json!({"ok": false, "error": format!("No API key found for \"{}\"", model.provider)})
                } else {
                    // Return headers without API key (extension can still use
                    // provider-specific headers).
                    let headers_map: std::collections::HashMap<String, Option<String>> = compat
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    serde_json::json!({"ok": true, "headers": headers_map})
                }
            }
            Err(error) => serde_json::json!({"ok": false, "error": error.message}),
        }
    }

    /// `ctx.setRuntimeApiKey(providerId, apiKey)` (model-runtime.ts:536-547
    /// @ 4181f66) — async, serialized per-provider.
    async fn set_runtime_api_key(
        &self,
        provider_id: &str,
        api_key: &str,
        _options: Option<rpi_ext_host::types::AuthOperationOptions>,
    ) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Err("session is gone".to_owned());
        };
        session
            .model_runtime()
            .set_runtime_api_key(provider_id, api_key)
            .await
            .map_err(|e| e.to_string())
    }

    /// `ctx.removeRuntimeApiKey(providerId)` (model-runtime.ts:549-560).
    async fn remove_runtime_api_key(&self, provider_id: &str) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Err("session is gone".to_owned());
        };
        session
            .model_runtime()
            .remove_runtime_api_key(provider_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// `sendUserMessage` content normalization (agent-session.ts:1476-1492):
/// string passes through; block arrays split into joined text + images.
fn normalize_user_message_content(content: Value) -> (String, Option<Vec<ImageContent>>) {
    if let Some(text) = content.as_str() {
        return (text.to_owned(), None);
    }
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<ImageContent> = Vec::new();
    for part in content.as_array().cloned().unwrap_or_default() {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_owned());
                }
            }
            Some("image") => {
                if let Ok(image) = serde_json::from_value::<ImageContent>(part) {
                    images.push(image);
                }
            }
            _ => {}
        }
    }
    let images = if images.is_empty() {
        None
    } else {
        Some(images)
    };
    (text_parts.join("\n"), images)
}

/// `execCommand` (exec.ts:34-106): spawn without a shell, capture
/// stdout/stderr, `killed` marks a timeout kill. Spawn failure resolves
/// with `code: 1` (exec.ts:98-103 catch branch).
async fn exec_command(
    command: &str,
    args: &[String],
    cwd: &str,
    timeout_ms: Option<u64>,
) -> ExecResult {
    let spawned = tokio::process::Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(_) => {
            return ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                code: 1,
                killed: false,
            }
        }
    };
    // Timeout: SIGKILL by pid, then collect whatever output was produced.
    // Deviation: upstream sends SIGTERM with a 5s SIGKILL escalation
    // (exec.ts:50-58); the bash tool's libc kill helpers set the precedent
    // for direct signals.
    let child_id = child.id();
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let (output, killed) = match timeout_ms {
        Some(timeout_ms) if timeout_ms > 0 => {
            tokio::select! {
                output = &mut wait => (output, false),
                () = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                    if let Some(pid) = child_id {
                        #[cfg(unix)]
                        // pid of a live child we own; SIGKILL is always safe to send.
                        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        #[cfg(not(unix))]
                        let _ = pid;
                    }
                    (wait.await, true)
                }
            }
        }
        _ => (wait.await, false),
    };
    match output {
        Ok(output) => ExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(0),
            killed,
        },
        Err(_) => ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            code: 1,
            killed,
        },
    }
}

fn thinking_level_str(level: rpi_agent::types::ThinkingLevel) -> &'static str {
    match level {
        rpi_agent::types::ThinkingLevel::Off => "off",
        rpi_agent::types::ThinkingLevel::Minimal => "minimal",
        rpi_agent::types::ThinkingLevel::Low => "low",
        rpi_agent::types::ThinkingLevel::Medium => "medium",
        rpi_agent::types::ThinkingLevel::High => "high",
        rpi_agent::types::ThinkingLevel::Xhigh => "xhigh",
        rpi_agent::types::ThinkingLevel::Max => "max",
    }
}
