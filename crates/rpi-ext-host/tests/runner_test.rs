//! Tests for `ExtensionRunnerCore` — conflict rules and emit dispatch
//! semantics, anchored to `external/pi/packages/coding-agent/src/core/
//! extensions/runner.ts` @ 2efa728 (line numbers per assertion).

use std::sync::{Arc, Mutex};

use rpi_ext_host::api::{EventHandler, ExtensionApi};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types::{
    self as ext, ExtensionError, ToolDefinition, EVENT_BEFORE_PROVIDER_HEADERS,
    EVENT_BEFORE_PROVIDER_REQUEST, EVENT_SESSION_BEFORE_SWITCH, EVENT_SESSION_START,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A sync JSON handler wrapped into the async [`EventHandler`] shape.
fn json_handler(
    f: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
) -> EventHandler {
    Arc::new(move |payload, _ctx| {
        let result = f(payload);
        Box::pin(async move { result })
    })
}

/// An inline extension whose factory runs `register` against the API.
fn inline_ext(
    name: &str,
    register: impl Fn(&ExtensionApi) + Send + Sync + 'static,
) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| {
        register(&api);
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Named {
        name: name.to_owned(),
        factory,
        hidden: false,
    }
}

async fn host_with(extensions: Vec<InlineExtension>) -> NativeExtensionHost {
    let host = NativeExtensionHost::new("/test-cwd");
    let errors = host.load_inline(&extensions).await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    host
}

fn minimal_tool(name: &str, label: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        label: label.to_owned(),
        description: format!("{name} description"),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_req, _ctx| {
            Box::pin(async { Ok(rpi_agent::types::AgentToolResult::default()) })
        }),
        render_call: None,
        render_result: None,
    }
}

fn noop_command_handler() -> ext::CommandHandlerFn {
    Arc::new(|_args, _ctx| Box::pin(async { Ok(()) }))
}

fn noop_shortcut_handler() -> ext::ShortcutHandlerFn {
    Arc::new(|_ctx| Box::pin(async { Ok(()) }))
}

fn collect_errors(host: &NativeExtensionHost) -> Arc<Mutex<Vec<ExtensionError>>> {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    // Dropping the unsubscribe closure does NOT unsubscribe (it must be
    // called), so the listener stays active for the test's duration.
    let _unsub = host.on_error(Arc::new(move |error| {
        sink.lock().unwrap_or_else(|e| e.into_inner()).push(error);
    }));
    errors
}

// ---------------------------------------------------------------------------
// Tool conflict rules (runner.ts:446-468)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_resolves_tool_conflicts_first_registration_wins() {
    // runner.ts:447-457 — across extensions the first registration wins.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_tool(minimal_tool("dup", "from-a")).unwrap();
            api.register_tool(minimal_tool("only-a", "a")).unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_tool(minimal_tool("dup", "from-b")).unwrap();
            api.register_tool(minimal_tool("only-b", "b")).unwrap();
        }),
    ])
    .await;

    let tools = host.get_all_registered_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.definition.name.as_str()).collect();
    assert_eq!(names, ["dup", "only-a", "only-b"]);
    let dup = &tools[0];
    assert_eq!(dup.definition.label, "from-a");
    assert_eq!(dup.source_info.path, "<inline:ext-a>");

    // getToolDefinition follows the same first-wins rule (runner.ts:460-468).
    assert_eq!(host.get_tool_definition("dup").unwrap().label, "from-a");
}

#[tokio::test]
async fn runner_tool_reregister_within_same_extension_overwrites_in_place() {
    // JS `Map.set` keeps the original insertion position
    // (loader.ts:245-252).
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.register_tool(minimal_tool("one", "v1")).unwrap();
        api.register_tool(minimal_tool("two", "t")).unwrap();
        api.register_tool(minimal_tool("one", "v2")).unwrap();
    })])
    .await;

    let tools = host.get_all_registered_tools();
    let summary: Vec<(&str, &str)> = tools
        .iter()
        .map(|t| (t.definition.name.as_str(), t.definition.label.as_str()))
        .collect();
    assert_eq!(summary, [("one", "v2"), ("two", "t")]);
}

#[tokio::test]
async fn runner_reports_tool_and_flag_conflicts_as_diagnostics() {
    // resource-loader.ts:1003-1038 — conflicts are diagnostics; every
    // extension stays loaded.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_tool(minimal_tool("dup", "a")).unwrap();
            api.register_flag("verbose", None, ext::FlagType::Boolean, None)
                .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_tool(minimal_tool("dup", "b")).unwrap();
            api.register_flag("verbose", None, ext::FlagType::Boolean, None)
                .unwrap();
        }),
    ])
    .await;

    let diagnostics = host.detect_extension_conflicts();
    let messages: Vec<(&str, &str)> = diagnostics
        .iter()
        .map(|d| (d.message.as_str(), d.path.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        messages,
        [
            (
                "Tool \"dup\" conflicts with <inline:ext-a>",
                "<inline:ext-b>"
            ),
            (
                "Flag \"--verbose\" conflicts with <inline:ext-a>",
                "<inline:ext-b>"
            ),
        ]
    );
    // Both extensions remain registered.
    assert_eq!(
        host.get_extension_paths(),
        ["<inline:ext-a>", "<inline:ext-b>"]
    );
}

// ---------------------------------------------------------------------------
// Flag rules (runner.ts:470-488, loader.ts:274-283)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_resolves_flag_conflicts_first_registration_wins() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_flag(
                "output",
                Some("a".to_owned()),
                ext::FlagType::String,
                Some(ext::FlagValue::String("a-default".to_owned())),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_flag(
                "output",
                Some("b".to_owned()),
                ext::FlagType::String,
                Some(ext::FlagValue::String("b-default".to_owned())),
            )
            .unwrap();
        }),
    ])
    .await;

    let flags = host.get_flags();
    let flag = flags.get("output").unwrap();
    assert_eq!(flag.description.as_deref(), Some("a"));
    assert_eq!(flag.extension_path, "<inline:ext-a>");
    // The first registration's default seeds the flag values
    // (loader.ts:280-282 — first writer wins).
    assert_eq!(
        host.runtime().get_flag_value("output"),
        Some(ext::FlagValue::String("a-default".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// Command conflict rules (runner.ts:595-629)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_resolves_command_conflicts_with_numeric_suffix() {
    // runner.ts:609-628 — all commands are kept; names registered more than
    // once get `:N` suffixes; a clashing literal name bumps the suffix.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_command("dup", None, noop_command_handler())
                .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_command("dup", None, noop_command_handler())
                .unwrap();
            api.register_command("dup:2", None, noop_command_handler())
                .unwrap();
            api.register_command("solo", None, noop_command_handler())
                .unwrap();
        }),
    ])
    .await;

    let resolved = host.get_registered_commands();
    let invocations: Vec<&str> = resolved
        .iter()
        .map(|c| c.invocation_name.as_str())
        .collect();
    // "dup" x2 → dup:1, dup:2; the literal "dup:2" command collides with
    // the taken suffix and bumps by re-suffixing its own name
    // (`${name}:${suffix}`, runner.ts:615-621); unique names keep theirs.
    assert_eq!(invocations, ["dup:1", "dup:2", "dup:2:2", "solo"]);

    assert_eq!(
        host.get_command("dup:2").unwrap().source_info.path,
        "<inline:ext-b>"
    );
    assert!(host.get_command("missing").is_none());
}

// ---------------------------------------------------------------------------
// Shortcut rules (runner.ts:67-111, 490-537)
// ---------------------------------------------------------------------------

fn test_keybindings() -> Vec<(String, Vec<String>)> {
    vec![
        ("app.interrupt".to_owned(), vec!["ctrl+c".to_owned()]),
        // Not in RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.
        ("app.session.new".to_owned(), vec!["ctrl+n".to_owned()]),
    ]
}

#[tokio::test]
async fn runner_shortcut_reserved_builtin_key_skips_extension() {
    // runner.ts:507-513 — reserved keys skip the extension shortcut with a
    // diagnostic.
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.register_shortcut("Ctrl+C", None, noop_shortcut_handler())
            .unwrap();
        api.register_shortcut("ctrl+x", None, noop_shortcut_handler())
            .unwrap();
    })])
    .await;

    let shortcuts = host.get_shortcuts(&test_keybindings());
    assert!(shortcuts.get("ctrl+c").is_none());
    assert!(shortcuts.get("ctrl+x").is_some());

    let diagnostics = host.get_shortcut_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].message,
        "Extension shortcut 'ctrl+c' from <inline:ext-a> conflicts with built-in shortcut. Skipping."
    );
}

#[tokio::test]
async fn runner_shortcut_non_reserved_builtin_conflict_extension_wins() {
    // runner.ts:515-520 — non-reserved built-in conflict: diagnostic, the
    // extension shortcut wins.
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.register_shortcut("ctrl+n", None, noop_shortcut_handler())
            .unwrap();
    })])
    .await;

    let shortcuts = host.get_shortcuts(&test_keybindings());
    assert_eq!(
        shortcuts.get("ctrl+n").unwrap().extension_path,
        "<inline:ext-a>"
    );
    let diagnostics = host.get_shortcut_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].message,
        "Extension shortcut conflict: 'ctrl+n' is built-in shortcut for app.session.new and <inline:ext-a>. Using <inline:ext-a>."
    );
}

#[tokio::test]
async fn runner_shortcut_conflicts_last_wins_with_diagnostic() {
    // runner.ts:522-530 — extension-vs-extension conflict: last wins.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_shortcut("ctrl+x", None, noop_shortcut_handler())
                .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_shortcut("ctrl+x", None, noop_shortcut_handler())
                .unwrap();
        }),
    ])
    .await;

    let shortcuts = host.get_shortcuts(&test_keybindings());
    assert_eq!(
        shortcuts.get("ctrl+x").unwrap().extension_path,
        "<inline:ext-b>"
    );
    let diagnostics = host.get_shortcut_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].message,
        "Extension shortcut conflict: 'ctrl+x' registered by both <inline:ext-a> and <inline:ext-b>. Using <inline:ext-b>."
    );
}

// ---------------------------------------------------------------------------
// Renderer rules (runner.ts:575-593)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_renderer_first_registration_wins_silently() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_message_renderer(
                "card",
                Arc::new(|_msg, _opts| Ok(Some(json!({"type": "text", "props": {"text": "a"}})))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_message_renderer(
                "card",
                Arc::new(|_msg, _opts| Ok(Some(json!({"type": "text", "props": {"text": "b"}})))),
            )
            .unwrap();
        }),
    ])
    .await;

    let renderer = host.get_message_renderer("card").unwrap();
    let tree = renderer(
        json!({}),
        ext::MessageRenderOptions {
            expanded: false,
            output_pad: 0,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(tree["props"]["text"], "a");
    // Silent: no diagnostics channel for renderer conflicts.
    assert!(host.get_command_diagnostics().is_empty());
    assert!(host.get_shortcut_diagnostics().is_empty());
    assert!(host.get_message_renderer("other").is_none());
}

/// Entry renderer: same first-registration-wins-silently rule as
/// `registerMessageRenderer` (runner.ts:628-629 handles both maps).
#[tokio::test]
async fn runner_entry_renderer_first_registration_wins_silently() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_entry_renderer(
                "card",
                Arc::new(|_entry, _opts| Ok(Some(json!({"type": "text", "props": {"text": "a"}})))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_entry_renderer(
                "card",
                Arc::new(|_entry, _opts| Ok(Some(json!({"type": "text", "props": {"text": "b"}})))),
            )
            .unwrap();
        }),
    ])
    .await;

    let renderer = host.get_entry_renderer("card").unwrap();
    let tree = renderer(json!({}), ext::EntryRenderOptions { expanded: false })
        .unwrap()
        .unwrap();
    assert_eq!(tree["props"]["text"], "a");
    assert!(host.get_command_diagnostics().is_empty());
    assert!(host.get_entry_renderer("other").is_none());
}

// ---------------------------------------------------------------------------
// Generic emit (runner.ts:788-820)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_emit_dispatches_serially_in_load_then_registration_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let make = |name: &'static str, log: Arc<Mutex<Vec<&'static str>>>| {
        inline_ext(name, move |api| {
            for suffix in ["1", "2"] {
                let entry: &'static str = match (name, suffix) {
                    ("ext-a", "1") => "a1",
                    ("ext-a", _) => "a2",
                    ("ext-b", "1") => "b1",
                    _ => "b2",
                };
                let log = log.clone();
                api.on(
                    EVENT_SESSION_START,
                    json_handler(move |_| {
                        log.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
                        Ok(Value::Null)
                    }),
                )
                .unwrap();
            }
        })
    };
    let host = host_with(vec![make("ext-a", log.clone()), make("ext-b", log.clone())]).await;

    assert!(host.has_handlers(EVENT_SESSION_START));
    assert!(!host.has_handlers(ext::EVENT_AGENT_START));
    host.emit(EVENT_SESSION_START, json!({"type": EVENT_SESSION_START}))
        .await;
    assert_eq!(
        log.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        ["a1", "a2", "b1", "b2"]
    );
}

#[tokio::test]
async fn runner_emit_isolates_handler_errors_and_continues() {
    // runner.ts:806-815 — a throwing handler is reported via emitError and
    // later handlers still run.
    let log = Arc::new(Mutex::new(Vec::new()));
    let hit = log.clone();
    let host = host_with(vec![
        inline_ext("bad", |api| {
            api.on(
                EVENT_SESSION_START,
                json_handler(|_| Err("boom".to_owned())),
            )
            .unwrap();
        }),
        inline_ext("good", move |api| {
            let hit = hit.clone();
            api.on(
                EVENT_SESSION_START,
                json_handler(move |_| {
                    hit.lock().unwrap_or_else(|e| e.into_inner()).push("ran");
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;
    let errors = collect_errors(&host);

    host.emit(EVENT_SESSION_START, json!({"type": EVENT_SESSION_START}))
        .await;

    assert_eq!(
        log.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        ["ran"]
    );
    let errors = errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].extension_path, "<inline:bad>");
    assert_eq!(errors[0].event, EVENT_SESSION_START);
    assert_eq!(errors[0].error, "boom");
}

#[tokio::test]
async fn runner_emit_error_unsubscribe_stops_delivery() {
    // runner.ts:554-557 — onError returns an unsubscribe closure.
    let host = host_with(vec![inline_ext("bad", |api| {
        api.on(EVENT_SESSION_START, json_handler(|_| Err("x".to_owned())))
            .unwrap();
    })])
    .await;
    // Always-on listener.
    let errors = collect_errors(&host);
    // Second listener, unsubscribed before any emit.
    let transient = Arc::new(Mutex::new(Vec::new()));
    let sink = transient.clone();
    let unsubscribe = host.on_error(Arc::new(move |error| {
        sink.lock().unwrap_or_else(|e| e.into_inner()).push(error);
    }));
    unsubscribe();

    host.emit(EVENT_SESSION_START, json!({})).await;
    assert_eq!(errors.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    assert!(transient
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());

    host.emit(EVENT_SESSION_START, json!({})).await;
    assert_eq!(errors.lock().unwrap_or_else(|e| e.into_inner()).len(), 2);
    assert!(transient
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());
}

// ---------------------------------------------------------------------------
// session_before_* cancel (runner.ts:779-786, 800-804)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_session_before_cancel_short_circuits() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let first = log.clone();
    let second = log.clone();
    let host = host_with(vec![
        inline_ext("ext-a", move |api| {
            let first = first.clone();
            api.on(
                EVENT_SESSION_BEFORE_SWITCH,
                json_handler(move |_| {
                    first.lock().unwrap_or_else(|e| e.into_inner()).push("a");
                    Ok(json!({"cancel": true}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", move |api| {
            let second = second.clone();
            api.on(
                EVENT_SESSION_BEFORE_SWITCH,
                json_handler(move |_| {
                    second.lock().unwrap_or_else(|e| e.into_inner()).push("b");
                    Ok(json!({"cancel": false}))
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit(
            EVENT_SESSION_BEFORE_SWITCH,
            json!({"type": EVENT_SESSION_BEFORE_SWITCH}),
        )
        .await
        .unwrap();
    assert_eq!(result["cancel"], true);
    // ext-b never ran.
    assert_eq!(
        log.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        ["a"]
    );
}

#[tokio::test]
async fn runner_session_before_last_non_null_result_wins_without_cancel() {
    // runner.ts:800-804 — the latest non-null result is returned.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                EVENT_SESSION_BEFORE_SWITCH,
                json_handler(|_| Ok(json!({"cancel": false}))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                EVENT_SESSION_BEFORE_SWITCH,
                json_handler(|_| Ok(Value::Null)),
            )
            .unwrap();
            api.on(
                EVENT_SESSION_BEFORE_SWITCH,
                json_handler(|_| Ok(json!({"skipConversationRestore": true}))),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit(EVENT_SESSION_BEFORE_SWITCH, json!({}))
        .await
        .unwrap();
    assert_eq!(result["skipConversationRestore"], true);
}

// ---------------------------------------------------------------------------
// message_end (runner.ts:822-862)
// ---------------------------------------------------------------------------

fn user_message(text: &str) -> Value {
    json!({"role": "user", "content": text, "timestamp": 1})
}

#[tokio::test]
async fn runner_message_end_replaces_and_chains() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_MESSAGE_END,
                json_handler(|event| {
                    let mut message = event["message"].clone();
                    message["content"] = Value::String("patched".to_owned());
                    Ok(json!({"message": message}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_MESSAGE_END,
                json_handler(|event| {
                    // Observes ext-a's replacement.
                    assert_eq!(event["message"]["content"], "patched");
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_message_end(json!({"type": ext::EVENT_MESSAGE_END, "message": user_message("hi")}))
        .await
        .unwrap();
    assert_eq!(result["content"], "patched");
}

#[tokio::test]
async fn runner_message_end_role_mismatch_is_rejected() {
    // runner.ts:837-844 — replacements must keep the role.
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_MESSAGE_END,
            json_handler(|_| {
                Ok(json!({"message": {"role": "assistant", "content": [], "timestamp": 2}}))
            }),
        )
        .unwrap();
    })])
    .await;
    let errors = collect_errors(&host);

    let result = host
        .emit_message_end(json!({"type": ext::EVENT_MESSAGE_END, "message": user_message("hi")}))
        .await;
    assert!(result.is_none());
    let errors = errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].error,
        "message_end handlers must return a message with the same role"
    );
}

#[tokio::test]
async fn runner_message_end_unmodified_returns_none() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(ext::EVENT_MESSAGE_END, json_handler(|_| Ok(Value::Null)))
            .unwrap();
    })])
    .await;
    assert!(host
        .emit_message_end(json!({"type": ext::EVENT_MESSAGE_END, "message": user_message("hi")}))
        .await
        .is_none());
}

// ---------------------------------------------------------------------------
// tool_result patch chaining (runner.ts:864-917)
// ---------------------------------------------------------------------------

fn tool_result_event() -> Value {
    json!({
        "type": ext::EVENT_TOOL_RESULT,
        "toolCallId": "call-1",
        "toolName": "bash",
        "input": {"command": "ls"},
        "content": [{"type": "text", "text": "original"}],
        "isError": false,
    })
}

#[tokio::test]
async fn runner_tool_result_partial_patches_chain_across_handlers() {
    // runner.ts:878-893 — each present field patches the event; later
    // handlers observe earlier patches.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_TOOL_RESULT,
                json_handler(|_| Ok(json!({"content": [{"type": "text", "text": "patched"}]}))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_TOOL_RESULT,
                json_handler(|event| {
                    assert_eq!(event["content"][0]["text"], "patched");
                    Ok(json!({"isError": true}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-c", |api| {
            // Null result = no patch (undefined upstream).
            api.on(ext::EVENT_TOOL_RESULT, json_handler(|_| Ok(Value::Null)))
                .unwrap();
        }),
    ])
    .await;

    let result = host.emit_tool_result(tool_result_event()).await.unwrap();
    // The aggregated result carries exactly the patchable fields
    // (runner.ts:911-916).
    assert_eq!(result["content"][0]["text"], "patched");
    assert_eq!(result["isError"], true);
}

#[tokio::test]
async fn runner_tool_result_unmodified_returns_none() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_TOOL_RESULT,
            json_handler(|_| Ok(json!({"content": Value::Null, "isError": Value::Null}))),
        )
        .unwrap();
    })])
    .await;
    assert!(host.emit_tool_result(tool_result_event()).await.is_none());
}

// ---------------------------------------------------------------------------
// tool_call (runner.ts:919-940)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_tool_call_block_short_circuits() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(ext::EVENT_TOOL_CALL, json_handler(|_| Ok(Value::Null)))
                .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_TOOL_CALL,
                json_handler(|_| Ok(json!({"block": true, "reason": "denied"}))),
            )
            .unwrap();
        }),
        inline_ext("ext-c", |api| {
            api.on(
                ext::EVENT_TOOL_CALL,
                json_handler(|_| panic!("must not run after block")),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_tool_call(json!({"type": ext::EVENT_TOOL_CALL, "toolCallId": "c1", "toolName": "bash", "input": {}}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["block"], true);
    assert_eq!(result["reason"], "denied");
}

#[tokio::test]
async fn runner_tool_call_handler_errors_propagate() {
    // runner.ts:927-935 — NO try/catch here upstream; a throwing handler
    // propagates to the caller.
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_TOOL_CALL,
            json_handler(|_| Err("tool_call boom".to_owned())),
        )
        .unwrap();
    })])
    .await;

    let error = host
        .emit_tool_call(json!({"type": ext::EVENT_TOOL_CALL, "toolCallId": "c1", "toolName": "bash", "input": {}}))
        .await
        .unwrap_err();
    assert_eq!(error.extension_path, "<inline:ext-a>");
    assert_eq!(error.event, ext::EVENT_TOOL_CALL);
    assert_eq!(error.error, "tool_call boom");
}

// ---------------------------------------------------------------------------
// user_bash (runner.ts:942-969)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_user_bash_first_non_null_result_wins() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_USER_BASH,
                json_handler(|_| Err("ignored".to_owned())),
            )
            .unwrap();
            api.on(ext::EVENT_USER_BASH, json_handler(|_| Ok(Value::Null)))
                .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_USER_BASH,
                json_handler(|_| Ok(json!({"result": {"output": "done"}}))),
            )
            .unwrap();
        }),
        inline_ext("ext-c", |api| {
            api.on(
                ext::EVENT_USER_BASH,
                json_handler(|_| panic!("must not run after first result")),
            )
            .unwrap();
        }),
    ])
    .await;
    let errors = collect_errors(&host);

    let result = host
        .emit_user_bash(json!({"type": ext::EVENT_USER_BASH, "command": "ls", "excludeFromContext": false, "cwd": "/x"}))
        .await
        .unwrap();
    assert_eq!(result["result"]["output"], "done");
    // ext-a's throw was isolated and reported.
    assert_eq!(errors.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
}

// ---------------------------------------------------------------------------
// context (runner.ts:971-1001)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_context_transforms_chain() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_CONTEXT,
                json_handler(|event| {
                    let mut messages = event["messages"].as_array().unwrap().clone();
                    messages.push(json!({"tag": "a"}));
                    Ok(json!({"messages": messages}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_CONTEXT,
                json_handler(|event| {
                    // Observes ext-a's transform.
                    assert_eq!(event["messages"].as_array().unwrap().len(), 2);
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host.emit_context(json!([{"tag": "start"}])).await;
    assert_eq!(result.as_array().unwrap().len(), 2);
    assert_eq!(result[1]["tag"], "a");
}

// ---------------------------------------------------------------------------
// before_provider_request (runner.ts:1003-1035)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_before_provider_request_undefined_does_not_replace() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                EVENT_BEFORE_PROVIDER_REQUEST,
                json_handler(|event| {
                    let mut payload = event["payload"].clone();
                    payload["model"] = json!("swapped");
                    Ok(payload)
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                EVENT_BEFORE_PROVIDER_REQUEST,
                json_handler(|event| {
                    // Observes ext-a's replacement.
                    assert_eq!(event["payload"]["model"], "swapped");
                    // undefined → no replacement (runner.ts:1018-1020).
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_before_provider_request(json!({"model": "original"}))
        .await;
    assert_eq!(result["model"], "swapped");
}

// ---------------------------------------------------------------------------
// before_provider_headers (runner.ts:1037-1063 + documented deviation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_before_provider_headers_returned_object_replaces() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                EVENT_BEFORE_PROVIDER_HEADERS,
                json_handler(|event| {
                    let mut headers = event["headers"].clone();
                    headers["x-trace"] = json!("1");
                    // Null value deletes the header (types.ts:678-679).
                    headers["x-drop"] = Value::Null;
                    Ok(headers)
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                EVENT_BEFORE_PROVIDER_HEADERS,
                json_handler(|event| {
                    assert_eq!(event["headers"]["x-trace"], "1");
                    // Non-object results are ignored.
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_before_provider_headers(json!({"x-drop": "gone", "x-keep": "yes"}))
        .await;
    assert_eq!(result["x-trace"], "1");
    assert_eq!(result["x-drop"], Value::Null);
    assert_eq!(result["x-keep"], "yes");
}

// ---------------------------------------------------------------------------
// before_agent_start (runner.ts:1068-1132)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_before_agent_start_collects_messages_and_chains_system_prompt() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_BEFORE_AGENT_START,
                json_handler(|event| {
                    assert_eq!(event["systemPrompt"], "base");
                    Ok(json!({
                        "message": {"customType": "note", "display": false},
                        "systemPrompt": "a-prompt",
                    }))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_BEFORE_AGENT_START,
                json_handler(|event| {
                    // Chained: observes ext-a's replacement (runner.ts:1092-1098).
                    assert_eq!(event["systemPrompt"], "a-prompt");
                    Ok(json!({"message": {"customType": "warn", "display": true}}))
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_before_agent_start(json!({
            "type": ext::EVENT_BEFORE_AGENT_START,
            "prompt": "hi",
            "systemPrompt": "base",
            "systemPromptOptions": {"cwd": "/test-cwd"},
        }))
        .await
        .unwrap();
    assert_eq!(result["systemPrompt"], "a-prompt");
    let messages = result["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["customType"], "note");
    assert_eq!(messages[1]["customType"], "warn");
}

#[tokio::test]
async fn runner_before_agent_start_no_results_returns_none() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_BEFORE_AGENT_START,
            json_handler(|_| Ok(Value::Null)),
        )
        .unwrap();
    })])
    .await;
    assert!(host
        .emit_before_agent_start(
            json!({"type": ext::EVENT_BEFORE_AGENT_START, "prompt": "hi", "systemPrompt": "base"})
        )
        .await
        .is_none());
}

// ---------------------------------------------------------------------------
// resources_discover (runner.ts:1134-1180)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_resources_discover_aggregates_paths_with_extension_tags() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_RESOURCES_DISCOVER,
                json_handler(|_| {
                    Ok(json!({"skillPaths": ["/a/skills"], "themePaths": ["/a/themes"]}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_RESOURCES_DISCOVER,
                json_handler(|_| Ok(json!({"promptPaths": ["/b/prompts"]}))),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_resources_discover(
            json!({"type": ext::EVENT_RESOURCES_DISCOVER, "cwd": "/x", "reason": "startup"}),
        )
        .await;
    assert_eq!(
        result["skillPaths"],
        json!([{"path": "/a/skills", "extensionPath": "<inline:ext-a>"}])
    );
    assert_eq!(
        result["promptPaths"],
        json!([{"path": "/b/prompts", "extensionPath": "<inline:ext-b>"}])
    );
    assert_eq!(
        result["themePaths"],
        json!([{"path": "/a/themes", "extensionPath": "<inline:ext-a>"}])
    );
}

// ---------------------------------------------------------------------------
// input (runner.ts:1182-1222)
// ---------------------------------------------------------------------------

fn input_event(text: &str) -> Value {
    json!({
        "type": ext::EVENT_INPUT,
        "text": text,
        "source": "interactive",
    })
}

#[tokio::test]
async fn runner_input_transforms_chain_and_continue_when_unchanged() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(ext::EVENT_INPUT, json_handler(|event| {
                Ok(json!({"action": "transform", "text": format!("{}+a", event["text"].as_str().unwrap())}))
            }))
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(ext::EVENT_INPUT, json_handler(|event| {
                // Observes ext-a's transform (runner.ts:1196-1202).
                assert_eq!(event["text"], "in+a");
                Ok(json!({"action": "transform", "text": "final"}))
            }))
            .unwrap();
        }),
    ])
    .await;

    let result = host.emit_input(input_event("in")).await;
    assert_eq!(result["action"], "transform");
    assert_eq!(result["text"], "final");
}

#[tokio::test]
async fn runner_input_handled_short_circuits() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_INPUT,
                json_handler(|_| Ok(json!({"action": "handled"}))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_INPUT,
                json_handler(|_| panic!("must not run after handled")),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host.emit_input(input_event("in")).await;
    assert_eq!(result["action"], "handled");
}

#[tokio::test]
async fn runner_input_continue_when_nothing_changed() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_INPUT,
            json_handler(|_| Ok(json!({"action": "continue"}))),
        )
        .unwrap();
        // A transform to the identical text reports continue
        // (runner.ts:1219-1221 reference comparison — here: value equality).
        api.on(
            ext::EVENT_INPUT,
            json_handler(|event| Ok(json!({"action": "transform", "text": event["text"].clone()}))),
        )
        .unwrap();
    })])
    .await;
    let result = host.emit_input(input_event("in")).await;
    assert_eq!(result["action"], "continue");
}

#[tokio::test]
async fn runner_input_transform_without_images_keeps_current_images() {
    // `result.images ?? currentImages` (runner.ts:1207).
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_INPUT,
            json_handler(|event| {
                assert_eq!(event["images"][0]["type"], "image");
                Ok(json!({"action": "transform", "text": "changed"}))
            }),
        )
        .unwrap();
    })])
    .await;

    let mut event = input_event("in");
    event["images"] = json!([{"type": "image", "data": "...", "mimeType": "image/png"}]);
    let result = host.emit_input(event).await;
    assert_eq!(result["action"], "transform");
    assert_eq!(result["images"][0]["mimeType"], "image/png");
}

// ---------------------------------------------------------------------------
// project_trust (runner.ts:201-231)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_project_trust_undecided_falls_through_first_decision_wins() {
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_PROJECT_TRUST,
                json_handler(|_| Ok(json!({"trusted": "undecided"}))),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_PROJECT_TRUST,
                json_handler(|_| Ok(json!({"trusted": "no", "remember": true}))),
            )
            .unwrap();
        }),
        inline_ext("ext-c", |api| {
            api.on(
                ext::EVENT_PROJECT_TRUST,
                json_handler(|_| panic!("must not run after a decision")),
            )
            .unwrap();
        }),
    ])
    .await;

    let (result, errors) = host
        .emit_project_trust(json!({"type": ext::EVENT_PROJECT_TRUST, "cwd": "/x"}))
        .await;
    assert!(errors.is_empty());
    let result = result.unwrap();
    assert_eq!(result["trusted"], "no");
    assert_eq!(result["remember"], true);
}

#[tokio::test]
async fn runner_project_trust_errors_are_collected_not_emitted() {
    // runner.ts:220-227 — errors go to the returned list, not emitError.
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_PROJECT_TRUST,
                json_handler(|_| Err("trust boom".to_owned())),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_PROJECT_TRUST,
                json_handler(|_| Ok(json!({"trusted": "yes"}))),
            )
            .unwrap();
        }),
    ])
    .await;
    let emitted = collect_errors(&host);

    let (result, errors) = host
        .emit_project_trust(json!({"type": ext::EVENT_PROJECT_TRUST, "cwd": "/x"}))
        .await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].error, "trust boom");
    assert_eq!(result.unwrap()["trusted"], "yes");
    assert!(emitted.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
}

// ---------------------------------------------------------------------------
// Stale lifecycle (runner.ts:539-552)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_invalidate_marks_stale_first_message_wins() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on(EVENT_SESSION_START, json_handler(|_| Ok(Value::Null)))
            .unwrap();
    })])
    .await;

    assert!(host.assert_active().is_ok());
    host.invalidate(Some("custom stale".to_owned()));
    host.invalidate(Some("later".to_owned()));
    match host.assert_active() {
        Err(rpi_ext_host::ExtError::Stale(message)) => assert_eq!(message, "custom stale"),
        other => panic!("expected stale error, got {other:?}"),
    }
}

#[tokio::test]
async fn runner_tool_call_input_threads_through_handlers() {
    // Divergent implementation (runner.rs emit_tool_call header note): upstream mutates
    // event.input in place; the rpi handler threads the changed value through the result's
    // "input" field, so later handlers see the updated value, and the final input returns
    // with the result (an unchanged-args result carries no "input" key).
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.on(
                ext::EVENT_TOOL_CALL,
                json_handler(|event| {
                    let mut input = event["input"].clone();
                    input["a"] = json!("patched");
                    Ok(json!({"input": input}))
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.on(
                ext::EVENT_TOOL_CALL,
                json_handler(|event| {
                    assert_eq!(event["input"]["a"], "patched");
                    Ok(Value::Null)
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_tool_call(json!({"type": ext::EVENT_TOOL_CALL, "toolCallId": "c1", "toolName": "bash", "input": {"a": 1}}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["input"], json!({"a": "patched"}));

    // Unchanged args: the result carries no "input".
    let host2 = host_with(vec![inline_ext("ext-a", |api| {
        api.on(
            ext::EVENT_TOOL_CALL,
            json_handler(|_| Ok(json!({"reason": "noted"}))),
        )
        .unwrap();
    })])
    .await;
    let result2 = host2
        .emit_tool_call(json!({"type": ext::EVENT_TOOL_CALL, "toolCallId": "c1", "toolName": "bash", "input": {}}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result2["reason"], "noted");
    assert!(result2.get("input").is_none());
}

#[tokio::test]
async fn runner_before_agent_start_ctx_get_system_prompt_reflects_chain() {
    // runner.ts:1075-1082 — during before_agent_start, ctx.getSystemPrompt()
    // returns the current (chained) prompt.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let host = host_with(vec![
        inline_ext("ext-a", move |api| {
            let sink = sink.clone();
            api.on(
                ext::EVENT_BEFORE_AGENT_START,
                std::sync::Arc::new(move |_payload, ctx| {
                    let sink = sink.clone();
                    Box::pin(async move {
                        sink.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(ctx.get_system_prompt().unwrap());
                        Ok(serde_json::json!({"systemPrompt": "chained-A"}))
                    })
                }),
            )
            .unwrap();
        }),
        inline_ext("ext-b", move |api| {
            api.on(
                ext::EVENT_BEFORE_AGENT_START,
                std::sync::Arc::new(move |_payload, ctx| {
                    Box::pin(async move {
                        let current = ctx.get_system_prompt().unwrap();
                        Ok(serde_json::json!({"systemPrompt": format!("{current}+B")}))
                    })
                }),
            )
            .unwrap();
        }),
    ])
    .await;

    let result = host
        .emit_before_agent_start(serde_json::json!({
            "type": ext::EVENT_BEFORE_AGENT_START,
            "prompt": "hi",
            "systemPrompt": "base",
        }))
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        ["base"]
    );
    assert_eq!(result["systemPrompt"], "chained-A+B");
}

// ---------------------------------------------------------------------------
// Event-bus lifecycle via host invalidate (6ca423447)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_invalidate_unsubscribes_extension_event_bus_subscription() {
    // 6ca423447 regression: a subscription made through the extension API's
    // `events().on()` must be cleaned up when the host invalidates the runtime.
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let host = host_with(vec![inline_ext("ext-a", move |api| {
        let sink = sink.clone();
        let _unsub = api.events().on(
            "my-channel",
            Arc::new(move |data| {
                sink.lock().unwrap().push(data);
            }),
        );
        // The tracked wrapper handles lifecycle — forget the handle so it
        // stays subscribed until invalidate().
        std::mem::forget(_unsub);
    })])
    .await;

    host.event_bus().emit("my-channel", json!({"n": 1}));
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[json!({"n": 1})],
        "delivery before invalidate"
    );

    host.invalidate(None);

    host.event_bus().emit("my-channel", json!({"n": 2}));
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[json!({"n": 1})],
        "no delivery after invalidate — subscription was auto-unsubscribed"
    );
}
