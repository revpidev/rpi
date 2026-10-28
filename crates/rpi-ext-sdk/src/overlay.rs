//! `ui.hasOverlay` (ADR-0041 / V18-02) — guest-side query for the current
//! session's visible overlay state.
//!
//! Returns `{visible, capturing}`:
//! - `visible`: any visible overlay (capturing or not) owned by the host or
//!   any extension in the current session; hidden and inline-only components
//!   are excluded.
//! - `capturing`: visible capturing overlays only. `capturing == true`
//!   implies `visible == true`.
//!
//! The object is always truthy; callers must read the fields. RPC/NullUi and
//! other non-interactive bridges answer `{visible:false,capturing:false}`;
//! pre-V18-02 hosts answer `unknownMethod` — probe with
//! [`supports_has_overlay`].
//!
//! Typical consumer: an inline questionnaire that must yield its editor slot
//! only when a capturing overlay (for example a modal ask dialog) is open;
//! non-capturing HUDs are ignored on purpose.
//!
//! rpi-docs: `adr/0041-additive-ui-has-overlay.md`, `extension-abi.md` §3 /
//! §8.4.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::interactive_ui::{HostCall, InteractiveUiError};

/// `ui.hasOverlay` — the additive host-call of ADR-0041.
pub const METHOD_UI_HAS_OVERLAY: &str = "ui.hasOverlay";

/// `{visible, capturing}` (camelCase on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HasOverlayState {
    pub visible: bool,
    pub capturing: bool,
}

impl HasOverlayState {
    /// Whether a visible capturing overlay is open (the ask-user-question
    /// inline/overlay fallback signal).
    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// Whether any visible overlay is open (capturing or not).
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

/// Query the current overlay state.
///
/// `capabilityDenied` without `ui`; `unknownMethod` on pre-V18-02 hosts.
pub fn has_overlay<H: HostCall + ?Sized>(host: &H) -> Result<HasOverlayState, InteractiveUiError> {
    let response = host.call(METHOD_UI_HAS_OVERLAY, Value::Object(Default::default()))?;
    serde_json::from_value(response)
        .map_err(|error| InteractiveUiError::protocol(format!("hasOverlay reply: {error}")))
}

/// Probe whether the host implements `ui.hasOverlay`.
pub fn supports_has_overlay<H: HostCall + ?Sized>(host: &H) -> Result<bool, InteractiveUiError> {
    match host.call(METHOD_UI_HAS_OVERLAY, Value::Object(Default::default())) {
        Ok(_) => Ok(true),
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
    fn parses_both_fields_and_uses_empty_args() {
        let host = host(Ok(json!({"visible": true, "capturing": false})));
        let state = has_overlay(&host).expect("state");
        assert!(state.is_visible());
        assert!(!state.is_capturing());
        let calls = host.calls.borrow();
        assert_eq!(calls[0].0, METHOD_UI_HAS_OVERLAY);
        assert_eq!(calls[0].1, json!({}));
    }

    #[test]
    fn capturing_implies_visible_on_the_wire_shape() {
        let host = host(Ok(json!({"visible": true, "capturing": true})));
        let state = has_overlay(&host).expect("state");
        assert!(state.capturing, "capturing true");
        assert!(state.visible, "capturing implies visible");
    }

    #[test]
    fn non_interactive_bridge_answers_double_false() {
        let host = host(Ok(json!({"visible": false, "capturing": false})));
        let state = has_overlay(&host).expect("state");
        assert!(!state.visible);
        assert!(!state.capturing);
        assert!(supports_has_overlay(&host).expect("probe"));
    }

    #[test]
    fn old_host_is_the_unknown_method_signal() {
        let host = host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "unknown host call: ui.hasOverlay",
        )));
        assert!(!supports_has_overlay(&host).expect("probe"));
        let error = has_overlay(&host).expect_err("old host");
        assert_eq!(error.kind, InteractiveUiErrorKind::UnknownMethod);
    }
}
