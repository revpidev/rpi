//! Permission-gate example extension (wasm guest, ABI v1).
//!
//! Behavior (matches the native inline variant used in
//! `crates/rpi/tests/extension_host_w6_test.rs`):
//! - blocks the `read` tool with a reason (`tool_call` handler),
//! - registers a custom `gate_tool` that returns fixed output,
//! - registers `interactive_ui_fixture` (V14-22 C2): a scripted interactive
//!   component driven by the dual-carrier parity corpus. The identical
//!   state machine lives in `crates/rpi-test-native-plugin`; keep the two
//!   render rules in lockstep (R-U7.4 diffs them byte-for-byte).

use rpi_ext_sdk::interactive_ui::{
    run_component, AnsiColor, AnsiStyle, Component, ComponentCursor, DisposeReason, LineBuilder,
    MountOptions, RpiHost, CURSOR_MARKER,
};
use rpi_ext_sdk::{export, Extension};
use serde_json::{json, Value};

fn register(ext: &mut Extension) {
    ext.on("tool_call", |payload| {
        if payload["toolName"].as_str() == Some("read") {
            Ok(json!({"block": true, "reason": "gate-block"}))
        } else {
            Ok(Value::Null)
        }
    });
    ext.tool(
        json!({
            "name": "gate_tool",
            "label": "Gate Tool",
            "description": "parity tool",
            "parameters": {"type": "object"},
        }),
        |_params| {
            Ok(json!({
                "content": [{"type": "text", "text": "gate-output"}],
                "details": null,
            }))
        },
    );
    ext.tool(
        json!({
            "name": "interactive_ui_fixture",
            "label": "Interactive UI Parity Fixture",
            "description": "Scripted interactive component driven by the parity corpus",
            "parameters": {"type": "object"},
        }),
        |_params| {
            let options = MountOptions {
                label: Some("parity".to_owned()),
                tick_ms: 250,
                keys_when_hidden: vec!["ctrl+g".to_owned()],
                ..MountOptions::default()
            };
            let result = run_component(&RpiHost, ScriptedComponent::default(), options);
            Ok(match result {
                Ok(done) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string(&done).unwrap_or_default()}],
                    "details": {"terminal": done},
                }),
                Err(error) => json!({
                    "content": [{"type": "text", "text": error.message.clone()}],
                    "details": {"error": {"kind": error.kind.as_str(), "message": error.message}},
                }),
            })
        },
    );
}

// ============================================================================
// V14-22 C2: scripted interactive component (wasm carrier)
// ============================================================================
//
// Mirror of `crates/rpi-test-native-plugin`'s `ScriptedComponent`: the
// component reacts only to protocol events (no guest-driven host calls), so
// one corpus drives both carriers. The parity harness diffs lines/cursor/
// done per frame and the terminal value.

/// Deterministic state driven only by protocol events.
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

export!(register);
