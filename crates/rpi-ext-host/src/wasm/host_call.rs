//! `rpi_host_call` method dispatch (T15 W6) — the JSON method table of ABI
//! v1 (docs/extension-abi.md). Every method maps onto the same surface the
//! native (L0) API uses: registration → `ExtensionApi`, actions →
//! `HostActions`, context → `ContextActions`, commands →
//! `CommandContextActions`, UI → `UiBridge`, bus → `EventBus`.
//!
//! Returns `Ok(value)` for the `{"ok": value}` envelope, or
//! `Err((kind, message))` — kinds: `capabilityDenied` (checked by the
//! caller before dispatch), `unbound`, `stale`, `invalidRequest`,
//! `unknownMethod`, `call`.

use std::sync::Arc;

use serde_json::{json, Value};

use super::{Capability, HostState};
use crate::api::{DeliverAs, SendMessageOptions, SendUserMessageOptions};
use crate::types as ext;
use crate::types::{FlagType, FlagValue};

/// Capability gate outcome for a host method (docs/extension-abi.md table).
pub enum CapabilityRequirement {
    /// No capability required: `capabilities: []` guests may subscribe to
    /// events (`on`), and a flag read (`getFlag`) only ever sees the
    /// extension's own flags.
    Free,
    /// The method requires the given manifest capability.
    Requires(Capability),
    /// Not a host method at all. The caller rejects it as `unknownMethod`
    /// without any capability check. This arm exists so the mapping is
    /// fail-closed for maintenance: a new dispatch arm whose author forgets
    /// to classify it here is unreachable (guests get `unknownMethod`) until
    /// it is assigned a capability — instead of silently inheriting one.
    UnknownMethod,
}

/// method → capability mapping (docs/extension-abi.md table). Every known
/// top-level method is listed explicitly; the `ui.`/`command.` prefixes are
/// safe to map wholesale because their sub-dispatchers are themselves
/// fail-closed (`unknownMethod` for unlisted sub-methods).
pub fn required_capability(method: &str) -> CapabilityRequirement {
    use CapabilityRequirement::{Free, Requires, UnknownMethod};
    match method {
        "on" | "getFlag" => Free,
        // ADR-0015 additions: unregisterTool (registry removal) and
        // toolUpdate (partial-result report) belong to the same capability
        // as registerTool — they write/affect only this extension's tools.
        "registerTool" | "unregisterTool" | "toolUpdate" => Requires(Capability::Tools),
        "registerCommand" | "registerShortcut" | "registerFlag" => Requires(Capability::Commands),
        "registerMessageRenderer" | "registerEntryRenderer" | "registerMarkdownTransformer" => {
            Requires(Capability::Ui)
        }
        "exec" => Requires(Capability::Exec),
        "registerProvider" | "unregisterProvider" => Requires(Capability::Provider),
        "events.emit" | "events.on" => Requires(Capability::Events),
        m if m.starts_with("ui.") => Requires(Capability::Ui),
        m if m.starts_with("command.") => Requires(Capability::Session),
        // Session-level actions and context reads, including the v0.11
        // additions (ctx.scopedModels / ctx.modelRegistry.* /
        // ctx.setRuntimeApiKey / ctx.removeRuntimeApiKey /
        // ctx.getSystemPromptSource / ctx.getAppendSystemPromptSources).
        "sendMessage"
        | "sendUserMessage"
        | "appendEntry"
        | "setSessionName"
        | "getSessionName"
        | "setLabel"
        | "getActiveTools"
        | "getAllTools"
        | "setActiveTools"
        | "getCommands"
        | "setModel"
        | "getThinkingLevel"
        | "setThinkingLevel"
        | "ctx.isIdle"
        | "ctx.isProjectTrusted"
        | "ctx.hasPendingMessages"
        | "ctx.getContextUsage"
        | "ctx.getSystemPrompt"
        | "ctx.model"
        | "ctx.cwd"
        | "ctx.mode"
        | "ctx.hasUI"
        | "ctx.abort"
        | "ctx.shutdown"
        | "ctx.compact"
        | "ctx.scopedModels"
        | "ctx.getSystemPromptSource"
        | "ctx.getAppendSystemPromptSources"
        | "ctx.modelRegistry.complete"
        | "ctx.modelRegistry.find"
        | "ctx.modelRegistry.hasConfiguredAuth"
        | "ctx.modelRegistry.getApiKeyAndHeaders"
        | "ctx.setRuntimeApiKey"
        | "ctx.removeRuntimeApiKey" => Requires(Capability::Session),
        _ => UnknownMethod,
    }
}

type CallResult = Result<Value, (&'static str, String)>;

fn err<T>(kind: &'static str, message: impl Into<String>) -> Result<T, (&'static str, String)> {
    Err((kind, message.into()))
}

pub(super) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// Async host work from the guest thread: spawn onto the ambient runtime,
/// block the (dedicated) guest thread on a std channel.
pub(super) fn block_on<R: Send + 'static>(
    handle: &tokio::runtime::Handle,
    future: impl std::future::Future<Output = R> + Send + 'static,
) -> Result<R, (&'static str, String)> {
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv()
        .map_err(|_| ("internal", "host runtime dropped the response".to_owned()))
}

pub(crate) fn dispatch(state: &mut HostState, method: &str, args: Value) -> CallResult {
    match method {
        // ------------------------------------------------------------------
        // Registration (ExtensionApi)
        // ------------------------------------------------------------------
        "on" => {
            let event = str_arg(&args, "event")
                .ok_or_else(|| ("invalidRequest", "on: missing event".to_owned()))?
                .to_owned();
            let forward = state.forward.clone();
            let dispatch_event = event.clone();
            state
                .api
                .on(
                    &event,
                    Arc::new(move |payload, _ctx| {
                        let forward = forward.clone();
                        let dispatch_event = dispatch_event.clone();
                        Box::pin(async move {
                            forward
                                .dispatch(
                                    json!({
                                        "kind": "event",
                                        "event": dispatch_event,
                                        "payload": payload,
                                    }),
                                    false,
                                )
                                .await
                        })
                    }),
                )
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        "registerTool" => {
            let definition = args.get("definition").cloned().unwrap_or(args.clone());
            let name = str_arg(&definition, "name")
                .ok_or_else(|| ("invalidRequest", "registerTool: missing name".to_owned()))?
                .to_owned();
            let forward_exec = state.forward.clone();
            let tool_name = name.clone();
            let execute_updates = state.tool_updates.clone();
            let render_call = state.forward.clone();
            let render_result = state.forward.clone();
            let has_render_call = definition
                .get("renderCall")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let has_render_result = definition
                .get("renderResult")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            state
                .api
                .register_tool(ext::ToolDefinition {
                    label: str_arg(&definition, "label").unwrap_or(&name).to_owned(),
                    description: str_arg(&definition, "description")
                        .unwrap_or_default()
                        .to_owned(),
                    prompt_snippet: str_arg(&definition, "promptSnippet").map(str::to_owned),
                    prompt_guidelines: definition
                        .get("promptGuidelines")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    parameters: definition
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                    constrained_sampling: definition.get("constrainedSampling").cloned(),
                    render_shell: str_arg(&definition, "renderShell").map(str::to_owned),
                    prepare_arguments: None,
                    execution_mode: definition.get("executionMode").and_then(Value::as_str).map(
                        |mode| match mode {
                            "sequential" => rpi_agent::types::ToolExecutionMode::Sequential,
                            _ => rpi_agent::types::ToolExecutionMode::Parallel,
                        },
                    ),
                    execute: Arc::new(move |request, _ctx| {
                        let forward = forward_exec.clone();
                        let tool_name = tool_name.clone();
                        let tool_updates = execute_updates.clone();
                        Box::pin(async move {
                            let tool_call_id = request.tool_call_id.clone();
                            // ADR-0015: stash the agent's on_update sink so
                            // `toolUpdate` host calls made by the guest/plugin
                            // during this dispatch stream partial results back.
                            // The entry is removed on return — late updates are
                            // dropped (upstream settle semantics).
                            if let Some(on_update) = request.on_update {
                                let sink: Arc<
                                    dyn Fn(rpi_agent::types::AgentToolResult) + Send + Sync,
                                > = on_update.into();
                                tool_updates
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(tool_call_id.clone(), sink);
                            }
                            let result = forward
                                .dispatch(
                                    json!({
                                        "kind": "toolExecute",
                                        "toolName": tool_name,
                                        "toolCallId": tool_call_id,
                                        "params": request.params,
                                    }),
                                    false,
                                )
                                .await;
                            tool_updates
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tool_call_id);
                            let result = result?;
                            serde_json::from_value(result)
                                .map_err(|e| format!("tool result JSON: {e}"))
                        })
                    }),
                    render_call: if has_render_call {
                        let render_tool_name = name.clone();
                        Some(Arc::new(move |context| {
                            let value =
                                serde_json::to_value(&context).map_err(|e| e.to_string())?;
                            let forward = render_call.clone();
                            let result = forward.dispatch_blocking(
                                json!({
                                    "kind": "render",
                                    "what": "toolCall",
                                    "toolName": render_tool_name,
                                    "context": value
                                }),
                                false,
                            )?;
                            if result.is_null() {
                                return Err("guest returned no component".to_owned());
                            }
                            Ok(result)
                        }))
                    } else {
                        None
                    },
                    render_result: if has_render_result {
                        let render_tool_name = name.clone();
                        Some(Arc::new(move |result, options, context| {
                            let forward = render_result.clone();
                            let payload = json!({
                                "kind": "render",
                                "what": "toolResult",
                                "toolName": render_tool_name,
                                "result": result,
                                "options": options,
                                "context": context,
                            });
                            let outcome = forward.dispatch_blocking(payload, false)?;
                            if outcome.is_null() {
                                return Err("guest returned no component".to_owned());
                            }
                            Ok(outcome)
                        }))
                    } else {
                        None
                    },
                    name,
                })
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        // ADR-0015: `unregisterTool(name)` — removes this extension's own
        // registry entry (returns bool); post-bind the session refreshes and
        // the tool leaves the active set. Unknown names → false, no error.
        "unregisterTool" => {
            let name = str_arg(&args, "name")
                .ok_or_else(|| ("invalidRequest", "unregisterTool: missing name".to_owned()))?;
            let removed = state
                .api
                .unregister_tool(name)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(json!(removed))
        }

        // ADR-0015: `toolUpdate(toolCallId, update)` — guest/plugin reports a
        // partial `AgentToolResult` for its in-flight toolExecute. The sink
        // lives in the per-extension pending table for the dispatch duration;
        // unknown/stale ids are dropped (upstream ignores post-execute
        // updates), still answering ok so the guest need not track lifetimes.
        "toolUpdate" => {
            let tool_call_id = str_arg(&args, "toolCallId").ok_or_else(|| {
                (
                    "invalidRequest",
                    "toolUpdate: missing toolCallId".to_owned(),
                )
            })?;
            let update = args
                .get("update")
                .cloned()
                .ok_or_else(|| ("invalidRequest", "toolUpdate: missing update".to_owned()))?;
            let update: rpi_agent::types::AgentToolResult = serde_json::from_value(update)
                .map_err(|e| {
                    (
                        "invalidRequest",
                        format!("toolUpdate: malformed update: {e}"),
                    )
                })?;
            let sink = state
                .tool_updates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(tool_call_id)
                .cloned();
            match sink {
                // Invoke outside the lock (coding-standards §6.5).
                Some(sink) => sink(update),
                None => {
                    tracing::debug!(
                        tool_call_id,
                        "toolUpdate dropped: no in-flight execution for this id"
                    );
                }
            }
            Ok(Value::Null)
        }

        "registerCommand" => {
            let name = str_arg(&args, "name")
                .ok_or_else(|| ("invalidRequest", "registerCommand: missing name".to_owned()))?
                .to_owned();
            let forward = state.forward.clone();
            let command_name = name.clone();
            state
                .api
                .register_command(
                    &name,
                    str_arg(&args, "description").map(str::to_owned),
                    Arc::new(move |args_text, _ctx| {
                        let forward = forward.clone();
                        let command_name = command_name.clone();
                        Box::pin(async move {
                            forward
                                .dispatch(
                                    json!({
                                        "kind": "command",
                                        "name": command_name,
                                        "args": args_text,
                                    }),
                                    true,
                                )
                                .await
                                .map(|_| ())
                        })
                    }),
                )
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        "registerShortcut" => {
            let shortcut = str_arg(&args, "shortcut")
                .ok_or_else(|| {
                    (
                        "invalidRequest",
                        "registerShortcut: missing shortcut".to_owned(),
                    )
                })?
                .to_owned();
            let forward = state.forward.clone();
            let shortcut_key = shortcut.clone();
            state
                .api
                .register_shortcut(
                    &shortcut,
                    str_arg(&args, "description").map(str::to_owned),
                    Arc::new(move |_ctx| {
                        let forward = forward.clone();
                        let shortcut = shortcut_key.clone();
                        Box::pin(async move {
                            forward
                                .dispatch(json!({"kind": "shortcut", "shortcut": shortcut}), false)
                                .await
                                .map(|_| ())
                        })
                    }),
                )
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        "registerFlag" => {
            let name = str_arg(&args, "name")
                .ok_or_else(|| ("invalidRequest", "registerFlag: missing name".to_owned()))?
                .to_owned();
            let flag_type = match str_arg(&args, "type") {
                Some("string") => FlagType::String,
                _ => FlagType::Boolean,
            };
            let default = args.get("default").and_then(|v| match v {
                Value::Bool(b) => Some(FlagValue::Boolean(*b)),
                Value::String(s) => Some(FlagValue::String(s.clone())),
                _ => None,
            });
            state
                .api
                .register_flag(
                    &name,
                    str_arg(&args, "description").map(str::to_owned),
                    flag_type,
                    default,
                )
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        "getFlag" => {
            let name = str_arg(&args, "name")
                .ok_or_else(|| ("invalidRequest", "getFlag: missing name".to_owned()))?;
            let value = state
                .api
                .get_flag(name)
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(match value {
                Some(FlagValue::Boolean(b)) => json!(b),
                Some(FlagValue::String(s)) => json!(s),
                None => Value::Null,
            })
        }

        "registerMessageRenderer" | "registerEntryRenderer" => {
            let custom_type = str_arg(&args, "customType")
                .ok_or_else(|| ("invalidRequest", "missing customType".to_owned()))?
                .to_owned();
            let forward = state.forward.clone();
            let is_message = method == "registerMessageRenderer";
            let result = if is_message {
                state.api.register_message_renderer(
                    &custom_type,
                    Arc::new(move |message, options| {
                        let forward = forward.clone();
                        forward
                            .dispatch_blocking(
                                json!({
                                    "kind": "render",
                                    "what": "message",
                                    "message": message,
                                    "options": options,
                                }),
                                false,
                            )
                            .map(Some)
                    }),
                )
            } else {
                state.api.register_entry_renderer(
                    &custom_type,
                    Arc::new(move |entry, options| {
                        let forward = forward.clone();
                        forward
                            .dispatch_blocking(
                                json!({
                                    "kind": "render",
                                    "what": "entry",
                                    "entry": entry,
                                    "options": options,
                                }),
                                false,
                            )
                            .map(Some)
                    }),
                )
            };
            result.map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        // v0.11: registerMarkdownTransformer (types.ts:1292 @ 4181f66).
        // Chained Markdown source transformer — TUI rendering wiring is T29;
        // the host stores one transformer per extension. The guest dispatch
        // for `render` kind "markdownTransform" is handled by the forward
        // path (same as message/entry renderers).
        "registerMarkdownTransformer" => {
            let forward = state.forward.clone();
            state
                .api
                .register_markdown_transformer(Arc::new(move |markdown, context| {
                    let forward = forward.clone();
                    // On error, return the input unchanged (upstream
                    // applyMarkdownTransformers catches and continues).
                    match forward.dispatch_blocking(
                        json!({
                            "kind": "render",
                            "what": "markdownTransform",
                            "markdown": markdown,
                            "context": context,
                        }),
                        false,
                    ) {
                        // markdown-transform.ts:19-23: non-string Ok return
                        // preserves the current markdown (do not assign).
                        Ok(result) => result
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| markdown.to_owned()),
                        Err(_) => markdown,
                    }
                }))
                .map_err(|e| ("stale", e.to_string()))?;
            Ok(Value::Null)
        }

        // ------------------------------------------------------------------
        // Actions (HostActions)
        // ------------------------------------------------------------------
        "sendMessage" => {
            let message = args.get("message").cloned().unwrap_or(Value::Null);
            let options = args.get("options").cloned().map(parse_send_message_options);
            state
                .api
                .send_message(message, options)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "sendUserMessage" => {
            let content = args.get("content").cloned().unwrap_or(Value::Null);
            let options = args
                .get("options")
                .cloned()
                .map(|options| SendUserMessageOptions {
                    deliver_as: options.get("deliverAs").and_then(parse_deliver_as),
                });
            state
                .api
                .send_user_message(content, options)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "appendEntry" => {
            let custom_type = str_arg(&args, "customType").unwrap_or_default();
            state
                .api
                .append_entry(custom_type, args.get("data").cloned())
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "setSessionName" => {
            state
                .api
                .set_session_name(str_arg(&args, "name").unwrap_or_default())
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "getSessionName" => Ok(state
            .api
            .get_session_name()
            .map_err(|e| (error_kind(&e), e.to_string()))?
            .map(Value::from)
            .unwrap_or(Value::Null)),
        "setLabel" => {
            state
                .api
                .set_label(
                    str_arg(&args, "entryId").unwrap_or_default(),
                    str_arg(&args, "label"),
                )
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "exec" => {
            let command = str_arg(&args, "command").unwrap_or_default().to_owned();
            let exec_args: Vec<String> = args
                .get("args")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let options = args
                .get("options")
                .cloned()
                .and_then(|v| serde_json::from_value::<crate::api::ExecOptions>(v).ok());
            let api = state.api.clone();
            let handle = state.async_handle.clone();
            block_on(&handle, async move {
                api.exec(&command, &exec_args, options).await
            })
            .and_then(|result| {
                result
                    .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                    .map_err(|e| (error_kind(&e), e.to_string()))
            })
        }
        "getActiveTools" => Ok(json!(state
            .api
            .get_active_tools()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "getAllTools" => Ok(json!(state
            .api
            .get_all_tools()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "setActiveTools" => {
            let names: Vec<String> = args
                .get("toolNames")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            state
                .api
                .set_active_tools(names)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "getCommands" => Ok(json!(state
            .api
            .get_commands()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "setModel" => {
            let api = state.api.clone();
            let handle = state.async_handle.clone();
            let model = args.get("model").cloned().unwrap_or(Value::Null);
            let result = block_on(&handle, async move { api.set_model(model).await })?
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(json!(result))
        }
        "getThinkingLevel" => Ok(json!(state
            .api
            .get_thinking_level()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "setThinkingLevel" => {
            state
                .api
                .set_thinking_level(str_arg(&args, "level").unwrap_or("off"))
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "registerProvider" => {
            let name = str_arg(&args, "name").unwrap_or_default().to_owned();
            let config = args.get("config").cloned().unwrap_or(Value::Null);
            let api = state.api.clone();
            let handle = state.async_handle.clone();
            block_on(&handle, async move {
                api.register_provider(&name, config).await
            })?
            .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "unregisterProvider" => {
            let name = str_arg(&args, "name").unwrap_or_default().to_owned();
            let api = state.api.clone();
            let handle = state.async_handle.clone();
            block_on(&handle, async move { api.unregister_provider(&name).await })?
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }

        // ------------------------------------------------------------------
        // Event bus
        // ------------------------------------------------------------------
        "events.emit" => {
            let channel = str_arg(&args, "channel").unwrap_or_default();
            state
                .api
                .events()
                .emit(channel, args.get("data").cloned().unwrap_or(Value::Null))
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "events.on" => {
            let channel = str_arg(&args, "channel")
                .ok_or_else(|| ("invalidRequest", "events.on: missing channel".to_owned()))?
                .to_owned();
            let forward = state.forward.clone();
            let bus_channel = channel.clone();
            let _unsubscribe = state
                .api
                .events()
                .on(
                    &channel,
                    Arc::new(move |data| {
                        forward.dispatch_forget(json!({
                            "kind": "bus",
                            "channel": bus_channel,
                            "data": data,
                        }));
                    }),
                )
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            // 6ca423447: subscriptions are tracked by the runtime and
            // auto-unsubscribed on `invalidate()`. The handle is dropped —
            // calling it would unsubscribe immediately, and keeping it alive
            // in JS land is unnecessary since the runtime tracks it.
            std::mem::forget(_unsubscribe);
            Ok(Value::Null)
        }

        // ------------------------------------------------------------------
        // Context (ContextActions via ExtensionContext)
        // ------------------------------------------------------------------
        "ctx.isIdle" => Ok(json!(state
            .api
            .context()
            .is_idle()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.isProjectTrusted" => Ok(json!(state
            .api
            .context()
            .is_project_trusted()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.hasPendingMessages" => Ok(json!(state
            .api
            .context()
            .has_pending_messages()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.getContextUsage" => {
            let usage = state
                .api
                .context()
                .get_context_usage()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(serde_json::to_value(usage).unwrap_or(Value::Null))
        }
        "ctx.getSystemPrompt" => Ok(json!(state
            .api
            .context()
            .get_system_prompt()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.model" => Ok(state
            .api
            .context()
            .model()
            .map_err(|e| (error_kind(&e), e.to_string()))?
            .unwrap_or(Value::Null)),
        "ctx.cwd" => Ok(json!(state
            .api
            .context()
            .cwd()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.mode" => {
            let mode = state
                .api
                .context()
                .mode()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(serde_json::to_value(mode).unwrap_or(Value::Null))
        }
        "ctx.hasUI" => Ok(json!(state
            .api
            .context()
            .has_ui()
            .map_err(|e| (error_kind(&e), e.to_string()))?)),
        "ctx.abort" => {
            state
                .api
                .context()
                .abort()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "ctx.shutdown" => {
            state
                .api
                .context()
                .shutdown()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "ctx.compact" => {
            state
                .api
                .context()
                .compact(crate::api::CompactOptions {
                    custom_instructions: str_arg(&args, "customInstructions").map(str::to_owned),
                    on_complete: None,
                    on_error: None,
                })
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }

        // --------------------------------------------------------------
        // v0.11 context additions (types.ts @ 4181f66)
        // --------------------------------------------------------------
        "ctx.scopedModels" => {
            let scoped = state
                .api
                .context()
                .scoped_models()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(serde_json::to_value(&scoped).unwrap_or(Value::Null))
        }
        "ctx.getSystemPromptSource" => {
            let path = state
                .api
                .context()
                .get_system_prompt_source()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(match path {
                Some(p) => json!(p),
                None => Value::Null,
            })
        }
        "ctx.getAppendSystemPromptSources" => {
            let paths = state
                .api
                .context()
                .get_append_system_prompt_sources()
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(serde_json::to_value(&paths).unwrap_or_else(|_| json!([])))
        }
        "ctx.modelRegistry.complete" => {
            let model = args
                .get("model")
                .cloned()
                .ok_or_else(|| ("invalidRequest", "missing model".to_owned()))?;
            let context = args
                .get("context")
                .cloned()
                .ok_or_else(|| ("invalidRequest", "missing context".to_owned()))?;
            let options = args.get("options").cloned();
            let api = state.api.clone();
            let result = block_on(&state.async_handle.clone(), async move {
                api.model_registry_complete(model, context, options).await
            })?;
            let result = result.map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(result.unwrap_or(Value::Null))
        }
        "ctx.modelRegistry.find" => {
            let provider = str_arg(&args, "provider").unwrap_or_default();
            let model_id = str_arg(&args, "modelId").unwrap_or_default();
            let result = state
                .api
                .model_registry_find(provider, model_id)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(result.unwrap_or(Value::Null))
        }
        "ctx.modelRegistry.hasConfiguredAuth" => {
            let provider_id = str_arg(&args, "providerId").unwrap_or_default();
            let result = state
                .api
                .model_registry_has_configured_auth(provider_id)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(json!(result))
        }
        "ctx.modelRegistry.getApiKeyAndHeaders" => {
            // #7030: null header deletion markers MUST pass through unchanged.
            let model = args
                .get("model")
                .cloned()
                .ok_or_else(|| ("invalidRequest", "missing model".to_owned()))?;
            let api = state.api.clone();
            let result = block_on(&state.async_handle.clone(), async move {
                api.get_api_key_and_headers(model).await
            })?;
            let result = result.map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(result)
        }
        "ctx.setRuntimeApiKey" => {
            let provider_id = str_arg(&args, "providerId")
                .ok_or_else(|| ("invalidRequest", "missing providerId".to_owned()))?
                .to_owned();
            let api_key = str_arg(&args, "apiKey")
                .ok_or_else(|| ("invalidRequest", "missing apiKey".to_owned()))?
                .to_owned();
            let api = state.api.clone();
            block_on(&state.async_handle.clone(), async move {
                api.set_runtime_api_key(&provider_id, &api_key, None).await
            })?
            .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }
        "ctx.removeRuntimeApiKey" => {
            let provider_id = str_arg(&args, "providerId")
                .ok_or_else(|| ("invalidRequest", "missing providerId".to_owned()))?
                .to_owned();
            let api = state.api.clone();
            block_on(&state.async_handle.clone(), async move {
                api.remove_runtime_api_key(&provider_id).await
            })?
            .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(Value::Null)
        }

        // ------------------------------------------------------------------
        // Command context (CommandContextActions) — command dispatches only
        // ------------------------------------------------------------------
        m if m.starts_with("command.") => {
            if !state.in_command.get() {
                return err(
                    "invalidRequest",
                    format!("{m} is only available inside a command handler"),
                );
            }
            let command_actions = state.api.runtime().command_actions();
            let Some(actions) = command_actions else {
                // Unbound: upstream defaults (runner.ts:421-427).
                return Ok(json!({"cancelled": false}));
            };
            let handle = state.async_handle.clone();
            match m {
                "command.waitForIdle" => {
                    block_on(&handle, async move { actions.wait_for_idle().await })?;
                    Ok(Value::Null)
                }
                "command.newSession" => {
                    let parent = str_arg(&args, "parentSession").map(str::to_owned);
                    let cancelled =
                        block_on(
                            &handle,
                            async move { actions.new_session(parent, None).await },
                        )?;
                    Ok(json!({"cancelled": cancelled}))
                }
                "command.fork" => {
                    let entry_id = str_arg(&args, "entryId").unwrap_or_default().to_owned();
                    let position = str_arg(&args, "position").map(str::to_owned);
                    let cancelled = block_on(&handle, async move {
                        actions.fork(&entry_id, position, None).await
                    })?;
                    Ok(json!({"cancelled": cancelled}))
                }
                "command.navigateTree" => {
                    let target_id = str_arg(&args, "targetId").unwrap_or_default().to_owned();
                    let options: crate::api::NavigateTreeOptions = args
                        .get("options")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();
                    let cancelled = block_on(&handle, async move {
                        actions.navigate_tree(&target_id, options).await
                    })?;
                    Ok(json!({"cancelled": cancelled}))
                }
                "command.switchSession" => {
                    let path = str_arg(&args, "sessionPath").unwrap_or_default().to_owned();
                    let cancelled =
                        block_on(
                            &handle,
                            async move { actions.switch_session(&path, None).await },
                        )?;
                    Ok(json!({"cancelled": cancelled}))
                }
                "command.reload" => {
                    block_on(&handle, async move { actions.reload().await })?;
                    Ok(Value::Null)
                }
                _ => err("unknownMethod", format!("unknown host call: {m}")),
            }
        }

        // ------------------------------------------------------------------
        // UI bridge (28 methods, "ui.<name>")
        // ------------------------------------------------------------------
        m if m.starts_with("ui.") => super::ui_dispatch::dispatch(state, m, args),

        _ => err("unknownMethod", format!("unknown host call: {method}")),
    }
}

fn error_kind(error: &crate::error::ExtError) -> &'static str {
    match error {
        crate::error::ExtError::Stale(_) => "stale",
        crate::error::ExtError::Unbound(_) => "unbound",
        crate::error::ExtError::CapabilityDenied(_) => "capabilityDenied",
        _ => "call",
    }
}

fn parse_deliver_as(value: &Value) -> Option<DeliverAs> {
    match value.as_str()? {
        "steer" => Some(DeliverAs::Steer),
        "followUp" => Some(DeliverAs::FollowUp),
        "nextTurn" => Some(DeliverAs::NextTurn),
        _ => None,
    }
}

fn parse_send_message_options(options: Value) -> SendMessageOptions {
    SendMessageOptions {
        trigger_turn: bool_arg(&options, "triggerTurn"),
        deliver_as: options.get("deliverAs").and_then(parse_deliver_as),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability gate is fail-closed: methods not listed in
    /// `required_capability` are classified `UnknownMethod` and rejected
    /// before dispatch, instead of silently inheriting a capability.
    #[test]
    fn unknown_methods_are_unclassified() {
        assert!(matches!(
            required_capability("ctx.totallyMadeUp"),
            CapabilityRequirement::UnknownMethod
        ));
        assert!(matches!(
            required_capability("registerSomethingNew"),
            CapabilityRequirement::UnknownMethod
        ));
        assert!(matches!(
            required_capability(""),
            CapabilityRequirement::UnknownMethod
        ));
    }

    #[test]
    fn known_methods_map_to_their_documented_capabilities() {
        use CapabilityRequirement::{Free, Requires};
        assert!(matches!(required_capability("on"), Free));
        assert!(matches!(required_capability("getFlag"), Free));
        assert!(matches!(
            required_capability("registerTool"),
            Requires(Capability::Tools)
        ));
        assert!(matches!(
            required_capability("registerCommand"),
            Requires(Capability::Commands)
        ));
        assert!(matches!(
            required_capability("registerMarkdownTransformer"),
            Requires(Capability::Ui)
        ));
        assert!(matches!(
            required_capability("exec"),
            Requires(Capability::Exec)
        ));
        assert!(matches!(
            required_capability("registerProvider"),
            Requires(Capability::Provider)
        ));
        assert!(matches!(
            required_capability("events.emit"),
            Requires(Capability::Events)
        ));
        // Prefix arms: sub-dispatchers reject unlisted sub-methods.
        assert!(matches!(
            required_capability("ui.select"),
            Requires(Capability::Ui)
        ));
        assert!(matches!(
            required_capability("command.newSession"),
            Requires(Capability::Session)
        ));
        // v0.11 additions are Session-gated.
        assert!(matches!(
            required_capability("ctx.modelRegistry.getApiKeyAndHeaders"),
            Requires(Capability::Session)
        ));
        assert!(matches!(
            required_capability("ctx.scopedModels"),
            Requires(Capability::Session)
        ));
        assert!(matches!(
            required_capability("ctx.setRuntimeApiKey"),
            Requires(Capability::Session)
        ));
    }
}
