//! `ui.*` host-call methods (T15 W6) — the 28 `UiBridge` methods behind
//! `rpi_host_call`. Sync methods run inline; dialog methods
//! (`select`/`confirm`/`input`/`editor`/`custom`) spawn onto the ambient
//! runtime and block the guest thread.

use std::sync::Arc;

use serde_json::{json, Value};

use super::host_call::{block_on, str_arg};
use super::HostState;
use crate::api::{
    ExtensionWidgetOptions, NotifyType, UiDialogOptions, WidgetContent, WidgetPlacement,
    WorkingIndicatorOptions,
};

type CallResult = Result<Value, (&'static str, String)>;

fn err<T>(kind: &'static str, message: impl Into<String>) -> Result<T, (&'static str, String)> {
    Err((kind, message.into()))
}

fn dialog_options(args: &Value) -> Option<UiDialogOptions> {
    args.get("timeout")
        .and_then(Value::as_u64)
        .map(|timeout| UiDialogOptions {
            timeout: Some(timeout),
        })
}

fn widget_content(args: &Value) -> Option<WidgetContent> {
    let content = args.get("content")?;
    if content.is_null() {
        return None;
    }
    if let Some(lines) = content.as_array().and_then(|a| {
        a.iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    }) {
        return Some(WidgetContent::Lines(lines));
    }
    Some(WidgetContent::Component(content.clone()))
}

pub(crate) fn dispatch(state: &mut HostState, method: &str, args: Value) -> CallResult {
    // v0.1.4 C0 (ADR-0024): the seven interactive custom UI host-calls are
    // frozen in the method table but not implemented yet. They answer
    // `unknownMethod` (the R-U9.2 probe signal) **before** the UI-bridge
    // lookup, so the answer does not depend on the bridge being bound — a
    // guest probing during load sees the same "unsupported" result as one
    // probing later. Capability `ui` was already enforced by the caller.
    if crate::interactive_ui::is_interactive_ui_method(method) {
        return err(
            "unknownMethod",
            format!(
                "{method}: interactive UI ABI is not implemented in this host build \
                 (C0 protocol freeze; implementation lands with C1/C2)"
            ),
        );
    }
    let ui = state
        .api
        .context()
        .ui()
        .map_err(|e| ("stale", e.to_string()))?;
    match method {
        "ui.select" => {
            let title = str_arg(&args, "title").unwrap_or_default().to_owned();
            let options: Vec<String> = args
                .get("options")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let dialog = dialog_options(&args);
            let handle = state.async_handle.clone();
            let result = block_on(
                &handle,
                async move { ui.select(&title, &options, dialog).await },
            )?;
            Ok(result.map(Value::from).unwrap_or(Value::Null))
        }
        "ui.confirm" => {
            let title = str_arg(&args, "title").unwrap_or_default().to_owned();
            let message = str_arg(&args, "message").unwrap_or_default().to_owned();
            let dialog = dialog_options(&args);
            let handle = state.async_handle.clone();
            let result = block_on(&handle, async move {
                ui.confirm(&title, &message, dialog).await
            })?;
            Ok(json!(result))
        }
        "ui.input" => {
            let title = str_arg(&args, "title").unwrap_or_default().to_owned();
            let placeholder = str_arg(&args, "placeholder").map(str::to_owned);
            let dialog = dialog_options(&args);
            let handle = state.async_handle.clone();
            let result = block_on(&handle, async move {
                ui.input(&title, placeholder.as_deref(), dialog).await
            })?;
            Ok(result.map(Value::from).unwrap_or(Value::Null))
        }
        "ui.editor" => {
            let title = str_arg(&args, "title").unwrap_or_default().to_owned();
            let prefill = str_arg(&args, "prefill").map(str::to_owned);
            let handle = state.async_handle.clone();
            let result = block_on(&handle, async move {
                ui.editor(&title, prefill.as_deref()).await
            })?;
            Ok(result.map(Value::from).unwrap_or(Value::Null))
        }
        "ui.notify" => {
            ui.notify(
                str_arg(&args, "message").unwrap_or_default(),
                match str_arg(&args, "notifyType") {
                    Some("warning") => NotifyType::Warning,
                    Some("error") => NotifyType::Error,
                    _ => NotifyType::Info,
                },
            );
            Ok(Value::Null)
        }
        "ui.setStatus" => {
            ui.set_status(
                str_arg(&args, "key").unwrap_or_default(),
                str_arg(&args, "text"),
            );
            Ok(Value::Null)
        }
        "ui.setWorkingMessage" => {
            ui.set_working_message(str_arg(&args, "message"));
            Ok(Value::Null)
        }
        "ui.setWorkingVisible" => {
            ui.set_working_visible(args.get("visible").and_then(Value::as_bool).unwrap_or(true));
            Ok(Value::Null)
        }
        "ui.setWorkingIndicator" => {
            let options = args.get("options").and_then(|o| {
                if o.is_null() {
                    return None;
                }
                Some(WorkingIndicatorOptions {
                    frames: o
                        .get("frames")
                        .and_then(|f| serde_json::from_value(f.clone()).ok()),
                    interval_ms: o.get("intervalMs").and_then(Value::as_u64),
                })
            });
            ui.set_working_indicator(options);
            Ok(Value::Null)
        }
        "ui.setHiddenThinkingLabel" => {
            ui.set_hidden_thinking_label(str_arg(&args, "label"));
            Ok(Value::Null)
        }
        "ui.setWidget" => {
            let placement = match str_arg(&args, "placement") {
                Some("belowEditor") => Some(WidgetPlacement::BelowEditor),
                Some("aboveEditor") => Some(WidgetPlacement::AboveEditor),
                _ => None,
            };
            ui.set_widget(
                str_arg(&args, "key").unwrap_or_default(),
                widget_content(&args),
                placement.map(|p| ExtensionWidgetOptions { placement: Some(p) }),
            );
            Ok(Value::Null)
        }
        "ui.setFooter" => {
            ui.set_footer(args.get("component").cloned().filter(|c| !c.is_null()));
            Ok(Value::Null)
        }
        "ui.setHeader" => {
            ui.set_header(args.get("component").cloned().filter(|c| !c.is_null()));
            Ok(Value::Null)
        }
        "ui.setTitle" => {
            ui.set_title(str_arg(&args, "title").unwrap_or_default());
            Ok(Value::Null)
        }
        "ui.custom" => {
            let component = args.get("component").cloned().unwrap_or(Value::Null);
            let options = args.get("options").cloned();
            let handle = state.async_handle.clone();
            let result = block_on(&handle, async move { ui.custom(component, options).await })?;
            Ok(result.unwrap_or(Value::Null))
        }
        "ui.pasteToEditor" => {
            ui.paste_to_editor(str_arg(&args, "text").unwrap_or_default());
            Ok(Value::Null)
        }
        "ui.setEditorText" => {
            ui.set_editor_text(str_arg(&args, "text").unwrap_or_default());
            Ok(Value::Null)
        }
        "ui.getEditorText" => Ok(json!(ui.get_editor_text())),
        "ui.addAutocompleteProvider" => {
            ui.add_autocomplete_provider(args.get("provider").cloned().unwrap_or(Value::Null));
            Ok(Value::Null)
        }
        "ui.setEditorComponent" => {
            ui.set_editor_component(args.get("component").cloned().filter(|c| !c.is_null()));
            Ok(Value::Null)
        }
        "ui.getEditorComponent" => Ok(ui.get_editor_component().unwrap_or(Value::Null)),
        "ui.theme" => Ok(ui.theme()),
        "ui.getAllThemes" => Ok(serde_json::to_value(ui.get_all_themes()).unwrap_or(Value::Null)),
        "ui.getTheme" => Ok(str_arg(&args, "name")
            .and_then(|name| ui.get_theme(name))
            .unwrap_or(Value::Null)),
        "ui.setTheme" => {
            let result = ui.set_theme(args.get("theme").cloned().unwrap_or(Value::Null));
            Ok(serde_json::to_value(result).unwrap_or(Value::Null))
        }
        "ui.getToolsExpanded" => Ok(json!(ui.get_tools_expanded())),
        "ui.setToolsExpanded" => {
            ui.set_tools_expanded(
                args.get("expanded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            Ok(Value::Null)
        }
        // `onTerminalInput` requires a guest-side listener registration;
        // wire: `ui.onTerminalInput` registers a forwarder.
        "ui.onTerminalInput" => {
            // Terminal input forwarding needs a guest handler id; v1
            // delivers nothing (no handler table yet). Acknowledge the
            // registration as a no-op, but say so loudly (ADR-0007: gaps
            // are not silent — a guest must be able to detect them).
            tracing::warn!(
                "extension called ui.onTerminalInput: not supported by the rpi host (v1 has \
                 no guest handler table); the registration is a no-op"
            );
            let unsubscribe = ui.on_terminal_input(Arc::new(|_data| None));
            std::mem::forget(unsubscribe);
            Ok(Value::Null)
        }
        _ => err("unknownMethod", format!("unknown host call: {method}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    use crate::api::{ExtensionApi, ExtensionRuntime, LoadedExtension};
    use crate::wasm::{Capability, DispatchTarget, HostState, NativeForward};

    extern "C" fn dummy_dispatch(
        _cookie: crate::native::PluginCookie,
        _message: abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8> {
        abi_stable::std_types::RVec::from(Vec::new())
    }

    fn host_state(capabilities: HashSet<Capability>) -> (HostState, tokio::runtime::Runtime) {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let api = ExtensionApi::for_extension(
            Arc::new(LoadedExtension::new("<inline:c0>", "<inline:c0>")),
            ExtensionRuntime::new(),
            "/test-cwd",
        );
        let state = HostState {
            api,
            capabilities,
            async_handle: runtime.handle().clone(),
            forward: DispatchTarget::Native(NativeForward {
                dispatch_fn: dummy_dispatch,
                cookie: 0,
            }),
            in_command: std::cell::Cell::new(false),
            tool_updates: Default::default(),
            tool_aborts: Default::default(),
        };
        (state, runtime)
    }

    /// V14-20 FR-E (R-U9.2): all seven C0-frozen methods answer
    /// `unknownMethod` — the guest probe signal — without touching the UI
    /// bridge (so an unbound bridge cannot turn the probe into `stale`).
    #[test]
    fn interactive_ui_methods_answer_unknown_method_in_c0() {
        let (mut state, _runtime) = host_state(HashSet::from([Capability::Ui]));
        for method in crate::interactive_ui::INTERACTIVE_UI_METHODS {
            match dispatch(&mut state, method, serde_json::json!({})) {
                Err((kind, message)) => {
                    assert_eq!(kind, "unknownMethod", "{method}");
                    assert!(
                        message.contains("C0 protocol freeze"),
                        "{method}: {message}"
                    );
                }
                Ok(value) => panic!("{method} unexpectedly succeeded: {value}"),
            }
        }
    }

    /// V14-20 FR-E (R-U8.1): the capability gate runs before the empty
    /// implementation, and the pre-existing `ui.custom` path is untouched
    /// (G2: it still reaches its own arm instead of the C0 answer).
    #[test]
    fn interactive_ui_capability_gate_precedes_c0_and_ui_custom_is_untouched() {
        let request = serde_json::to_vec(&serde_json::json!({
            "call": "ui.mountComponent",
            "args": {},
            "seq": 1,
        }))
        .expect("request json");
        let (mut state, _runtime) = host_state(HashSet::new());
        let response: Value =
            serde_json::from_slice(&crate::wasm::handle_host_call(&mut state, &request))
                .expect("response json");
        assert_eq!(response["error"]["kind"], "capabilityDenied", "{response}");

        let (mut state, _runtime) = host_state(HashSet::from([Capability::Ui]));
        // `ui.custom` keeps its own arm: an unbound slot resolves through the
        // upstream `noOpUIContext` equivalent (NullUiBridge) and returns null,
        // exactly as before C0 — it is not intercepted by the C0 answer.
        match dispatch(
            &mut state,
            "ui.custom",
            serde_json::json!({"component": null}),
        ) {
            Ok(value) => assert_eq!(value, Value::Null),
            Err((kind, message)) => panic!("ui.custom must keep its own arm: {kind}: {message}"),
        }
    }
}
