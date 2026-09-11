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

/// Map a structured bridge error onto the ABI error-kind table.
fn ui_error(error: crate::interactive_ui::InteractiveUiError) -> (&'static str, String) {
    use crate::interactive_ui::InteractiveUiErrorKind as Kind;
    let kind = match error.kind {
        Kind::CapabilityDenied => "capabilityDenied",
        Kind::InvalidRequest => "invalidRequest",
        Kind::UnknownMethod => "unknownMethod",
        Kind::Call => "call",
        Kind::Internal => "internal",
        Kind::HandlerError => "handlerError",
        Kind::FuelExhausted => "fuelExhausted",
        Kind::ProtocolError => "protocolError",
        // Forward-compatible unknown kinds must never masquerade as a
        // known kind; `internal` is the safe fallback.
        Kind::Other(_) => "internal",
    };
    (kind, error.message)
}

fn parse_ui_args<T: serde::de::DeserializeOwned>(
    method: &str,
    args: Value,
) -> Result<T, (&'static str, String)> {
    serde_json::from_value(args).map_err(|error| ("invalidRequest", format!("{method}: {error}")))
}

/// Whether the calling guest runs on the wasm (L1) carrier (V14-22 C2).
///
/// Native (L0) plugins share this dispatch path, so the carrier-specific
/// execution constraints of R-U7.2 / design §4.2/§4.4 (stricter frame
/// limits, no background-thread `wakeComponent`) are applied here from the
/// per-call dispatch target.
fn is_wasm_carrier(state: &HostState) -> bool {
    matches!(state.forward, crate::wasm::DispatchTarget::Wasm(_))
}

/// Interactive custom UI ABI dispatch (ADR-0024 §2.1). The blocking calls
/// (`pollComponent`, `editExternal`) park the guest thread through the same
/// [`block_on`] mechanism as `ui.select`; the host runtime keeps running.
fn dispatch_interactive_ui(
    state: &mut HostState,
    ui: &Arc<dyn crate::api::UiBridge>,
    method: &str,
    args: Value,
) -> CallResult {
    use crate::interactive_ui::{
        DisposeComponentArgs, EditExternalArgs, MountComponentArgs, PollComponentArgs,
        RenderComponentArgs, SetComponentHiddenArgs, WakeComponentArgs,
    };
    // The context-level namespace is applied by `NamespacedUiBridge`; direct
    // (host-level) callers fall back to the extension path as the owner.
    let owner = state.api.extension().path.clone();
    match method {
        crate::interactive_ui::METHOD_MOUNT_COMPONENT => {
            let mut args: MountComponentArgs = parse_ui_args(method, args)?;
            if is_wasm_carrier(state) {
                // R-U7.2 / design §4.4: the wasm carrier's total frame budget
                // is stricter than native even when the guest asks for more
                // (fuel / memory amplification guard).
                args.options.max_frame_bytes = args
                    .options
                    .max_frame_bytes
                    .min(crate::interactive_ui::WASM_DEFAULT_MAX_FRAME_BYTES);
            }
            let ui = Arc::clone(ui);
            let handle = block_on(&state.async_handle, async move {
                ui.mount_component(&owner, args.options).await
            })?
            .map_err(ui_error)?;
            Ok(json!({ "handle": handle.0 }))
        }
        crate::interactive_ui::METHOD_POLL_COMPONENT => {
            let args: PollComponentArgs = parse_ui_args(method, args)?;
            let ui = Arc::clone(ui);
            let event = block_on(&state.async_handle, async move {
                ui.poll_component(&owner, args.handle).await
            })?
            .map_err(ui_error)?;
            Ok(json!({ "event": event }))
        }
        crate::interactive_ui::METHOD_RENDER_COMPONENT => {
            let args: RenderComponentArgs = parse_ui_args(method, args)?;
            if is_wasm_carrier(state) {
                // R-U7.2 / design §4.4: rows ≤ 2000 on wasm. Rejecting before
                // the registry keeps the previous frame (fail-visible,
                // R-U3.5) and mirrors the registry's `frameTooLarge`
                // `invalidRequest` shape.
                let rows = args.frame.lines.len();
                if rows > crate::interactive_ui::WASM_DEFAULT_MAX_FRAME_ROWS {
                    return err(
                        "invalidRequest",
                        format!(
                            "frameTooLarge: rows {rows} > {}",
                            crate::interactive_ui::WASM_DEFAULT_MAX_FRAME_ROWS
                        ),
                    );
                }
            }
            ui.render_component(&owner, args.handle, args.frame)
                .map_err(ui_error)?;
            Ok(json!({ "ok": true }))
        }
        crate::interactive_ui::METHOD_SET_COMPONENT_HIDDEN => {
            let args: SetComponentHiddenArgs = parse_ui_args(method, args)?;
            ui.set_component_hidden(&owner, args.handle, args.hidden)
                .map_err(ui_error)?;
            Ok(json!({ "ok": true }))
        }
        crate::interactive_ui::METHOD_WAKE_COMPONENT => {
            if is_wasm_carrier(state) {
                // A wasm guest has no background thread to wake a parked poll
                // from (design §4.2/§4.4); async refresh uses `tickMs`.
                // `unknownMethod` is the R-U9.2 probe signal for absence.
                return err(
                    "unknownMethod",
                    "ui.wakeComponent: not supported by the wasm carrier (use tickMs)",
                );
            }
            let args: WakeComponentArgs = parse_ui_args(method, args)?;
            ui.wake_component(&owner, args.handle).map_err(ui_error)?;
            Ok(json!({ "ok": true }))
        }
        crate::interactive_ui::METHOD_DISPOSE_COMPONENT => {
            let args: DisposeComponentArgs = parse_ui_args(method, args)?;
            ui.dispose_component(&owner, args.handle)
                .map_err(ui_error)?;
            Ok(json!({ "ok": true }))
        }
        crate::interactive_ui::METHOD_EDIT_EXTERNAL => {
            let args: EditExternalArgs = parse_ui_args(method, args)?;
            let ui = Arc::clone(ui);
            let text = block_on(&state.async_handle, async move {
                ui.edit_external(&owner, &args.text, args.language.as_deref())
                    .await
            })?
            .map_err(ui_error)?;
            Ok(json!({ "text": text }))
        }
        // `is_interactive_ui_method` gates the caller; keep the fallback
        // fail-closed if the frozen method table ever grows.
        _ => err("unknownMethod", format!("unknown host call: {method}")),
    }
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
    // v0.1.4 C1 (ADR-0024 / V14-21): the interactive custom UI host-calls
    // route to the mode bridge. The bridge stamps the calling extension's
    // identity (`NamespacedUiBridge`) and owns the component registry; host
    // modes without an interactive UI answer `unknownMethod` (the R-U9.2
    // probe signal) **before** args are validated, so a probe never depends
    // on the method's argument shape. All seven methods are forwarded
    // (C1–C3 landed; the carrier constraint for `wakeComponent` on wasm is
    // applied above).
    if crate::interactive_ui::is_interactive_ui_method(method) {
        if !ui.supports_interactive_ui() {
            return err(
                "unknownMethod",
                format!("{method}: interactive UI ABI is not supported by this host mode"),
            );
        }
        return dispatch_interactive_ui(state, &ui, method, args);
    }
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

    use crate::api::{
        ExtensionApi, ExtensionRuntime, LoadedExtension, SetThemeResult, TerminalInputHandler,
        ThemeInfo, UiBridge, Unsubscribe,
    };
    use crate::types::{ComponentTree, ExtensionMode};
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

    /// Same host state but on the wasm (L1) carrier (V14-22 C2 tests): the
    /// dispatch target drives the carrier-specific execution constraints.
    fn host_state_wasm(capabilities: HashSet<Capability>) -> (HostState, tokio::runtime::Runtime) {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let api = ExtensionApi::for_extension(
            Arc::new(LoadedExtension::new("<inline:c0>", "<inline:c0>")),
            ExtensionRuntime::new(),
            "/test-cwd",
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        let state = HostState {
            api,
            capabilities,
            async_handle: runtime.handle().clone(),
            forward: DispatchTarget::Wasm(crate::wasm::WasmForward { tx }),
            in_command: std::cell::Cell::new(false),
            tool_updates: Default::default(),
            tool_aborts: Default::default(),
        };
        (state, runtime)
    }

    /// V14-22 FR-A/FR-C/FR-G (R-U7.2): the wasm carrier clamps the frame
    /// budget to the stricter wasm default, rejects >2000-row frames before
    /// the bridge (fail-visible) and answers `unknownMethod` for
    /// `wakeComponent` (no background thread — use `tickMs`).
    #[test]
    fn interactive_ui_wasm_carrier_limits_and_wake_constraint() {
        let bridge = Arc::new(ScriptedC1Bridge::new());
        let (mut state, _runtime) = host_state_wasm(HashSet::from([Capability::Ui]));
        state
            .api
            .runtime()
            .set_ui_bridge(Some(bridge.clone()), ExtensionMode::Tui);

        // The guest asks for 1 MiB; the wasm carrier caps it at 512 KiB.
        dispatch(
            &mut state,
            "ui.mountComponent",
            serde_json::json!({ "options": { "maxFrameBytes": 1048576 } }),
        )
        .expect("mount dispatch");
        let calls = bridge.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2["maxFrameBytes"], 524_288, "{calls:?}");

        // Rows > 2000 are rejected before the bridge; the previous frame is
        // untouched (R-U3.5 fail-visible).
        let oversize = serde_json::json!({
            "handle": 7,
            "lines": vec!["x"; crate::interactive_ui::WASM_DEFAULT_MAX_FRAME_ROWS + 1],
        });
        let (kind, message) =
            dispatch(&mut state, "ui.renderComponent", oversize).expect_err("oversize");
        assert_eq!(kind, "invalidRequest");
        assert!(
            message.contains("frameTooLarge: rows 2001 > 2000"),
            "{message}"
        );
        assert_eq!(bridge.calls().len(), 1, "oversize frame reached the bridge");

        // Exactly the carrier cap passes the gate (host registry limits are
        // exercised in the rpi crate).
        let at_limit = serde_json::json!({
            "handle": 7,
            "lines": vec!["x"; crate::interactive_ui::WASM_DEFAULT_MAX_FRAME_ROWS],
        });
        dispatch(&mut state, "ui.renderComponent", at_limit).expect("at limit");
        assert_eq!(bridge.calls().len(), 2);

        // `wakeComponent`: no background thread on wasm (design §4.4).
        let (kind, message) = dispatch(
            &mut state,
            "ui.wakeComponent",
            serde_json::json!({ "handle": 7 }),
        )
        .expect_err("wake unsupported");
        assert_eq!(kind, "unknownMethod");
        assert!(message.contains("wasm carrier"), "{message}");
        assert_eq!(bridge.calls().len(), 2, "wake reached the bridge");

        // The same mount on the native carrier keeps the full 1 MiB budget.
        let native_bridge = Arc::new(ScriptedC1Bridge::new());
        let (mut native, _native_runtime) = host_state(HashSet::from([Capability::Ui]));
        native
            .api
            .runtime()
            .set_ui_bridge(Some(native_bridge.clone()), ExtensionMode::Tui);
        dispatch(
            &mut native,
            "ui.mountComponent",
            serde_json::json!({ "options": { "maxFrameBytes": 1048576 } }),
        )
        .expect("native mount");
        assert_eq!(native_bridge.calls()[0].2["maxFrameBytes"], 1_048_576);
    }

    /// V14-20 FR-E (R-U9.2): all seven C0-frozen methods answer
    /// `unknownMethod` — the guest probe signal — without touching the UI
    /// bridge (so an unbound bridge cannot turn the probe into `stale`).
    #[test]
    fn interactive_ui_methods_answer_unknown_method_on_unbound_host() {
        let (mut state, _runtime) = host_state(HashSet::from([Capability::Ui]));
        for method in crate::interactive_ui::INTERACTIVE_UI_METHODS {
            match dispatch(&mut state, method, serde_json::json!({})) {
                Err((kind, message)) => {
                    assert_eq!(kind, "unknownMethod", "{method}");
                    assert!(
                        message.contains("not supported by this host mode"),
                        "{method}: {message}"
                    );
                }
                Ok(value) => panic!("{method} unexpectedly succeeded: {value}"),
            }
        }
    }

    /// Scripted `UiBridge` for the C1 dispatch tests: records the owner and
    /// args, answers the interactive-UI methods (C1–C3; `editExternal`
    /// resolves `None` = cancelled, `wake` is a recorded no-op).
    struct ScriptedC1Bridge {
        calls: std::sync::Mutex<Vec<(String, String, Value)>>,
    }

    impl ScriptedC1Bridge {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl UiBridge for ScriptedC1Bridge {
        async fn select(
            &self,
            _t: &str,
            _o: &[String],
            _opts: Option<UiDialogOptions>,
        ) -> Option<String> {
            None
        }
        async fn confirm(&self, _t: &str, _m: &str, _opts: Option<UiDialogOptions>) -> bool {
            false
        }
        async fn input(
            &self,
            _t: &str,
            _p: Option<&str>,
            _opts: Option<UiDialogOptions>,
        ) -> Option<String> {
            None
        }
        fn notify(&self, _m: &str, _k: NotifyType) {}
        fn on_terminal_input(&self, _h: TerminalInputHandler) -> Unsubscribe {
            Box::new(|| {})
        }
        fn set_status(&self, _k: &str, _t: Option<&str>) {}
        fn set_working_message(&self, _m: Option<&str>) {}
        fn set_working_visible(&self, _v: bool) {}
        fn set_working_indicator(&self, _o: Option<WorkingIndicatorOptions>) {}
        fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
        fn set_widget(
            &self,
            _k: &str,
            _c: Option<WidgetContent>,
            _o: Option<ExtensionWidgetOptions>,
        ) {
        }
        fn set_footer(&self, _c: Option<ComponentTree>) {}
        fn set_header(&self, _c: Option<ComponentTree>) {}
        fn set_title(&self, _t: &str) {}
        async fn custom(&self, _c: ComponentTree, _o: Option<Value>) -> Option<Value> {
            None
        }
        fn paste_to_editor(&self, _t: &str) {}
        fn set_editor_text(&self, _t: &str) {}
        fn get_editor_text(&self) -> String {
            String::new()
        }
        async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
            None
        }
        fn add_autocomplete_provider(&self, _p: Value) {}
        fn set_editor_component(&self, _c: Option<ComponentTree>) {}
        fn get_editor_component(&self) -> Option<ComponentTree> {
            None
        }
        fn theme(&self) -> Value {
            Value::Null
        }
        fn get_all_themes(&self) -> Vec<ThemeInfo> {
            Vec::new()
        }
        fn get_theme(&self, _n: &str) -> Option<Value> {
            None
        }
        fn set_theme(&self, _t: Value) -> SetThemeResult {
            SetThemeResult {
                success: false,
                error: None,
            }
        }
        fn get_tools_expanded(&self) -> bool {
            false
        }
        fn set_tools_expanded(&self, _e: bool) {}

        fn supports_interactive_ui(&self) -> bool {
            true
        }

        async fn mount_component(
            &self,
            owner: &str,
            options: crate::interactive_ui::MountOptions,
        ) -> Result<crate::interactive_ui::ComponentHandle, crate::interactive_ui::InteractiveUiError>
        {
            self.calls.lock().unwrap().push((
                "mount".to_owned(),
                owner.to_owned(),
                serde_json::to_value(options).unwrap(),
            ));
            Ok(crate::interactive_ui::ComponentHandle(7))
        }

        async fn poll_component(
            &self,
            owner: &str,
            handle: crate::interactive_ui::ComponentHandle,
        ) -> Result<crate::interactive_ui::ComponentEvent, crate::interactive_ui::InteractiveUiError>
        {
            self.calls.lock().unwrap().push((
                "poll".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "handle": handle.0 }),
            ));
            Ok(crate::interactive_ui::ComponentEvent::Tick)
        }

        fn render_component(
            &self,
            owner: &str,
            handle: crate::interactive_ui::ComponentHandle,
            frame: crate::interactive_ui::ComponentFrame,
        ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
            self.calls.lock().unwrap().push((
                "render".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "handle": handle.0, "lines": frame.lines }),
            ));
            Ok(())
        }

        fn set_component_hidden(
            &self,
            owner: &str,
            handle: crate::interactive_ui::ComponentHandle,
            hidden: bool,
        ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
            self.calls.lock().unwrap().push((
                "hidden".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "handle": handle.0, "hidden": hidden }),
            ));
            Ok(())
        }

        fn dispose_component(
            &self,
            owner: &str,
            handle: crate::interactive_ui::ComponentHandle,
        ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
            self.calls.lock().unwrap().push((
                "dispose".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "handle": handle.0 }),
            ));
            Ok(())
        }

        fn wake_component(
            &self,
            owner: &str,
            handle: crate::interactive_ui::ComponentHandle,
        ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
            self.calls.lock().unwrap().push((
                "wake".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "handle": handle.0 }),
            ));
            Ok(())
        }

        async fn edit_external(
            &self,
            owner: &str,
            text: &str,
            language: Option<&str>,
        ) -> Result<Option<String>, crate::interactive_ui::InteractiveUiError> {
            self.calls.lock().unwrap().push((
                "editExternal".to_owned(),
                owner.to_owned(),
                serde_json::json!({ "text": text, "language": language }),
            ));
            Ok(None)
        }
    }

    /// V14-21 FR-A…FR-H + V14-22/V14-23 tails: the methods parse args,
    /// stamp the extension namespace as owner and wrap results in the §2.1
    /// envelopes; `wake`/`editExternal` forward to the bridge since C2/C3;
    /// invalid args are `invalidRequest` **before** any state is created
    /// (probe path).
    #[test]
    fn component_registry_dispatch_c1_methods() {
        let bridge = Arc::new(ScriptedC1Bridge::new());
        let (mut state, _runtime) = host_state(HashSet::from([Capability::Ui]));
        state
            .api
            .runtime()
            .set_ui_bridge(Some(bridge.clone()), ExtensionMode::Tui);

        let mounted = dispatch(
            &mut state,
            "ui.mountComponent",
            serde_json::json!({ "options": { "label": "ask_user_question" } }),
        )
        .expect("mount dispatch");
        assert_eq!(mounted, serde_json::json!({ "handle": 7 }));

        let polled = dispatch(
            &mut state,
            "ui.pollComponent",
            serde_json::json!({ "handle": 7 }),
        )
        .expect("poll dispatch");
        assert_eq!(polled, serde_json::json!({ "event": { "type": "tick" } }));

        let rendered = dispatch(
            &mut state,
            "ui.renderComponent",
            serde_json::json!({ "handle": 7, "lines": ["line"] }),
        )
        .expect("render dispatch");
        assert_eq!(rendered, serde_json::json!({ "ok": true }));

        dispatch(
            &mut state,
            "ui.setComponentHidden",
            serde_json::json!({ "handle": 7, "hidden": true }),
        )
        .expect("hidden dispatch");
        dispatch(
            &mut state,
            "ui.disposeComponent",
            serde_json::json!({ "handle": 7 }),
        )
        .expect("dispose dispatch");

        // C2/C3 methods forward to the bridge with the §2.1 envelopes
        // (`wake`: {ok:true}; `editExternal`: cancelled → {text:null}).
        dispatch(
            &mut state,
            "ui.wakeComponent",
            serde_json::json!({ "handle": 7 }),
        )
        .expect("wake dispatch");
        let edited = dispatch(
            &mut state,
            "ui.editExternal",
            serde_json::json!({ "text": "draft", "language": "markdown" }),
        )
        .expect("editExternal dispatch");
        assert_eq!(edited, serde_json::json!({ "text": null }));

        // The owner stamped by the namespaced context, not the caller's.
        let calls = bridge.calls();
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[0].0, "mount");
        assert!(calls[0].1.contains("inline"), "{calls:?}");
        assert_eq!(
            calls[0].2,
            serde_json::json!({ "overlay": true, "tickMs": 0, "keysWhenHidden": [],
                                 "cursor": true, "maxFrameBytes": 1048576,
                                 "label": "ask_user_question" })
        );
        assert_eq!(
            calls[2].2,
            serde_json::json!({ "handle": 7, "lines": ["line"] })
        );
        assert_eq!(
            calls[6],
            (
                "editExternal".to_owned(),
                calls[6].1.clone(),
                serde_json::json!({ "text": "draft", "language": "markdown" })
            )
        );

        // Probe (R-U9.2): empty args fail validation before any mount.
        let (kind, _) =
            dispatch(&mut state, "ui.mountComponent", serde_json::json!({})).expect_err("probe");
        assert_eq!(kind, "invalidRequest");
        assert_eq!(bridge.calls().len(), 7, "probe must not reach the bridge");
    }

    /// V14-20 FR-E (R-U8.1): the capability gate runs before the dispatch,
    /// and the pre-existing `ui.custom` path is untouched (G2: it still
    /// reaches its own arm).
    #[test]
    fn interactive_ui_capability_gate_precedes_dispatch_and_ui_custom_is_untouched() {
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
        // exactly as before the interactive UI ABI — it is not intercepted by
        // the C1 dispatch.
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
