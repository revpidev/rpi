//! `RpcUiBridge` — RPC-mode extension UI bridge (T15 W4), port of
//! `createExtensionUIContext` (modes/rpc/rpc-mode.ts:126-309).
//!
//! Dialog methods (`select`/`confirm`/`input`/`editor`) emit an
//! `extension_ui_request` frame on stdout and block on the client's
//! `extension_ui_response` (routed via the shared pending table);
//! fire-and-forget methods (`notify`/`setStatus`/`setWidget`/`setTitle`/
//! `set_editor_text`) emit and return. The 18 degraded methods follow
//! rpc-mode.ts:162-309 exactly (see `EXTENSION_UI_DEGRADED_METHODS`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use rpi_ext_host::types::ComponentTree;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

/// Shared pending-dialog table (`pendingExtensionRequests`,
/// rpc-mode.ts:88-128): stdin `extension_ui_response` frames resolve these.
pub type PendingUiTable = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

pub fn new_pending_ui_table() -> PendingUiTable {
    Arc::new(Mutex::new(HashMap::new()))
}

/// RPC extension UI context. Cheap to clone (channel + shared table).
pub struct RpcUiBridge {
    output: mpsc::UnboundedSender<String>,
    pending_ui: PendingUiTable,
    /// Default theme JSON for the `theme` getter (upstream returns the
    /// statically imported default theme, rpc-mode.ts:283-285).
    default_theme: Value,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl RpcUiBridge {
    pub fn new(
        output: mpsc::UnboundedSender<String>,
        pending_ui: PendingUiTable,
        default_theme: Value,
    ) -> Self {
        RpcUiBridge {
            output,
            pending_ui,
            default_theme,
        }
    }

    fn emit(&self, frame: Value) {
        let mut line = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_owned());
        line.push('\n');
        let _ = self.output.send(line);
    }

    /// `createDialogPromise` (rpc-mode.ts:90-129): register the pending
    /// entry, emit the request, await the response; an optional timeout
    /// auto-resolves with the method's default and drops the pending entry.
    async fn dialog(&self, method_fields: Value, timeout_ms: Option<u64>) -> Option<Value> {
        let id = rpi_ai::utils::uuid::random_uuid();
        let (tx, rx) = oneshot::channel();
        lock(&self.pending_ui).insert(id.clone(), tx);

        let mut frame = json!({"type": "extension_ui_request", "id": id});
        if let (Some(map), Some(fields)) = (frame.as_object_mut(), method_fields.as_object()) {
            for (key, value) in fields {
                map.insert(key.clone(), value.clone());
            }
            if let Some(timeout) = timeout_ms {
                map.insert("timeout".to_owned(), json!(timeout));
            }
        }
        self.emit(frame);

        let response = match timeout_ms {
            Some(ms) if ms > 0 => {
                match tokio::time::timeout(std::time::Duration::from_millis(ms), rx).await {
                    Ok(received) => received.ok(),
                    // Timeout: auto-resolve with the default; the pending
                    // entry must go (rpc-mode.ts:101-108 cleanup).
                    Err(_) => {
                        lock(&self.pending_ui).remove(&id);
                        None
                    }
                }
            }
            _ => rx.await.ok(),
        };
        response
    }
}

/// Response mapping (`parseResponse`, rpc-mode.ts:136-151): `cancelled`
/// wins, then the value/confirmed field, else the default.
fn value_response(response: Option<Value>) -> Option<String> {
    let response = response?;
    if response.get("cancelled").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    response
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn confirm_response(response: Option<Value>) -> bool {
    let Some(response) = response else {
        return false;
    };
    if response.get("cancelled").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    response
        .get("confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[async_trait]
impl UiBridge for RpcUiBridge {
    async fn select(
        &self,
        title: &str,
        options: &[String],
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        let response = self
            .dialog(
                json!({"method": "select", "title": title, "options": options}),
                opts.and_then(|o| o.timeout),
            )
            .await;
        value_response(response)
    }

    async fn confirm(&self, title: &str, message: &str, opts: Option<UiDialogOptions>) -> bool {
        let response = self
            .dialog(
                json!({"method": "confirm", "title": title, "message": message}),
                opts.and_then(|o| o.timeout),
            )
            .await;
        confirm_response(response)
    }

    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        let response = self
            .dialog(
                json!({"method": "input", "title": title, "placeholder": placeholder}),
                opts.and_then(|o| o.timeout),
            )
            .await;
        value_response(response)
    }

    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String> {
        // `editor` carries no timeout upstream (rpc-mode.ts:238-262).
        let response = self
            .dialog(
                json!({"method": "editor", "title": title, "prefill": prefill}),
                None,
            )
            .await;
        value_response(response)
    }

    fn notify(&self, message: &str, kind: NotifyType) {
        self.emit(json!({
            "type": "extension_ui_request",
            "id": rpi_ai::utils::uuid::random_uuid(),
            "method": "notify",
            "message": message,
            "notifyType": match kind {
                NotifyType::Info => "info",
                NotifyType::Warning => "warning",
                NotifyType::Error => "error",
            },
        }));
    }

    /// Degraded: raw terminal input is not supported (rpc-mode.ts:165-167).
    fn on_terminal_input(&self, _handler: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        self.emit(json!({
            "type": "extension_ui_request",
            "id": rpi_ai::utils::uuid::random_uuid(),
            "method": "setStatus",
            "statusKey": key,
            "statusText": text,
        }));
    }

    /// Degraded no-op (rpc-mode.ts:172-174).
    fn set_working_message(&self, _message: Option<&str>) {}
    /// Degraded no-op (rpc-mode.ts:176-178).
    fn set_working_visible(&self, _visible: bool) {}
    /// Degraded no-op (rpc-mode.ts:180-182).
    fn set_working_indicator(&self, _options: Option<WorkingIndicatorOptions>) {}
    /// Degraded no-op (rpc-mode.ts:184-186).
    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}

    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: Option<ExtensionWidgetOptions>,
    ) {
        // Only string arrays are supported; component descriptors are
        // ignored (no frame at all, rpc-mode.ts:188-204).
        let lines = match &content {
            None => None,
            Some(WidgetContent::Lines(lines)) => Some(lines.clone()),
            Some(WidgetContent::Component(_)) => return,
        };
        self.emit(json!({
            "type": "extension_ui_request",
            "id": rpi_ai::utils::uuid::random_uuid(),
            "method": "setWidget",
            "widgetKey": key,
            "widgetLines": lines,
            "widgetPlacement": options.and_then(|o| o.placement).map(|p| match p {
                rpi_ext_host::api::WidgetPlacement::AboveEditor => "aboveEditor",
                rpi_ext_host::api::WidgetPlacement::BelowEditor => "belowEditor",
            }),
        }));
    }

    /// Degraded no-op (rpc-mode.ts:206-208).
    fn set_footer(&self, _component: Option<ComponentTree>) {}
    /// Degraded no-op (rpc-mode.ts:210-212).
    fn set_header(&self, _component: Option<ComponentTree>) {}

    fn set_title(&self, title: &str) {
        self.emit(json!({
            "type": "extension_ui_request",
            "id": rpi_ai::utils::uuid::random_uuid(),
            "method": "setTitle",
            "title": title,
        }));
    }

    /// Degraded: `custom` returns `undefined` (rpc-mode.ts:218-220).
    async fn custom(&self, _component: ComponentTree, _options: Option<Value>) -> Option<Value> {
        None
    }

    /// Degraded: delegates to `setEditorText` (rpc-mode.ts:222-225).
    fn paste_to_editor(&self, text: &str) {
        self.set_editor_text(text);
    }

    fn set_editor_text(&self, text: &str) {
        self.emit(json!({
            "type": "extension_ui_request",
            "id": rpi_ai::utils::uuid::random_uuid(),
            "method": "set_editor_text",
            "text": text,
        }));
    }

    /// Degraded: always "" (rpc-mode.ts:233-237).
    fn get_editor_text(&self) -> String {
        String::new()
    }

    /// Degraded no-op (rpc-mode.ts:264-266).
    fn add_autocomplete_provider(&self, _provider: Value) {}
    /// Degraded no-op (rpc-mode.ts:268-270).
    fn set_editor_component(&self, _component: Option<ComponentTree>) {}

    /// Degraded: `undefined` (rpc-mode.ts:272-275).
    fn get_editor_component(&self) -> Option<ComponentTree> {
        None
    }

    fn theme(&self) -> Value {
        self.default_theme.clone()
    }

    /// Degraded: [] (rpc-mode.ts:287-289).
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }

    /// Degraded: `undefined` (rpc-mode.ts:291-293).
    fn get_theme(&self, _name: &str) -> Option<Value> {
        None
    }

    /// Degraded: `{success: false, error}` (rpc-mode.ts:295-298).
    fn set_theme(&self, _theme: Value) -> SetThemeResult {
        SetThemeResult {
            success: false,
            error: Some("Theme switching not supported in RPC mode".to_owned()),
        }
    }

    /// Degraded: false (rpc-mode.ts:300-302).
    fn get_tools_expanded(&self) -> bool {
        false
    }

    /// Degraded no-op (rpc-mode.ts:304-306).
    fn set_tools_expanded(&self, _expanded: bool) {}
}
