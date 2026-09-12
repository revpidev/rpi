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
        // v0.1.4 C0 (ADR-0024): the seven interactive custom UI host-calls
        // are frozen in the method table but not implemented yet. They are
        // listed explicitly (before the `ui.` prefix arm) so the capability
        // classification is independent of the prefix rule; the dispatch arm
        // in `ui_dispatch` answers `unknownMethod` until C1/C2 land.
        "ui.mountComponent"
        | "ui.pollComponent"
        | "ui.renderComponent"
        | "ui.setComponentHidden"
        | "ui.wakeComponent"
        | "ui.disposeComponent"
        | "ui.editExternal" => Requires(Capability::Ui),
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
        | "ctx.aborted"
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
        | "ctx.removeRuntimeApiKey"
        | "ctx.sessionFile"
        | "ctx.sessionEntries" => Requires(Capability::Session),
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
            let execute_aborts = state.tool_aborts.clone();
            // The component owner identity the wire calls stamp
            // (`NamespacedUiBridge`), precomputed for the tool-abort watcher
            // below (V14-23 C3).
            let owner_namespace = crate::api::extension_namespace(&state.api.extension().path);
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
                    execute: Arc::new(move |request, ctx| {
                        let forward = forward_exec.clone();
                        let tool_name = tool_name.clone();
                        let tool_updates = execute_updates.clone();
                        let tool_aborts = execute_aborts.clone();
                        let watcher_owner = owner_namespace.clone();
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
                            // Abort-channel gap: the native dispatch below is
                            // a synchronous FFI call the runtime cannot cancel,
                            // so expose the execution's CancellationToken to
                            // the plugin (`ctx.aborted`) for the dispatch
                            // duration. A blocking tool polls it and returns
                            // promptly on a user abort.
                            tool_aborts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(tool_call_id.clone(), request.signal.clone());
                            // V14-23 C3 (R-U1.5 / design §3.6): when the
                            // turn is aborted, the extension's mounted
                            // interactive component (e.g. a dialog mounted by
                            // this very tool) receives `dispose{toolAbort}`
                            // and its grace window. Owner-scoped so an
                            // aborted extension never disposes another
                            // extension's component; a stale runtime (post
                            // reload) makes `ui()` fail — the extensionUnload
                            // path owns that case. The watcher dies with the
                            // dispatch; without an ambient runtime there is
                            // no abort to observe.
                            let abort_watcher = {
                                let signal = request.signal.clone();
                                let watcher_ctx = ctx.clone();
                                let owner = watcher_owner;
                                tokio::runtime::Handle::try_current().ok().map(|handle| {
                                    handle.spawn(async move {
                                        signal.cancelled().await;
                                        if let Ok(ui) = watcher_ctx.ui() {
                                            ui.begin_forced_dispose(
                                                Some(&owner),
                                                crate::interactive_ui::DisposeReason::ToolAbort,
                                            );
                                        }
                                    })
                                })
                            };
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
                            if let Some(watcher) = abort_watcher {
                                watcher.abort();
                            }
                            tool_aborts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&tool_call_id);
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

        // Abort-channel query (see PendingToolAborts): is the in-flight
        // toolExecute for this toolCallId cancelled? Unknown ids answer
        // false — a late poll after the dispatch returned is not an abort.
        "ctx.aborted" => {
            let tool_call_id = str_arg(&args, "toolCallId").ok_or_else(|| {
                (
                    "invalidRequest",
                    "ctx.aborted: missing toolCallId".to_owned(),
                )
            })?;
            let cancelled = state
                .tool_aborts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(tool_call_id)
                .map(|token| token.is_cancelled())
                .unwrap_or(false);
            Ok(json!(cancelled))
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
            let raw_default = args.get("default").filter(|v| !v.is_null());
            let default = raw_default.and_then(|v| match v {
                Value::Bool(b) => Some(FlagValue::Boolean(*b)),
                Value::String(s) => Some(FlagValue::String(s.clone())),
                _ => None,
            });
            // f47faf459 (V14-11 FR-E): reject a default whose JSON type does
            // not match the declared flag type — including kinds that have
            // no FlagValue variant (upstream compares `typeof`).
            if let Some(raw) = raw_default {
                let matches = matches!(
                    (&default, flag_type),
                    (Some(FlagValue::Boolean(_)), FlagType::Boolean)
                        | (Some(FlagValue::String(_)), FlagType::String)
                );
                if !matches {
                    let expected = match flag_type {
                        FlagType::Boolean => "boolean",
                        FlagType::String => "string",
                    };
                    let got = match raw {
                        Value::Bool(_) => "boolean",
                        Value::String(_) => "string",
                        Value::Number(_) => "number",
                        _ => "object",
                    };
                    return err(
                        "call",
                        format!(
                            "Invalid default for flag \"{name}\": expected {expected}, got {got}"
                        ),
                    );
                }
            }
            state
                .api
                .register_flag(
                    &name,
                    str_arg(&args, "description").map(str::to_owned),
                    flag_type,
                    default,
                )
                .map_err(|e| (error_kind(&e), e.to_string()))?;
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
                    expand_prompt_templates: options
                        .get("expandPromptTemplates")
                        .and_then(Value::as_bool),
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
        "ctx.sessionFile" => {
            // rpi additive (ADR-0022): `{path: string|null, id: string}`;
            // unbound hosts answer the all-null shape rather than an error
            // so guests can feature-detect.
            let info = state
                .api
                .context()
                .session_file()
                .map_err(|e| (error_kind(&e), e.to_string()))?
                .unwrap_or(ext::SessionFileInfo {
                    path: None,
                    id: String::new(),
                });
            Ok(serde_json::to_value(info).unwrap_or(Value::Null))
        }
        "ctx.sessionEntries" => {
            // rpi additive (ADR-0027): read-only `custom` entries of the
            // active branch. Both arguments are optional; invalid values
            // (non-string `customType`, non-positive/non-integer `limit`)
            // are treated as absent, and an explicit `limit` is capped at
            // SESSION_ENTRIES_MAX_LIMIT. Unbound hosts fail closed with [].
            let custom_type = str_arg(&args, "customType");
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .filter(|limit| *limit > 0)
                .map(|limit| limit.min(ext::SESSION_ENTRIES_MAX_LIMIT));
            let entries = state
                .api
                .context()
                .session_entries(custom_type, limit)
                .map_err(|e| (error_kind(&e), e.to_string()))?;
            Ok(serde_json::to_value(entries).unwrap_or(Value::Array(Vec::new())))
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
        // ADR-0022 addition is Session-gated (same family as ctx.*).
        assert!(matches!(
            required_capability("ctx.sessionFile"),
            Requires(Capability::Session)
        ));
        // ADR-0027 addition is Session-gated too (same family as ctx.*).
        assert!(matches!(
            required_capability("ctx.sessionEntries"),
            Requires(Capability::Session)
        ));
        // v0.1.4 C0 interactive UI additions are all `ui`-gated, additive.
        for method in crate::interactive_ui::INTERACTIVE_UI_METHODS {
            assert!(
                matches!(required_capability(method), Requires(Capability::Ui)),
                "{method}"
            );
        }
    }
}

/// V14-25 (ADR-0027): `ctx.sessionEntries` dispatch-level behavior — arg
/// parsing/validation/clamping at the ABI boundary, fail-closed `[]` for
/// unbound hosts, and the capability gate. Branch/filter/order semantics
/// against a real `SessionManager` live in
/// `rpi/tests/extension_ctx_session_entries_test.rs`.
#[cfg(test)]
mod session_entries_tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::dispatch;
    use crate::api::{
        CompactOptions, ContextActions, ContextUsage, ExtensionApi, ExtensionRuntime,
        LoadedExtension,
    };
    use crate::types::SessionEntryInfo;
    use crate::wasm::{Capability, DispatchTarget, HostState, WasmForward};

    /// ContextActions recorder: captures the (customType, limit) pair the
    /// dispatch layer hands to the trait boundary.
    struct RecordingActions {
        calls: std::sync::Mutex<Vec<(Option<String>, Option<u64>)>>,
    }

    #[async_trait]
    impl ContextActions for RecordingActions {
        fn get_model(&self) -> Option<Value> {
            None
        }
        fn is_idle(&self) -> bool {
            true
        }
        fn is_project_trusted(&self) -> bool {
            true
        }
        fn get_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
            None
        }
        fn abort(&self) {}
        fn has_pending_messages(&self) -> bool {
            false
        }
        fn shutdown(&self) {}
        fn get_context_usage(&self) -> Option<ContextUsage> {
            None
        }
        fn compact(&self, _options: CompactOptions) {}
        fn get_system_prompt(&self) -> String {
            String::new()
        }
        fn get_system_prompt_options(&self) -> Value {
            Value::Null
        }
        fn get_session_entries(
            &self,
            custom_type: Option<&str>,
            limit: Option<u64>,
        ) -> Vec<SessionEntryInfo> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((custom_type.map(str::to_owned), limit));
            vec![SessionEntryInfo {
                id: "stub".to_owned(),
                parent_id: None,
                timestamp: String::new(),
                custom_type: custom_type.unwrap_or_default().to_owned(),
                data: Value::Null,
            }]
        }
    }

    fn host_state(capabilities: HashSet<Capability>) -> HostState {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let api = ExtensionApi::for_extension(
            Arc::new(LoadedExtension::new("<inline:v14-25>", "<inline:v14-25>")),
            ExtensionRuntime::new(),
            "/test-cwd",
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        HostState {
            api,
            capabilities,
            async_handle: runtime.handle().clone(),
            forward: DispatchTarget::Wasm(WasmForward { tx }),
            in_command: std::cell::Cell::new(false),
            tool_updates: Default::default(),
            tool_aborts: Default::default(),
            memory_limiter: crate::wasm::MemoryLimiter,
        }
    }

    /// A5 (dispatch level): a host with no bound ContextActions fails
    /// closed with `[]`, not an error.
    #[test]
    fn ctx_session_entries_unbound_answers_empty_array() {
        let mut state = host_state(HashSet::from([Capability::Session]));
        let value =
            dispatch(&mut state, "ctx.sessionEntries", json!({})).expect("unbound host answers []");
        assert_eq!(value, json!([]));
    }

    /// Arg contract at the trait boundary: optional `customType`/`limit`,
    /// non-positive / non-integer / non-string values treated as absent,
    /// explicit limits clamped to SESSION_ENTRIES_MAX_LIMIT.
    #[test]
    fn ctx_session_entries_arg_parsing_and_clamp() {
        let actions = Arc::new(RecordingActions {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let mut state = host_state(HashSet::from([Capability::Session]));
        state
            .api
            .runtime()
            .set_context_actions(Some(actions.clone() as Arc<dyn ContextActions>));

        dispatch(
            &mut state,
            "ctx.sessionEntries",
            json!({"customType": "mcp-approval-v1", "limit": 100}),
        )
        .expect("filtered call");
        dispatch(&mut state, "ctx.sessionEntries", json!({})).expect("empty-object call is legal");
        dispatch(
            &mut state,
            "ctx.sessionEntries",
            json!({"limit": 0, "customType": 42}),
        )
        .expect("invalid values are treated as absent");
        dispatch(&mut state, "ctx.sessionEntries", json!({"limit": u64::MAX}))
            .expect("oversized limit is clamped, not rejected");

        let calls = calls(&actions);
        assert_eq!(
            calls,
            vec![
                (Some("mcp-approval-v1".to_owned()), Some(100)),
                (None, None),
                (None, None),
                (None, Some(crate::types::SESSION_ENTRIES_MAX_LIMIT)),
            ],
            "limit 0 / non-string customType are absent; limit clamps at the host cap"
        );
    }

    /// The reply serializes the trait result through the `{"ok": [...]}`
    /// envelope shape (camelCase fields, verbatim data).
    #[test]
    fn ctx_session_entries_serializes_the_entry_shape() {
        let actions = Arc::new(RecordingActions {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let mut state = host_state(HashSet::from([Capability::Session]));
        state
            .api
            .runtime()
            .set_context_actions(Some(actions.clone() as Arc<dyn ContextActions>));
        let value = dispatch(
            &mut state,
            "ctx.sessionEntries",
            json!({"customType": "mcp-approval-v1"}),
        )
        .expect("call");
        assert_eq!(
            value,
            json!([{
                "id": "stub",
                "parentId": null,
                "timestamp": "",
                "customType": "mcp-approval-v1",
                "data": null,
            }]),
            "camelCase wire shape with data: null when the entry carries none"
        );
    }

    /// A9: without capability `session` the guest-call gate (checked in
    /// `handle_host_call` before dispatch) rejects with `capabilityDenied`.
    #[test]
    fn ctx_session_entries_requires_session_capability() {
        let mut state = host_state(HashSet::from([Capability::Tools]));
        let response = crate::wasm::handle_host_call(
            &mut state,
            b"{\"call\": \"ctx.sessionEntries\", \"args\": {}}",
        );
        let response: Value = serde_json::from_slice(&response).expect("envelope JSON");
        assert_eq!(
            response["error"]["kind"],
            json!("capabilityDenied"),
            "full envelope: {response}"
        );
    }

    fn calls(actions: &RecordingActions) -> Vec<(Option<String>, Option<u64>)> {
        actions
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod c3_dispose_tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;

    use super::dispatch;
    use crate::api::{ExtensionApi, ExtensionRuntime, LoadedExtension};
    use crate::interactive_ui::DisposeReason;
    use crate::test_bridge::TestUiBridge;
    use crate::types::{ExtensionMode, ToolExecuteRequest};
    use crate::wasm::{Capability, DispatchTarget, GuestCommand, HostState, WasmForward};

    fn wasm_host_state(
        capabilities: HashSet<Capability>,
    ) -> (
        HostState,
        tokio::runtime::Runtime,
        std::sync::mpsc::Receiver<GuestCommand>,
        Arc<TestUiBridge>,
    ) {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let api = ExtensionApi::for_extension(
            Arc::new(LoadedExtension::new("<inline:c0>", "<inline:c0>")),
            ExtensionRuntime::new(),
            "/test-cwd",
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = Arc::new(TestUiBridge::new());
        api.runtime()
            .set_ui_bridge(Some(bridge.clone()), ExtensionMode::Tui);
        let state = HostState {
            api,
            capabilities,
            async_handle: runtime.handle().clone(),
            forward: DispatchTarget::Wasm(WasmForward { tx }),
            in_command: std::cell::Cell::new(false),
            tool_updates: Default::default(),
            tool_aborts: Default::default(),
            memory_limiter: crate::wasm::MemoryLimiter,
        };
        (state, runtime, rx, bridge)
    }

    /// V14-23 C3 (R-U1.5 / design §3.6): cancelling the tool execution's
    /// abort token delivers `dispose{toolAbort}` to the extension's mounted
    /// component through the bridge — owner-stamped with the extension
    /// namespace, before the tool dispatch returns. A completed tool (no
    /// cancellation) never fires the dispose.
    #[test]
    fn component_dispose_tool_abort_watcher_fires_on_cancel() {
        let (mut state, runtime, rx, bridge) = wasm_host_state(HashSet::from([Capability::Tools]));

        dispatch(
            &mut state,
            "registerTool",
            json!({"definition": {
                "name": "dialog_tool",
                "label": "Dialog Tool",
                "description": "mounts a component",
                "parameters": {"type": "object"},
            }}),
        )
        .expect("registerTool dispatch");
        let tools = state.api.extension().tools();
        let execute = tools
            .get("dialog_tool")
            .expect("registered tool")
            .definition
            .execute
            .clone();

        // Cancellation case: the "guest" cancels the run signal while the
        // toolExecute dispatch is in flight, then answers.
        {
            let signal = CancellationToken::new();
            let cancel_signal = signal.clone();
            let bridge_probe = bridge.clone();
            let guest = runtime.spawn(async move {
                while let Ok(command) = rx.recv() {
                    let GuestCommand::Dispatch {
                        message, respond, ..
                    } = command
                    else {
                        continue;
                    };
                    let message: Value = serde_json::from_slice(&message).expect("guest message");
                    if message["kind"] == "toolExecute" {
                        // The user aborts the turn mid-tool.
                        cancel_signal.cancel();
                        // Let the watcher observe the cancellation BEFORE the
                        // tool returns (deterministic, no scheduler race).
                        for _ in 0..200 {
                            if !bridge_probe.aborts().is_empty() {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                        respond
                            .send(Ok(json!({
                                "content": [{"type": "text", "text": "aborted-but-returned"}],
                                "details": null,
                            })))
                            .expect("respond");
                        // Exit after one round-trip: a worker blocked in
                        // `rx.recv()` cannot park and would deadlock runtime
                        // shutdown while the sender still lives in HostState.
                        break;
                    }
                }
            });
            let request = ToolExecuteRequest {
                tool_call_id: "call_1".to_owned(),
                params: json!({}),
                signal,
                on_update: None,
            };
            let ctx = state.api.context();
            let result = runtime
                .block_on(async { execute(request, ctx).await })
                .expect("tool result after abort");
            let text = match result.content.first() {
                Some(rpi_ai::types::ToolResultContent::Text(text)) => text.text.clone(),
                other => panic!("expected text content: {other:?}"),
            };
            assert_eq!(text, "aborted-but-returned");
            guest.abort();

            let aborts = bridge.aborts();
            assert_eq!(
                aborts,
                vec![("inline".to_owned(), DisposeReason::ToolAbort)],
                "owner = the registering extension's namespace, reason = toolAbort"
            );
        }

        // No-cancellation case: a normally completing tool never disposes.
        {
            let (mut state2, runtime2, rx2, bridge2) =
                wasm_host_state(HashSet::from([Capability::Tools]));
            dispatch(
                &mut state2,
                "registerTool",
                json!({"definition": {"name": "plain_tool", "parameters": {"type": "object"}}}),
            )
            .expect("registerTool");
            let tools = state2.api.extension().tools();
            let execute = tools
                .get("plain_tool")
                .expect("registered")
                .definition
                .execute
                .clone();
            // One round-trip then exit (same shutdown-hazard note as above).
            let guest = runtime2.spawn(async move {
                if let Ok(GuestCommand::Dispatch { respond, .. }) = rx2.recv() {
                    let _ = respond.send(Ok(json!({
                        "content": [{"type": "text", "text": "done"}],
                        "details": null,
                    })));
                }
            });
            let request = ToolExecuteRequest {
                tool_call_id: "call_2".to_owned(),
                params: json!({}),
                signal: CancellationToken::new(),
                on_update: None,
            };
            let ctx = state2.api.context();
            let result = runtime2
                .block_on(async { execute(request, ctx).await })
                .expect("tool result");
            let text = match result.content.first() {
                Some(rpi_ai::types::ToolResultContent::Text(text)) => text.text.clone(),
                other => panic!("expected text content: {other:?}"),
            };
            assert_eq!(text, "done");
            guest.abort();
            assert!(
                bridge2.aborts().is_empty(),
                "no cancellation → no dispose: {:?}",
                bridge2.aborts()
            );
        }
    }
}
