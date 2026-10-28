//! Command-text output (ADR-0040 / V18-02) — guest-side helper for the
//! additive `ui.emitText` host-call.
//!
//! `ui.emitText {text, validateOnly?}` is the machine-readable output face
//! for statusline configuration/help text. TUI routes it to a notification,
//! RPC and `--mode json` emit an `extension_ui_request{method:"notify"}`
//! frame, and plain print writes the text to stdout. `hasUI` is **not** the
//! gate: print/json bridges are output-capable with `hasUI=false`. NullUi /
//! subagents answer `unsupported-output` — call [`validate_only`] before any
//! fetch, edit or file write and fall back gracefully.
//!
//! `ui.emitStructured` (usage reports) is a separate V18-10 host-call; do not
//! overload this one with JSON strings.
//!
//! rpi-docs: `adr/0040-statusline-items-settings-rendering.md`,
//! `extension-abi.md` §3 (method table) / §8.4.

use serde_json::Value;

use crate::interactive_ui::{HostCall, InteractiveUiError};

/// `ui.emitText` — the additive host-call of ADR-0040.
pub const METHOD_UI_EMIT_TEXT: &str = "ui.emitText";

/// Exact message of the `call` error returned when the current bridge has no
/// output channel (NullUi / subagent).
pub const UNSUPPORTED_OUTPUT: &str = "unsupported-output";

/// Emit command text through the current mode's output channel.
///
/// `validate_only` asks the host to check the channel without producing any
/// output. Errors: `call`/`unsupported-output` when the mode has no output
/// channel, `unknownMethod` on hosts without the method, `capabilityDenied`
/// without capability `ui`.
pub fn emit_text<H: HostCall + ?Sized>(
    host: &H,
    text: &str,
    validate_only: bool,
) -> Result<(), InteractiveUiError> {
    let mut args = serde_json::Map::new();
    args.insert("text".to_owned(), Value::String(text.to_owned()));
    if validate_only {
        args.insert("validateOnly".to_owned(), Value::Bool(true));
    }
    host.call(METHOD_UI_EMIT_TEXT, Value::Object(args))?;
    Ok(())
}

/// Validate the output channel without emitting anything.
///
/// Prefer this at the start of a command handler when the handler would
/// otherwise fetch data, open an editor or write files.
pub fn validate_only<H: HostCall + ?Sized>(host: &H) -> Result<(), InteractiveUiError> {
    emit_text(host, "", true)
}

/// Whether `error` is the host's `unsupported-output` refusal.
pub fn is_unsupported_output(error: &InteractiveUiError) -> bool {
    error.message == UNSUPPORTED_OUTPUT
}

/// Probe whether the host implements `ui.emitText`.
///
/// Uses `validateOnly` so a supporting host produces no output. `Ok(false)`
/// means an old host answered `unknownMethod`; a host with no output channel
/// answers `unsupported-output` and is a real error here (the caller must
/// handle the channel, not the method, as missing).
pub fn supports_emit_text<H: HostCall + ?Sized>(host: &H) -> Result<bool, InteractiveUiError> {
    match validate_only(host) {
        Ok(()) => Ok(true),
        Err(error) => match error.kind {
            crate::interactive_ui::InteractiveUiErrorKind::UnknownMethod => Ok(false),
            _ => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive_ui::{InteractiveUiError, InteractiveUiErrorKind};
    use serde_json::json;
    use std::cell::RefCell;

    struct RecordingHost {
        calls: RefCell<Vec<(String, Value)>>,
        reply: Result<Value, InteractiveUiError>,
    }

    impl HostCall for RecordingHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
            self.calls
                .borrow_mut()
                .push((method.to_owned(), args.clone()));
            self.reply.clone()
        }
    }

    fn host(reply: Result<Value, InteractiveUiError>) -> RecordingHost {
        RecordingHost {
            calls: RefCell::new(Vec::new()),
            reply,
        }
    }

    #[test]
    fn sends_text_and_validate_only() {
        let host = host(Ok(Value::Null));
        emit_text(&host, "hello", false).expect("emit");
        validate_only(&host).expect("validate");
        let calls = host.calls.borrow();
        assert_eq!(calls[0].0, METHOD_UI_EMIT_TEXT);
        assert_eq!(calls[0].1, json!({"text": "hello"}));
        assert_eq!(calls[1].1, json!({"text": "", "validateOnly": true}));
    }

    #[test]
    fn unsupported_output_is_recognized() {
        let host = host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::Call,
            UNSUPPORTED_OUTPUT,
        )));
        let error = validate_only(&host).expect_err("no channel");
        assert!(is_unsupported_output(&error));
        assert_eq!(error.kind, InteractiveUiErrorKind::Call);
    }

    #[test]
    fn old_host_reports_unknown_method() {
        let host = host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "unknown host call: ui.emitText",
        )));
        assert!(!supports_emit_text(&host).expect("probe"));
    }

    #[test]
    fn supporting_host_probes_without_output() {
        let host = host(Ok(Value::Null));
        assert!(supports_emit_text(&host).expect("probe"));
        assert_eq!(
            host.calls.borrow()[0].1,
            json!({"text": "", "validateOnly": true})
        );
    }
}
