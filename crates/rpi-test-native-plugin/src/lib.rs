//! Test fixture native plugin (T15 W7): an abi_stable cdylib implementing
//! the same permission-gate behavior as the WAT wasm guest — `on
//! tool_call` blocks with reason "native-gate".
//!
//! TE01 (ADR-0015): a `te01_probe` tool_call arm (a tool name that is never
//! registered) exercises the additive `unregisterTool` / `toolUpdate`
//! methods over the L0 carrier; the gate behavior for real tool names is
//! unchanged.

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::interactive_ui::{
    run_component, AnsiColor, AnsiStyle, Component, ComponentCursor, DisposeReason,
    InteractiveUiError, LineBuilder, MountOptions, NativeHostCall, CURSOR_MARKER,
};
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

/// The host-call handle stashed at init so dispatch handlers can call back
/// (TE01 probe). `extern "C" fn` pointers are Send + Sync.
type HostCallFn = extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>;

static HOST_CALL: std::sync::Mutex<Option<HostCallFn>> = std::sync::Mutex::new(None);

fn host_call(calls: &RpiHostCalls, cookie: PluginCookie, method: &str, args: Value) -> Value {
    call_with(calls.call, cookie, method, args)
}

fn call_with(call: HostCallFn, cookie: PluginCookie, method: &str, args: Value) -> Value {
    let request = serde_json::to_vec(&json!({
        "call": method,
        "args": args,
        "seq": 0,
    }))
    .unwrap_or_default();
    let response = call(cookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

/// Host call from a dispatch handler (uses the stashed init-time handle).
fn dispatch_host_call(cookie: PluginCookie, method: &str, args: Value) -> Value {
    let call = HOST_CALL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .to_owned();
    match call {
        Some(call) => call_with(call, cookie, method, args),
        None => json!({"error": {"kind": "internal", "message": "host calls not stashed"}}),
    }
}

fn pack(value: Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()))
}

/// `{"ok": ...}` payload, or the error JSON verbatim.
fn ok_value(response: &Value) -> Value {
    response
        .get("ok")
        .cloned()
        .unwrap_or_else(|| response.clone())
}

/// TE01 probe (ADR-0015): register → unregister → unregister again →
/// unregister unknown → re-register, then register an update-reporting tool.
/// Returns the three unregister results so the host test can assert the
/// boundary semantics over the L0 carrier.
fn te01_probe(cookie: PluginCookie) -> Value {
    let definition = json!({
        "name": "te01_native_tool",
        "label": "TE01 Native Tool",
        "description": "te01 probe tool",
        "parameters": {"type": "object"},
    });
    let registered = dispatch_host_call(cookie, "registerTool", json!({"definition": definition}));
    if let Some(error) = registered.get("error") {
        return json!({"error": error.clone()});
    }
    let first = dispatch_host_call(
        cookie,
        "unregisterTool",
        json!({"name": "te01_native_tool"}),
    );
    let repeat = dispatch_host_call(
        cookie,
        "unregisterTool",
        json!({"name": "te01_native_tool"}),
    );
    let unknown = dispatch_host_call(cookie, "unregisterTool", json!({"name": "te01_never"}));
    let re_registered =
        dispatch_host_call(cookie, "registerTool", json!({"definition": definition}));
    let updater = dispatch_host_call(
        cookie,
        "registerTool",
        json!({"definition": {
            "name": "te01_native_updater",
            "label": "TE01 Native Updater",
            "description": "te01 on_update probe tool",
            "parameters": {"type": "object"},
        }}),
    );
    json!({
        "unregisterFirst": ok_value(&first),
        "unregisterRepeat": ok_value(&repeat),
        "unregisterUnknown": ok_value(&unknown),
        "reRegistered": re_registered.get("error").is_none(),
        "updaterRegistered": updater.get("error").is_none(),
    })
}

/// `toolExecute` for `te01_native_updater`: report one partial result via
/// `toolUpdate`, then return the final result (ADR-0015).
fn te01_execute_updater(cookie: PluginCookie, message: &Value) -> Value {
    let tool_call_id = message
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let update = dispatch_host_call(
        cookie,
        "toolUpdate",
        json!({
            "toolCallId": tool_call_id,
            "update": {
                "content": [{"type": "text", "text": "native-partial"}],
                "details": null,
            },
        }),
    );
    json!({
        "content": [{"type": "text", "text": "native-final"}],
        "details": {"updateAccepted": update.get("error").is_none()},
    })
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    *HOST_CALL.lock().unwrap_or_else(|e| e.into_inner()) = Some(calls.call);
    let response = host_call(&calls, cookie, "on", json!({"event": "tool_call"}));
    if response.get("error").is_some() {
        return pack(response);
    }
    // V14-22 dual-carrier parity fixture: register the scripted interactive
    // UI tool. Registration is additive and must not fail the load when the
    // `tools` capability is absent (bare `.so` loads keep working).
    let fixture = host_call(
        &calls,
        cookie,
        "registerTool",
        json!({"definition": {
            "name": "interactive_ui_fixture",
            "label": "Interactive UI Parity Fixture",
            "description": "Scripted interactive component driven by the parity corpus",
            "parameters": {"type": "object"},
        }}),
    );
    if let Some(_error) = fixture.get("error") {
        // Without `tools` the fixture tool is unavailable; the parity harness
        // always grants it and fails loudly when the definition is missing.
    }
    pack(json!({"ok": true}))
}

pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = message.get("payload").cloned().unwrap_or(Value::Null);
    // TE01 probe (ADR-0015): triggered by a tool_call event for the
    // (never-registered) tool name "te01_probe", so the probe result rides
    // the emit_tool_call return channel. Other tool_calls gate as before.
    if kind == "event"
        && message.get("event").and_then(Value::as_str) == Some("tool_call")
        && payload.get("toolName").and_then(Value::as_str) == Some("te01_probe")
    {
        return pack(te01_probe(_cookie));
    }
    if kind == "event" && message.get("event").and_then(Value::as_str) == Some("tool_call") {
        return pack(json!({"block": true, "reason": "native-gate"}));
    }
    if kind == "toolExecute"
        && message.get("toolName").and_then(Value::as_str) == Some("te01_native_updater")
    {
        return pack(te01_execute_updater(_cookie, &message));
    }
    // V14-22: scripted interactive-UI component (dual-carrier parity).
    if kind == "toolExecute"
        && message.get("toolName").and_then(Value::as_str) == Some("interactive_ui_fixture")
    {
        return pack(execute_interactive_ui_fixture(_cookie));
    }
    pack(Value::Null)
}

// ============================================================================
// V14-22 C2: scripted interactive component (native carrier)
// ============================================================================
//
// The identical state machine is implemented in the wasm fixture
// (`examples/wasm-extension`); the parity harness drives both with one
// corpus and diffs the submitted frames byte-for-byte (R-U7.4). Keep the
// two render rules in lockstep.

/// Deterministic state driven only by protocol events (no host calls, so
/// the corpus is carrier-agnostic).
#[derive(Default)]
struct ScriptedComponent {
    events: usize,
    inputs: Vec<String>,
    ticks: usize,
    wakes: usize,
    height: usize,
    hidden: bool,
    focused: bool,
    theme: String,
    quit: bool,
    disposed: Option<String>,
}

impl Component for ScriptedComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut header = LineBuilder::new();
        header.push(format!("ev={}", self.events), AnsiStyle::new().bold());
        header.plain(format!(
            " in={} tick={} wake={}",
            self.inputs.len(),
            self.ticks,
            self.wakes
        ));
        let last = self
            .inputs
            .last()
            .cloned()
            .unwrap_or_else(|| "-".to_owned());
        let mut state = LineBuilder::new();
        state.push(
            format!("last={last}"),
            AnsiStyle::new().fg(AnsiColor::Basic(6)),
        );
        state.plain(format!(
            " w={} h={} hidden={} focus={} theme={}",
            width, self.height, self.hidden, self.focused, self.theme
        ));
        // `c` toggles the embedded cursor marker; otherwise a wide-char line
        // (glyph handling is the host's job — the guest emits both verbatim).
        let third = if self.inputs.iter().any(|input| input == "c") {
            format!("cur{CURSOR_MARKER}sor")
        } else {
            "wide 漢字 tail".to_owned()
        };
        vec![header.build(), state.build(), third]
    }

    fn handle_input(&mut self, data: &str) {
        self.events += 1;
        self.inputs.push(data.to_owned());
        if data == "q" {
            self.quit = true;
        }
    }

    fn on_focus(&mut self) {
        self.events += 1;
        self.focused = true;
    }

    fn on_blur(&mut self) {
        self.events += 1;
        self.focused = false;
    }

    fn on_tick(&mut self) {
        self.events += 1;
        self.ticks += 1;
    }

    fn on_resize(&mut self, _width: usize, height: usize) {
        self.events += 1;
        self.height = height;
    }

    fn on_theme(&mut self, theme: &Value) {
        self.events += 1;
        self.theme = theme
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_owned();
    }

    fn on_visibility(&mut self, hidden: bool) {
        self.events += 1;
        self.hidden = hidden;
    }

    fn on_render(&mut self) {
        self.events += 1;
        self.wakes += 1;
    }

    fn on_dispose(&mut self, reason: DisposeReason) {
        self.events += 1;
        self.disposed = Some(reason.as_str().to_owned());
    }

    fn cursor(&self) -> Option<ComponentCursor> {
        // Explicit-cursor branch of the frame triple: active while the last
        // input is the `c` toggle (the other branch embeds CURSOR_MARKER).
        (self.inputs.last().map(String::as_str) == Some("c")).then_some(ComponentCursor {
            row: 2,
            col: self.inputs.len(),
        })
    }

    fn done(&mut self) -> Option<Value> {
        self.quit.then(|| {
            json!({
                "events": self.events,
                "inputs": self.inputs,
                "ticks": self.ticks,
                "wakes": self.wakes,
                "hidden": self.hidden,
                "disposed": self.disposed,
            })
        })
    }
}

/// `toolExecute` for `interactive_ui_fixture`: mount the scripted component
/// and return its terminal value as the tool result.
fn execute_interactive_ui_fixture(cookie: PluginCookie) -> Value {
    let call = HOST_CALL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .to_owned();
    let Some(call) = call else {
        return json!({"error": {"kind": "internal", "message": "host calls not stashed"}});
    };
    let transport = NativeHostCall::new(call, cookie);
    let options = MountOptions {
        label: Some("parity".to_owned()),
        tick_ms: 250,
        keys_when_hidden: vec!["ctrl+g".to_owned()],
        ..MountOptions::default()
    };
    tool_result(run_component(
        &transport,
        ScriptedComponent::default(),
        options,
    ))
}

/// Shared tool-result envelope for the fixture (both carriers).
fn tool_result(result: Result<Value, InteractiveUiError>) -> Value {
    match result {
        Ok(done) => json!({
            "content": [{"type": "text", "text": serde_json::to_string(&done).unwrap_or_default()}],
            "details": {"terminal": done},
        }),
        Err(error) => json!({
            "content": [{"type": "text", "text": error.message.clone()}],
            "details": {"error": {"kind": error.kind.as_str(), "message": error.message}},
        }),
    }
}

/// The root module export (abi_stable).
#[abi_stable::export_root_module]
pub fn module() -> RpiNativeModule_Ref {
    RpiNativeModule {
        rpi_extension_init: init,
        rpi_dispatch: dispatch,
    }
    .leak_into_prefix()
}
