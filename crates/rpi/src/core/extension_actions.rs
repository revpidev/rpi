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
    DeliverAs, ExecOptions, ExecResult, ExecuteToolOptions, ExecuteToolOutcome, HostActions,
    SendMessageOptions, SendUserMessageOptions,
};
use rpi_ext_host::error::ExtError;
use serde_json::Value;

use crate::core::agent_session::{AgentSession, CustomDeliverAs, WeakAgentSession};
use crate::core::extensions::StreamingBehavior;

/// An error `ClassifierResult` (`classify` never rejects;
/// model-registry.ts:173-179).
fn classifier_error_result(message: impl Into<String>) -> Value {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let result = rpi_ai::types::ClassifierResult {
        api: rpi_ai::types::ApiKind::from(""),
        provider: String::new(),
        model: String::new(),
        answers: std::collections::BTreeMap::new(),
        usage: None,
        stop_reason: rpi_ai::types::ClassifierStopReason::Error,
        error_message: Some(message.into()),
        timestamp,
    };
    serde_json::to_value(result).unwrap_or(Value::Null)
}

/// Parse the portable subset of `ModelsClassifierOptions` from the JSON
/// boundary (camelCase). Callback-bearing fields cannot cross and are
/// ignored; `signal` is absorbed by the request-time credential paths.
fn parse_classifier_options(options: &Value) -> Option<rpi_ai::types::ClassifierOptions> {
    let object = options.as_object()?;
    let mut parsed = rpi_ai::types::ClassifierOptions {
        api_key: object
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_owned),
        headers: object
            .get("headers")
            .and_then(|headers| serde_json::from_value(headers.clone()).ok()),
        env: object
            .get("env")
            .and_then(|env| serde_json::from_value(env.clone()).ok()),
        timeout_ms: object.get("timeoutMs").and_then(Value::as_u64),
        max_retries: object
            .get("maxRetries")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        max_retry_delay_ms: object.get("maxRetryDelayMs").and_then(Value::as_u64),
        temperature: object.get("temperature").and_then(Value::as_f64),
        ..Default::default()
    };
    // An all-empty options object is equivalent to none (upstream
    // `options?` semantics).
    if parsed
        .headers
        .as_ref()
        .is_some_and(|headers| headers.is_empty())
    {
        parsed.headers = None;
    }
    Some(parsed)
}

/// A stream that terminates immediately with an `error` event (and error
/// result) — the `ctx.modelRegistry.stream()` answer for setup failures
/// (#8964: "Setup failures produce error events and error results",
/// docs/extensions.md streaming-model-calls section).
fn setup_error_stream(message: &str) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
    use rpi_ai::types::{
        ApiKind, AssistantMessage, AssistantRole, ErrorReason, StopReason, StreamEvent, Usage,
    };
    let stream = rpi_ai::utils::event_stream::AssistantMessageEventStream::new();
    stream.push(StreamEvent::Error {
        reason: ErrorReason::Error,
        error: AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![],
            api: ApiKind::from(""),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(message.to_owned()),
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        },
    });
    stream.end(None);
    stream
}

/// Build and bind the session-backed host actions
/// (`runner.bindCore(actions, ...)`, agent-session.ts:2356).
pub async fn bind_session_actions(
    host: &Arc<rpi_ext_host::host::NativeExtensionHost>,
    session: &AgentSession,
) {
    let actions = Arc::new(SessionHostActions {
        session: session.downgrade(),
        usage: session.usage_registry(),
        host_handle: tokio::runtime::Handle::current(),
    });
    host.bind_actions(actions).await;
    // V16-08 FR-E/H: extensions registering or unregistering an MCP server
    // notify every `mcp_servers_change` handler (loader.ts:464-492 +
    // runner.ts `applyRuntimeChange`). The listener holds a weak session so
    // a stale registry never keeps one alive.
    {
        let registry = host.runtime().mcp_servers();
        let weak = session.downgrade();
        let host_handle = tokio::runtime::Handle::current();
        let registry_for_emit = registry.clone();
        let listener: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let weak = weak.clone();
            let registry = registry_for_emit.clone();
            let handle = host_handle.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                handle.spawn(async move {
                    if let Some(session) = weak.upgrade() {
                        let servers = registry
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .to_json();
                        session
                            .extension_runner()
                            .emit_event(
                                rpi_ext_host::types::EVENT_MCP_SERVERS_CHANGE,
                                serde_json::json!({
                                    "type": rpi_ext_host::types::EVENT_MCP_SERVERS_CHANGE,
                                    "servers": servers,
                                }),
                            )
                            .await;
                    }
                });
            }));
        });
        registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_change_listener(Some(listener));
    }
    // `ExtensionContextActions` half (agent-session.ts:2405-2431,
    // runner.ts:336-347).
    host.runtime().set_context_actions(Some(Arc::new(
        crate::core::extension_context::SessionContextActions::new(session),
    )));
}

struct SessionHostActions {
    session: WeakAgentSession,
    /// V16-05 FR-A: the session's usage-provider registry (created with the
    /// session; shared across rebinds of the same session).
    usage: Arc<crate::core::usage_providers::UsageProviderRegistry>,
    /// Host runtime handle captured at bind time. `sendMessage` /
    /// `sendUserMessage` arrive as SYNCHRONOUS extension-ABI callbacks, so
    /// the calling thread can be a plugin-owned runtime worker or even a
    /// bare `std::thread` — `tokio::spawn` there would either land the
    /// future on the wrong runtime or panic with "there is no reactor
    /// running" (and that panic crosses the `extern "C"` trampoline, which
    /// aborts the process). Spawning on this handle keeps the future on
    /// the host runtime regardless of the caller's thread.
    host_handle: tokio::runtime::Handle,
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
        let future = async move {
            if let Err(error) = future.await {
                session.extension_runner().emit_error(
                    crate::core::extensions::ExtensionErrorInfo {
                        extension_path: "<runtime>".to_owned(),
                        event: event.to_owned(),
                        error: error.to_string(),
                    },
                );
            }
        };
        // Late calls can race host-runtime shutdown at process exit (a
        // plugin's background runner delivering its final notification):
        // `Handle::spawn` panics once the runtime is dropped, and this
        // whole call chain sits under the `extern "C"` ABI trampoline
        // where a panic aborts. Swallow that shutdown race — there is no
        // session loop left to receive the report anyway.
        let handle = self.host_handle.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            handle.spawn(future);
        }));
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
                    // `triggerTurn` crosses as `undefined` when unset — the
                    // session branches on `!= Some(false)` (steer) vs
                    // `== Some(true)` (run), mirroring agent-session.ts:1497-1508.
                    options.trigger_turn,
                    deliver_as,
                )
                .await
        });
    }

    /// `sendUserMessage` → `sendUserMessage` (agent-session.ts:2366-2373);
    /// content normalization at agent-session.ts:1476-1492.
    /// `expandPromptTemplates` rides through (b987ead35, V14-11 FR-D).
    fn send_user_message(&self, content: Value, options: Option<SendUserMessageOptions>) {
        let Some(session) = self.session() else {
            return;
        };
        let (text, images) = normalize_user_message_content(content);
        let (deliver_as, expand_prompt_templates) = options
            .map(|options| {
                let deliver_as = options.deliver_as.map(|deliver| match deliver {
                    DeliverAs::FollowUp => StreamingBehavior::FollowUp,
                    // `sendUserMessage` has no `nextTurn` upstream (types.ts:1292).
                    DeliverAs::Steer | DeliverAs::NextTurn => StreamingBehavior::Steer,
                });
                (deliver_as, options.expand_prompt_templates)
            })
            .unwrap_or((None, None));
        self.spawn_reporting("send_user_message", async move {
            session
                .send_user_message(&text, images, deliver_as, expand_prompt_templates)
                .await
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

    /// `getMode()` (V16-05 FR-B R4): the session's permission mode wire
    /// value. Non-interactive sessions answer `"default"` (the gate lives
    /// in `AgentSession::permission_mode`).
    fn get_mode(&self) -> String {
        self.session()
            .map(|session| session.permission_mode().as_str().to_owned())
            .unwrap_or_else(|| "default".to_owned())
    }

    /// `setMode(mode)` (V16-05 FR-B R4): unknown values are ignored; the
    /// session path dispatches `mode_change` on an actual change.
    fn set_mode(&self, mode: &str) {
        let Some(session) = self.session() else {
            return;
        };
        match crate::core::permission_mode::PermissionMode::parse(mode) {
            Some(mode) => session.set_permission_mode(mode),
            None => tracing::warn!("extension setMode with invalid mode: {mode}"),
        }
    }

    // -- V16-05 FR-A usage-provider framework ---------------------------

    /// `ctx.usage.listProviders()`: explicit settings ∪ user dir ∪ plugin
    /// registrations.
    fn usage_list_providers(&self) -> Vec<String> {
        self.usage.list_providers()
    }

    /// `ctx.usage.fetch(provider, force?)`: serialized script execution with
    /// the last-success cache; failures answer the cached success.
    async fn usage_fetch(&self, provider: &str, force: bool) -> Option<Value> {
        self.usage.fetch(provider, force).await
    }

    /// `ctx.usage.register(provider, scriptPath)`: plugin registrations are
    /// the lowest resolution priority.
    fn usage_register(&self, provider: &str, script_path: &str) -> Result<(), String> {
        self.usage.register(provider, script_path)
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

    /// `registerVirtualModel(definition)` (agent-session.ts:2443-2446 @
    /// upstream v0.99.0 + `virtual-models.ts:955-974`). The route callback is
    /// created by the carrier's host-call layer (wasm/native), which is the
    /// only place that holds the guest dispatch handle.
    async fn register_virtual_model(
        &self,
        definition: Value,
        route: rpi_ext_host::types::VirtualModelRouteFn,
    ) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Err("session is gone".to_owned());
        };
        session
            .model_runtime()
            .register_virtual_model(definition, route)
            .await
    }

    /// `unregisterVirtualModel(provider, id)` (agent-session.ts:2447-2450).
    async fn unregister_virtual_model(&self, provider: &str, id: &str) {
        if let Some(session) = self.session() {
            session
                .model_runtime()
                .unregister_virtual_model(provider, id)
                .await;
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

    /// `ctx.modelRegistry.findOfType(type, provider, modelId)`
    /// (model-registry.ts:76-77 @ a13d35a74): non-chat catalog lookup over
    /// the runtime's typed accessors (V16-06 R2.5.2).
    fn model_registry_find_of_type(
        &self,
        model_type: &str,
        provider: &str,
        model_id: &str,
    ) -> Option<Value> {
        let model_type = match model_type {
            "chat" => rpi_ai::types::ModelType::Chat,
            "image" => rpi_ai::types::ModelType::Image,
            "classifier" => rpi_ai::types::ModelType::Classifier,
            _ => return None,
        };
        let session = self.session()?;
        let model = session
            .model_runtime()
            .get_model_of_type(model_type, provider, model_id)?;
        serde_json::to_value(model).ok()
    }

    /// `ctx.modelRegistry.classify(model, context, options?)`
    /// (model-registry.ts:173-179 @ a13d35a74): structured classification
    /// with request-time authentication. Never rejects — parse failures and
    /// an unbound session produce an error `ClassifierResult`. The
    /// callback-bearing option fields (`signal`/`fetch`/`onPayload`/
    /// `onResponse`) cannot cross the JSON boundary and are ignored.
    async fn model_registry_classify(
        &self,
        model: Value,
        context: Value,
        options: Option<Value>,
    ) -> Option<Value> {
        let Some(session) = self.session() else {
            return Some(classifier_error_result("session is gone"));
        };
        let model: rpi_ai::types::ClassifierModel = match serde_json::from_value(model) {
            Ok(model) => model,
            Err(error) => {
                return Some(classifier_error_result(format!(
                    "bad classifier model JSON: {error}"
                )));
            }
        };
        let context: rpi_ai::types::ClassifierContext = match serde_json::from_value(context) {
            Ok(context) => context,
            Err(error) => {
                return Some(classifier_error_result(format!(
                    "bad classifier context JSON: {error}"
                )));
            }
        };
        let options = options.as_ref().and_then(parse_classifier_options);
        let result = session
            .model_runtime()
            .classify(&model, &context, options.as_ref())
            .await;
        serde_json::to_value(result).ok()
    }

    /// `ctx.modelRegistry.stream(model, context, options)` (#8964,
    /// 1f78cea7a — model-registry.ts:105-110): direct delegation to the
    /// runtime's streaming facade (same lazy auth resolution + header merge
    /// as built-in requests). An unbound session answers with an error
    /// event stream — upstream setup failures "produce error events and
    /// error results" (docs/extensions.md, #8964 section).
    fn model_registry_stream(
        &self,
        model: rpi_ai::types::Model,
        context: rpi_ai::types::Context,
        options: Option<rpi_ai::models::ModelsStreamOptions>,
    ) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
        match self.session() {
            Some(session) => session.model_runtime().stream(&model, &context, options),
            None => setup_error_stream("model registry is not bound to a session"),
        }
    }

    /// `ctx.modelRegistry.streamSimple(model, context, options)` (#8964,
    /// model-registry.ts:112-116).
    fn model_registry_stream_simple(
        &self,
        model: rpi_ai::types::Model,
        context: rpi_ai::types::Context,
        options: Option<rpi_ai::models::ModelsSimpleStreamOptions>,
    ) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
        match self.session() {
            Some(session) => session
                .model_runtime()
                .stream_simple(&model, &context, options),
            None => setup_error_stream("model registry is not bound to a session"),
        }
    }

    /// `ctx.modelRegistry.hasConfiguredAuth(providerId)`
    /// (model-registry.ts:76).
    fn model_registry_has_configured_auth(&self, provider_id: &str) -> bool {
        self.session()
            .map(|session| session.model_runtime().has_configured_auth(provider_id))
            .unwrap_or(false)
    }

    /// `ctx.modelRegistry.getApiKeyAndHeaders(model)` (model-registry.ts:64-93
    /// @ 4181f66). Aligns with upstream key-omission semantics: JS
    /// `JSON.stringify` drops `undefined` keys, so `apiKey`/`headers`/`baseUrl`
    /// /`env` are each omitted when absent. **#7030**: null header deletion
    /// markers inside the headers map still serialize as JSON `null`.
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
                // model-registry.ts:74-80: success branch emits `{ ok, apiKey,
                // headers, baseUrl?, env }` but JS `JSON.stringify` omits
                // `undefined` keys — so apiKey/headers/baseUrl/env are each
                // omitted when absent. #7030: null header deletion markers
                // inside the headers map are still serialized as `null`.
                let mut result = serde_json::json!({"ok": true});
                if let Some(api_key) = &auth.api_key {
                    result["apiKey"] = serde_json::json!(api_key);
                }
                if let Some(headers) = &auth.headers
                    && !headers.is_empty()
                {
                    let headers_map: std::collections::HashMap<String, Option<String>> =
                        headers.clone();
                    result["headers"] = serde_json::json!(headers_map);
                }
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
                    // model-registry.ts:72: `{ ok: true, headers }` — `headers`
                    // is omitted when the map is empty (JS `undefined`).
                    if compat.headers.is_empty() {
                        serde_json::json!({"ok": true})
                    } else {
                        let headers_map: std::collections::HashMap<String, Option<String>> =
                            compat.headers.clone();
                        serde_json::json!({"ok": true, "headers": headers_map})
                    }
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

    /// Backs `ExtensionToolContext.executeTool()` (types.ts:390-394 @
    /// a13d35a74; V16-06 FR-E): run a nested tool call through the session's
    /// nested-call runner (same validation, hooks, and permission checks as
    /// model-issued calls). Tool failures come back as `is_error: true`.
    async fn execute_tool(
        &self,
        caller_id: &str,
        name: &str,
        args: Value,
        options: Option<ExecuteToolOptions>,
    ) -> Result<ExecuteToolOutcome, ExtError> {
        let Some(session) = self.session() else {
            return Err(ExtError::Unbound("session is gone".to_owned()));
        };
        let options = options.unwrap_or_default();
        let outcome = session
            .execute_nested_tool_call(
                caller_id,
                name,
                args,
                rpi_agent::nested_tool_calls::NestedToolCallOptions {
                    signal: options.signal,
                    on_update: options.on_update,
                },
            )
            .await
            .map_err(|error| ExtError::Call(error.to_string()))?;
        Ok(ExecuteToolOutcome {
            tool_call: serde_json::to_value(&outcome.tool_call).unwrap_or(Value::Null),
            result: outcome.result,
            is_error: outcome.is_error,
        })
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
    let outcome = exec_script(ScriptExecRequest {
        command,
        args,
        cwd,
        timeout_ms,
        stdin: None,
        env: &[],
        max_stdout_bytes: None,
    })
    .await;
    ExecResult {
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        code: outcome.code,
        killed: outcome.killed,
    }
}

/// One request through the shared exec channel ([`exec_script`]).
///
/// The optional knobs are host-internal additions for the V16-05 usage
/// provider runner (stdin context, credential environment, stdout cap);
/// `HostActions::exec` leaves them empty and keeps the upstream shape.
/// `ScriptExecRequest` is `pub(crate)` because spawning must stay on this
/// channel — no second subprocess API is exposed.
pub(crate) struct ScriptExecRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub cwd: &'a str,
    pub timeout_ms: Option<u64>,
    pub stdin: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub max_stdout_bytes: Option<usize>,
}

/// Outcome of [`exec_script`]. `stdout_overflow` marks a cap kill; the
/// caller treats it as a failed run.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptExecOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
    pub stdout_overflow: bool,
    pub spawn_failed: bool,
}

/// Shared exec channel: spawn without a shell, write optional stdin, read
/// stdout/stderr (optionally capped — overflow kills the child), and reap.
/// A timeout kills by pid and still drains/reaps the child so no zombie is
/// left behind (V16-05 FR-A R3, statusline kill+reap precedent).
pub(crate) async fn exec_script(request: ScriptExecRequest<'_>) -> ScriptExecOutcome {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new(request.command);
    command
        .args(request.args)
        .current_dir(request.cwd)
        .stdin(if request.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, value) in request.env {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return ScriptExecOutcome {
                code: 1,
                spawn_failed: true,
                ..ScriptExecOutcome::default()
            };
        }
    };

    if let Some(input) = request.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output_limit = request.max_stdout_bytes.unwrap_or(usize::MAX);
    let mut stdout_task = tokio::spawn(read_capped(stdout, output_limit, pid));
    let mut stderr_task = tokio::spawn(read_capped(stderr, output_limit, pid));
    let wait = child.wait();
    tokio::pin!(wait);

    let read_and_wait = async {
        let stdout = (&mut stdout_task).await.unwrap_or_default();
        let stderr = (&mut stderr_task).await.unwrap_or_default();
        let status = (&mut wait).await.ok();
        (stdout.0, stdout.1, stderr.0, status)
    };

    let (stdout_text, stdout_overflow, stderr_text, status, timed_out) = match request.timeout_ms {
        Some(timeout_ms) if timeout_ms > 0 => {
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read_and_wait)
                .await
            {
                Ok((stdout, overflow, stderr, status)) => (stdout, overflow, stderr, status, false),
                Err(_) => {
                    // Timeout: kill by pid, then drain/reap under a short
                    // bound so a descendant holding the pipes open cannot
                    // hang the fetch.
                    kill_process(pid);
                    let drained = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                        let stdout = (&mut stdout_task).await.unwrap_or_default();
                        let stderr = (&mut stderr_task).await.unwrap_or_default();
                        let status = (&mut wait).await.ok();
                        (stdout.0, stdout.1, stderr.0, status)
                    })
                    .await
                    .unwrap_or_default();
                    (drained.0, drained.1, drained.2, drained.3, true)
                }
            }
        }
        _ => {
            let (stdout, overflow, stderr, status) = read_and_wait.await;
            (stdout, overflow, stderr, status, false)
        }
    };

    ScriptExecOutcome {
        stdout: stdout_text,
        stderr: stderr_text,
        code: status.and_then(|status| status.code()).unwrap_or(0),
        killed: timed_out,
        stdout_overflow,
        spawn_failed: false,
    }
}

/// Cap-bounded pipe reader. Overflow kills the child (so the writer stops)
/// and reports it; the caller treats the run as failed.
async fn read_capped<R>(reader: Option<R>, limit: usize, pid: Option<u32>) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut buffer = [0u8; 8192];
    let mut collected: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if collected.len().saturating_add(read) > limit {
                    collected.extend_from_slice(&buffer[..limit.saturating_sub(collected.len())]);
                    kill_process(pid);
                    return (String::from_utf8_lossy(&collected).into_owned(), true);
                }
                collected.extend_from_slice(&buffer[..read]);
            }
        }
    }
    (String::from_utf8_lossy(&collected).into_owned(), false)
}

fn kill_process(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: pid belongs to a child process this function spawned and
        // still owns; SIGKILL is always safe to send.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pid;
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
