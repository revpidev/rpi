//! ADR-0043: host side of the inbound questionnaire answer channel.
//!
//! Extensions (or a host program) emit `rpi:ask-user:answer` on the extension
//! event bus to answer or cancel an open `ask_user_question` questionnaire
//! without synthesizing keys. The TUI host routes a payload to the active
//! answerable component by `toolCallId` and lets the guest validate it; hosts
//! without the interactive component path (RPC/print) answer every attempt
//! with `unsupported_host` on `rpi:ask-user:answer-result`.

use std::sync::Weak;

use rpi_ext_host::api::EventBus;
use serde_json::{Value, json};

use super::InteractiveUi;

/// Inbound event channel (upstream `rpiv:ask-user:answer`).
pub const ANSWER_EVENT: &str = "rpi:ask-user:answer";
/// Verdict channel (upstream `rpiv:ask-user:answer-result`).
pub const ANSWER_RESULT_EVENT: &str = "rpi:ask-user:answer-result";

/// Routing outcome for one inbound payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Delivered,
    NotFound,
    InvalidPayload,
}

/// Emit one verdict (`{ok, code?, reason?, requestId?, toolCallId?}`).
pub(crate) fn emit_verdict(
    bus: &EventBus,
    payload: &Value,
    code: Option<&str>,
    reason: Option<&str>,
) {
    let request_id = payload.get("requestId").and_then(Value::as_str);
    let tool_call_id = payload.get("toolCallId").and_then(Value::as_str);
    let verdict = json!({
        "ok": code.is_none(),
        "code": code,
        "reason": reason,
        "requestId": request_id,
        "toolCallId": tool_call_id,
    });
    bus.emit(ANSWER_RESULT_EVENT, verdict);
}

/// Install the TUI routing handler on `bus`; the weak UI keeps the handler
/// inert after teardown.
pub(crate) fn install_tui(bus: &EventBus, ui: Weak<InteractiveUi>) {
    let bus_for_handler = bus.clone();
    let unsubscribe = bus.on(
        ANSWER_EVENT,
        std::sync::Arc::new(move |payload: Value| {
            let Some(ui) = ui.upgrade() else {
                return;
            };
            match ui.component_registry.deliver_external_answer(&payload) {
                Delivery::Delivered => {}
                Delivery::NotFound => emit_verdict(
                    &bus_for_handler,
                    &payload,
                    Some("not_found"),
                    Some("no open questionnaire for that tool call"),
                ),
                Delivery::InvalidPayload => emit_verdict(
                    &bus_for_handler,
                    &payload,
                    Some("invalid_payload"),
                    Some("payload must be an object with a string toolCallId"),
                ),
            }
        }),
    );
    // The bus lives as long as the extension runtime; dropping the closure is
    // fine because the handler holds a weak UI.
    let _ = unsubscribe;
}

/// Install the non-interactive responder: every attempt is unsupported.
pub(crate) fn install_unsupported(bus: &EventBus) {
    let bus_for_handler = bus.clone();
    let unsubscribe = bus.on(
        ANSWER_EVENT,
        std::sync::Arc::new(move |payload: Value| {
            emit_verdict(
                &bus_for_handler,
                &payload,
                Some("unsupported_host"),
                Some("this host shows the questions in its own dialogs"),
            );
        }),
    );
    let _ = unsubscribe;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn capture(bus: &EventBus) -> Arc<Mutex<Vec<Value>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let _ = bus.on(
            ANSWER_RESULT_EVENT,
            Arc::new(move |verdict: Value| sink.lock().unwrap().push(verdict)),
        );
        seen
    }

    #[test]
    fn unsupported_host_answers_every_attempt_with_a_code() {
        let bus = EventBus::new();
        let seen = capture(&bus);
        install_unsupported(&bus);
        bus.emit(
            ANSWER_EVENT,
            serde_json::json!({"toolCallId": "call-1", "requestId": "req-1"}),
        );
        let verdicts = seen.lock().unwrap();
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0]["ok"], serde_json::json!(false));
        assert_eq!(verdicts[0]["code"], serde_json::json!("unsupported_host"));
        assert_eq!(verdicts[0]["requestId"], serde_json::json!("req-1"));
        assert_eq!(verdicts[0]["toolCallId"], serde_json::json!("call-1"));
    }

    #[test]
    fn verdict_without_a_code_is_ok_and_echoes_string_fields_only() {
        let bus = EventBus::new();
        let seen = capture(&bus);
        emit_verdict(&bus, &serde_json::json!({"requestId": 3}), None, None);
        let verdicts = seen.lock().unwrap();
        assert_eq!(verdicts[0]["ok"], serde_json::json!(true));
        assert_eq!(verdicts[0]["code"], Value::Null);
        assert_eq!(verdicts[0]["requestId"], Value::Null);
    }
}
