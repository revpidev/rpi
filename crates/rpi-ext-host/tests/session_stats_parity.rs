//! V18-02 (ADR-0040): the additive `ctx.sessionStats` and `ui.emitText`
//! mirrors must stay byte-compatible between the host and the wasm SDK.
//!
//! Precedent: `session_entries_parity.rs` (ADR-0027) / ADR-0013's additive
//! ABI rule. The native types live in `rpi-ext-host`, the guest types in
//! `rpi-ext-sdk`; both serialize the same camelCase shape with
//! `sessionFile` omitted for in-memory sessions and no `tokens.total`.

use rpi_ext_host::api::UiOutputError;
use rpi_ext_host::types::{SessionStatsSnapshot, SessionTokenStats};
use rpi_ext_sdk::output::{METHOD_UI_EMIT_TEXT, UNSUPPORTED_OUTPUT, is_unsupported_output};
use rpi_ext_sdk::overlay::{METHOD_UI_HAS_OVERLAY, has_overlay};
use rpi_ext_sdk::session_stats::{METHOD_SESSION_STATS, SessionStats};

fn native_snapshot() -> SessionStatsSnapshot {
    SessionStatsSnapshot {
        session_id: "s1".to_owned(),
        session_file: Some("/tmp/s1.jsonl".to_owned()),
        started_at: "2026-10-27T10:00:00Z".to_owned(),
        entry_count: 12,
        tokens: SessionTokenStats {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write: 40,
        },
        cost: 1.25,
    }
}

fn sdk_snapshot() -> SessionStats {
    SessionStats {
        session_id: "s1".to_owned(),
        session_file: Some("/tmp/s1.jsonl".to_owned()),
        started_at: "2026-10-27T10:00:00Z".to_owned(),
        entry_count: 12,
        tokens: rpi_ext_sdk::session_stats::SessionTokenStats {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write: 40,
        },
        cost: 1.25,
    }
}

#[test]
fn session_stats_wire_shape_is_identical() {
    let native = serde_json::to_value(native_snapshot()).expect("native json");
    let sdk = serde_json::to_value(sdk_snapshot()).expect("sdk json");
    assert_eq!(native, sdk);
    assert_eq!(native["tokens"]["cacheRead"], 30);
    assert_eq!(native["tokens"]["cacheWrite"], 40);
    assert!(native["tokens"].get("total").is_none());
    assert_eq!(native["startedAt"], "2026-10-27T10:00:00Z");
    assert_eq!(native["entryCount"], 12);
}

#[test]
fn session_stats_round_trips_across_the_boundary() {
    let native = serde_json::to_value(native_snapshot()).expect("native json");
    let sdk: SessionStats = serde_json::from_value(native.clone()).expect("sdk decode");
    assert_eq!(sdk, sdk_snapshot());
    let native_back: SessionStatsSnapshot =
        serde_json::from_value(serde_json::to_value(sdk).expect("sdk json"))
            .expect("native decode");
    assert_eq!(native_back, native_snapshot());
}

#[test]
fn in_memory_sessions_omit_session_file_on_both_sides() {
    let mut native = native_snapshot();
    native.session_file = None;
    let mut sdk = sdk_snapshot();
    sdk.session_file = None;
    let native_json = serde_json::to_value(native).expect("native json");
    let sdk_json = serde_json::to_value(sdk).expect("sdk json");
    assert_eq!(native_json, sdk_json);
    assert!(native_json.get("sessionFile").is_none());
}

fn overlay_host(state: serde_json::Value) -> impl rpi_ext_sdk::interactive_ui::HostCall {
    struct H(serde_json::Value);
    impl rpi_ext_sdk::interactive_ui::HostCall for H {
        fn call(
            &self,
            _method: &str,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, rpi_ext_sdk::interactive_ui::InteractiveUiError> {
            Ok(self.0.clone())
        }
    }
    H(state)
}

#[test]
fn has_overlay_wire_shape_and_method_match() {
    assert_eq!(METHOD_UI_HAS_OVERLAY, "ui.hasOverlay");
    let native = rpi_ext_host::api::HasOverlayState {
        visible: true,
        capturing: true,
    };
    let native_json = serde_json::to_value(native).expect("native json");
    let sdk = has_overlay(&overlay_host(native_json.clone())).expect("sdk decode");
    assert_eq!(sdk.visible, native.visible);
    assert_eq!(sdk.capturing, native.capturing);
    assert_eq!(
        serde_json::to_value(sdk).expect("sdk json"),
        native_json,
        "byte-identical visible/capturing"
    );
}

#[test]
fn emit_text_method_and_unsupported_token_match() {
    assert_eq!(METHOD_SESSION_STATS, "ctx.sessionStats");
    assert_eq!(METHOD_UI_EMIT_TEXT, "ui.emitText");
    assert_eq!(UNSUPPORTED_OUTPUT, "unsupported-output");
    assert_eq!(
        UiOutputError::UnsupportedOutput.to_string(),
        UNSUPPORTED_OUTPUT
    );
    let error = rpi_ext_sdk::interactive_ui::InteractiveUiError::new(
        rpi_ext_sdk::interactive_ui::InteractiveUiErrorKind::Call,
        UiOutputError::UnsupportedOutput.to_string(),
    );
    assert!(is_unsupported_output(&error));
}
