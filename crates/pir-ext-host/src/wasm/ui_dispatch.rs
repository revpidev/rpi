//! `ui.*` host-call methods (T15 W6) — the 28 `UiBridge` methods behind
//! `pir_host_call`. Sync methods run inline; dialog methods
//! (`select`/`confirm`/`input`/`editor`/`custom`) spawn onto the ambient
//! runtime and block the guest thread.

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
                "extension called ui.onTerminalInput: not supported by the pir host (v1 has \
                 no guest handler table); the registration is a no-op"
            );
            let unsubscribe = ui.on_terminal_input(Arc::new(|_data| None));
            std::mem::forget(unsubscribe);
            Ok(Value::Null)
        }
        _ => err("unknownMethod", format!("unknown host call: {method}")),
    }
}

use std::sync::Arc;
