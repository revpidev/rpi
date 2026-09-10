//! V14-20 C0 (R-U9.4 / G11 item 7): the native mirror in `rpi-ext-host`
//! and the wasm SDK in `rpi-ext-sdk` must freeze the **same** protocol
//! shape. This suite asserts it three ways:
//!
//! 1. byte-level JSON parity over a shared sample corpus (both directions:
//!    serialize native → parse wasm and vice versa, re-serialize equal);
//! 2. identical scripted `run_component` traces (method + args sequence and
//!    terminal result) against one transport script;
//! 3. identical `supports_interactive_ui` probe outcomes for identical host
//!    answers (including the C0 `unknownMethod` signal).
//!
//! `Scripted` implements both `Component` traits with every method: if
//! either trait drops/adds a callback or changes a signature, this file
//! stops compiling (a compile-time method-set assertion, stronger than a
//! runtime reflection check).
//!
//! The native transport (`NativeHostCall`) is exercised against a canned
//! abi_stable trampoline so the envelope parsing is covered too.

use std::collections::VecDeque;
use std::sync::Mutex;

use rpi_ext_host::interactive_ui as native;
use rpi_ext_sdk::interactive_ui as wasm;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Scripted transport (implements both HostCall traits)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Reply {
    Ok(Value),
    Err {
        kind: &'static str,
        message: &'static str,
    },
}

/// `(kind, message)` error wire pair used to compare outcomes.
type ErrorWire = (String, String);
/// Recorded host-call trace: `(method, args)` per call.
type CallTrace = Vec<(String, Value)>;
/// Probe outcome plus the calls it made.
type ProbeOutcome = (Result<bool, ErrorWire>, CallTrace);
/// `run_component` outcome plus the calls it made.
type RunOutcome = (Result<Value, ErrorWire>, CallTrace);

struct FakeHost {
    calls: Mutex<Vec<(String, Value)>>,
    replies: Mutex<VecDeque<Reply>>,
}

impl FakeHost {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        }
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn next(&self, method: &str, args: Value) -> Reply {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), args.clone()));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Err {
                kind: "internal",
                message: "no scripted reply",
            })
    }
}

impl native::HostCall for FakeHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, native::InteractiveUiError> {
        match self.next(method, args) {
            Reply::Ok(value) => Ok(value),
            Reply::Err { kind, message } => {
                Err(native::InteractiveUiError::from_host_error(kind, message))
            }
        }
    }
}

impl wasm::HostCall for FakeHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, wasm::InteractiveUiError> {
        match self.next(method, args) {
            Reply::Ok(value) => Ok(value),
            Reply::Err { kind, message } => {
                Err(wasm::InteractiveUiError::from_host_error(kind, message))
            }
        }
    }
}

/// Serialize both sides and assert byte equality; returns the JSON string.
macro_rules! assert_same_json {
    ($native:expr, $wasm:expr) => {{
        let native_json = serde_json::to_string(&$native).expect("native json");
        let wasm_json = serde_json::to_string(&$wasm).expect("wasm json");
        assert_eq!(native_json, wasm_json, "native vs wasm JSON");
        native_json
    }};
}

/// Parse one JSON sample into both type sets and re-serialize to the same
/// bytes (bidirectional wire compatibility).
macro_rules! assert_cross_parse {
    ($json:expr, $native_ty:ty, $wasm_ty:ty) => {{
        let native_value: $native_ty = serde_json::from_str($json).expect("native parse");
        let wasm_value: $wasm_ty = serde_json::from_str($json).expect("wasm parse");
        assert_eq!(
            serde_json::to_string(&native_value).expect("native reserialize"),
            $json
        );
        assert_eq!(
            serde_json::to_string(&wasm_value).expect("wasm reserialize"),
            $json
        );
    }};
}

// ---------------------------------------------------------------------------
// Frozen table / constants
// ---------------------------------------------------------------------------

#[test]
fn interactive_ui_parity_method_table_and_constants() {
    assert_eq!(
        native::INTERACTIVE_UI_METHODS,
        wasm::INTERACTIVE_UI_METHODS,
        "method table"
    );
    for method in native::INTERACTIVE_UI_METHODS {
        assert!(native::is_interactive_ui_method(method), "{method}");
        assert!(wasm::is_interactive_ui_method(method), "{method}");
    }
    assert!(!native::is_interactive_ui_method("ui.custom"));
    assert!(!wasm::is_interactive_ui_method("ui.custom"));

    assert_eq!(native::CURSOR_MARKER, wasm::CURSOR_MARKER);
    assert_eq!(
        native::DEFAULT_MAX_FRAME_BYTES,
        wasm::DEFAULT_MAX_FRAME_BYTES
    );
    assert_eq!(native::DEFAULT_MAX_FRAME_ROWS, wasm::DEFAULT_MAX_FRAME_ROWS);
    assert_eq!(native::DEFAULT_MAX_LINE_BYTES, wasm::DEFAULT_MAX_LINE_BYTES);
    assert_eq!(native::DEFAULT_MAX_FRAME_BYTES, 1_048_576);
    assert_eq!(native::CURSOR_MARKER, "\x1b_pi:c\x07");

    // V14-22 C2: the wasm carrier limits exist in both mirrors and are
    // strictly tighter than the native defaults (design §4.4 / R-U7.2).
    assert_eq!(
        native::WASM_DEFAULT_MAX_FRAME_BYTES,
        wasm::WASM_DEFAULT_MAX_FRAME_BYTES
    );
    assert_eq!(
        native::WASM_DEFAULT_MAX_FRAME_ROWS,
        wasm::WASM_DEFAULT_MAX_FRAME_ROWS
    );
    assert_eq!(native::WASM_DEFAULT_MAX_FRAME_BYTES, 512 * 1024);
    assert_eq!(native::WASM_DEFAULT_MAX_FRAME_ROWS, 2_000);
    const { assert!(native::WASM_DEFAULT_MAX_FRAME_BYTES < native::DEFAULT_MAX_FRAME_BYTES) };
    const { assert!(native::WASM_DEFAULT_MAX_FRAME_ROWS < native::DEFAULT_MAX_FRAME_ROWS) };
}

/// V14-22 FR-E / R-U9.4 (G11 item 7): the lightweight line/ANSI builders are
/// behaviourally identical in both mirrors (same input → same SGR bytes).
#[test]
fn interactive_ui_parity_line_builders() {
    let native_colors = [
        native::AnsiColor::Basic(2),
        native::AnsiColor::Basic(9),
        native::AnsiColor::Indexed(196),
        native::AnsiColor::Rgb(1, 2, 3),
    ];
    let wasm_colors = [
        wasm::AnsiColor::Basic(2),
        wasm::AnsiColor::Basic(9),
        wasm::AnsiColor::Indexed(196),
        wasm::AnsiColor::Rgb(1, 2, 3),
    ];
    for (native_color, wasm_color) in native_colors.into_iter().zip(wasm_colors) {
        let native_style = native::AnsiStyle::new().bold().underline().fg(native_color);
        let wasm_style = wasm::AnsiStyle::new().bold().underline().fg(wasm_color);
        assert_eq!(native_style.sgr(), wasm_style.sgr());
        assert_eq!(native_style.apply("x"), wasm_style.apply("x"));
    }
    // Plain styles are a passthrough on both sides.
    assert_eq!(
        native::AnsiStyle::new().apply("x"),
        wasm::AnsiStyle::new().apply("x")
    );
    assert_eq!(native::AnsiStyle::new().sgr(), "");
    assert!(native::AnsiStyle::new().is_plain());
    assert!(wasm::AnsiStyle::new().is_plain());

    let mut native_line = native::LineBuilder::new();
    native_line.plain("ab").push(
        "CD",
        native::AnsiStyle::new()
            .bold()
            .fg(native::AnsiColor::Indexed(196)),
    );
    let mut wasm_line = wasm::LineBuilder::new();
    wasm_line.plain("ab").push(
        "CD",
        wasm::AnsiStyle::new()
            .bold()
            .fg(wasm::AnsiColor::Indexed(196)),
    );
    assert_eq!(native_line.build(), wasm_line.build());
    assert_eq!(native_line.build(), "ab\x1b[1;38;5;196mCD\x1b[0m");
    assert_eq!(native_line.len(), wasm_line.len());
    assert_eq!(native_line.is_empty(), wasm_line.is_empty());

    // A styled line survives the frame wire shape unchanged (host renders it
    // verbatim; the wasm guest produces the same bytes).
    let json = assert_same_json!(
        native::ComponentFrame::lines(vec![native_line.build()]),
        wasm::ComponentFrame::lines(vec![wasm_line.build()])
    );
    assert_cross_parse!(json.as_str(), native::ComponentFrame, wasm::ComponentFrame);
}

// ---------------------------------------------------------------------------
// MountOptions / OverlayOptions
// ---------------------------------------------------------------------------

#[test]
fn interactive_ui_parity_mount_options() {
    let json = assert_same_json!(
        native::MountOptions::default(),
        wasm::MountOptions::default()
    );
    assert_eq!(
        json,
        r#"{"overlay":true,"tickMs":0,"keysWhenHidden":[],"cursor":true,"maxFrameBytes":1048576}"#
    );
    assert_cross_parse!(json.as_str(), native::MountOptions, wasm::MountOptions);

    let native_full = native::MountOptions {
        overlay: false,
        overlay_options: Some(native::OverlayOptions {
            anchor: Some(native::OverlayAnchor::BottomCenter),
            width: Some(native::SizeValue::Percent(100.0)),
            min_width: Some(native::SizeValue::Absolute(40)),
            max_height: Some(native::SizeValue::Percent(100.0)),
            row: None,
            col: Some(native::SizeValue::Absolute(2)),
            margin: Some(native::Margin {
                left: 1,
                right: 2,
                bottom: 3,
                top: 4,
            }),
            non_capturing: true,
        }),
        tick_ms: 250,
        keys_when_hidden: vec!["ctrl+]".to_owned(), "alt+h".to_owned()],
        cursor: false,
        max_frame_bytes: 4096,
        label: Some("ask_user_question".to_owned()),
    };
    let wasm_full = wasm::MountOptions {
        overlay: false,
        overlay_options: Some(wasm::OverlayOptions {
            anchor: Some(wasm::OverlayAnchor::BottomCenter),
            width: Some(wasm::SizeValue::Percent(100.0)),
            min_width: Some(wasm::SizeValue::Absolute(40)),
            max_height: Some(wasm::SizeValue::Percent(100.0)),
            row: None,
            col: Some(wasm::SizeValue::Absolute(2)),
            margin: Some(wasm::Margin {
                left: 1,
                right: 2,
                bottom: 3,
                top: 4,
            }),
            non_capturing: true,
        }),
        tick_ms: 250,
        keys_when_hidden: vec!["ctrl+]".to_owned(), "alt+h".to_owned()],
        cursor: false,
        max_frame_bytes: 4096,
        label: Some("ask_user_question".to_owned()),
    };
    let json = assert_same_json!(native_full, wasm_full);
    assert_cross_parse!(json.as_str(), native::MountOptions, wasm::MountOptions);
}

#[test]
fn interactive_ui_parity_overlay_anchors_and_sizes() {
    let anchors = [
        (
            native::OverlayAnchor::Center,
            wasm::OverlayAnchor::Center,
            "\"center\"",
        ),
        (
            native::OverlayAnchor::TopLeft,
            wasm::OverlayAnchor::TopLeft,
            "\"top-left\"",
        ),
        (
            native::OverlayAnchor::TopRight,
            wasm::OverlayAnchor::TopRight,
            "\"top-right\"",
        ),
        (
            native::OverlayAnchor::BottomLeft,
            wasm::OverlayAnchor::BottomLeft,
            "\"bottom-left\"",
        ),
        (
            native::OverlayAnchor::BottomRight,
            wasm::OverlayAnchor::BottomRight,
            "\"bottom-right\"",
        ),
        (
            native::OverlayAnchor::TopCenter,
            wasm::OverlayAnchor::TopCenter,
            "\"top-center\"",
        ),
        (
            native::OverlayAnchor::BottomCenter,
            wasm::OverlayAnchor::BottomCenter,
            "\"bottom-center\"",
        ),
        (
            native::OverlayAnchor::LeftCenter,
            wasm::OverlayAnchor::LeftCenter,
            "\"left-center\"",
        ),
        (
            native::OverlayAnchor::RightCenter,
            wasm::OverlayAnchor::RightCenter,
            "\"right-center\"",
        ),
    ];
    for (native_anchor, wasm_anchor, expected) in anchors {
        let json = assert_same_json!(native_anchor, wasm_anchor);
        assert_eq!(json, expected);
    }

    let sizes = [
        (
            native::SizeValue::Absolute(40),
            wasm::SizeValue::Absolute(40),
            "40",
        ),
        (
            native::SizeValue::Percent(100.0),
            wasm::SizeValue::Percent(100.0),
            "\"100%\"",
        ),
        (
            native::SizeValue::Percent(12.5),
            wasm::SizeValue::Percent(12.5),
            "\"12.5%\"",
        ),
    ];
    for (native_size, wasm_size, expected) in sizes {
        let json = assert_same_json!(native_size, wasm_size);
        assert_eq!(json, expected);
    }

    assert_same_json!(
        native::OverlayOptions::default(),
        wasm::OverlayOptions::default()
    );
    assert_same_json!(
        native::Margin {
            left: 0,
            right: 0,
            bottom: 0,
            top: 0
        },
        wasm::Margin {
            left: 0,
            right: 0,
            bottom: 0,
            top: 0
        }
    );
}

// ---------------------------------------------------------------------------
// ComponentEvent / frame
// ---------------------------------------------------------------------------

#[test]
fn interactive_ui_parity_component_events() {
    let pairs: Vec<(native::ComponentEvent, wasm::ComponentEvent, &str)> = vec![
        (
            native::ComponentEvent::Resize {
                width: 80,
                height: 24,
            },
            wasm::ComponentEvent::Resize {
                width: 80,
                height: 24,
            },
            r#"{"type":"resize","width":80,"height":24}"#,
        ),
        (
            native::ComponentEvent::Input {
                data: "\u{1b}[B".to_owned(),
            },
            wasm::ComponentEvent::Input {
                data: "\u{1b}[B".to_owned(),
            },
            "{\"type\":\"input\",\"data\":\"\\u001b[B\"}",
        ),
        (
            native::ComponentEvent::Input {
                data: "\u{1b}[97;5u".to_owned(),
            },
            wasm::ComponentEvent::Input {
                data: "\u{1b}[97;5u".to_owned(),
            },
            "{\"type\":\"input\",\"data\":\"\\u001b[97;5u\"}",
        ),
        (
            native::ComponentEvent::Focus,
            wasm::ComponentEvent::Focus,
            r#"{"type":"focus"}"#,
        ),
        (
            native::ComponentEvent::Blur,
            wasm::ComponentEvent::Blur,
            r#"{"type":"blur"}"#,
        ),
        (
            native::ComponentEvent::Tick,
            wasm::ComponentEvent::Tick,
            r#"{"type":"tick"}"#,
        ),
        (
            native::ComponentEvent::Theme {
                theme: json!({"name": "dark"}),
            },
            wasm::ComponentEvent::Theme {
                theme: json!({"name": "dark"}),
            },
            r#"{"type":"theme","theme":{"name":"dark"}}"#,
        ),
        (
            native::ComponentEvent::Visibility { hidden: true },
            wasm::ComponentEvent::Visibility { hidden: true },
            r#"{"type":"visibility","hidden":true}"#,
        ),
        (
            native::ComponentEvent::Render,
            wasm::ComponentEvent::Render,
            r#"{"type":"render"}"#,
        ),
        (
            native::ComponentEvent::Dispose {
                reason: native::DisposeReason::Timeout,
            },
            wasm::ComponentEvent::Dispose {
                reason: wasm::DisposeReason::Timeout,
            },
            r#"{"type":"dispose","reason":"timeout"}"#,
        ),
    ];
    for (native_event, wasm_event, expected) in pairs {
        let json = assert_same_json!(native_event, wasm_event);
        assert_eq!(json, expected);
        assert_eq!(native_event.kind(), wasm_event.kind());
        assert_cross_parse!(json.as_str(), native::ComponentEvent, wasm::ComponentEvent);
    }
}

#[test]
fn interactive_ui_parity_frames_and_done_values() {
    let plain = assert_same_json!(
        native::ComponentFrame::lines(vec!["hello".to_owned()]),
        wasm::ComponentFrame::lines(vec!["hello".to_owned()])
    );
    assert_eq!(plain, r#"{"lines":["hello"]}"#);
    assert_cross_parse!(plain.as_str(), native::ComponentFrame, wasm::ComponentFrame);

    let cursor = assert_same_json!(
        native::ComponentFrame {
            lines: vec!["x".to_owned()],
            cursor: Some(native::ComponentCursor { row: 3, col: 7 }),
            done: native::DoneValue::Absent,
        },
        wasm::ComponentFrame {
            lines: vec!["x".to_owned()],
            cursor: Some(wasm::ComponentCursor { row: 3, col: 7 }),
            done: wasm::DoneValue::Absent,
        }
    );
    assert_eq!(cursor, r#"{"lines":["x"],"cursor":{"row":3,"col":7}}"#);
    assert_cross_parse!(
        cursor.as_str(),
        native::ComponentFrame,
        wasm::ComponentFrame
    );

    let marker = assert_same_json!(
        native::ComponentFrame::lines(vec![format!("a{}b", native::CURSOR_MARKER)]),
        wasm::ComponentFrame::lines(vec![format!("a{}b", wasm::CURSOR_MARKER)])
    );
    assert!(marker.contains("\\u001b_pi:c\\u0007"), "{marker}");

    let done = assert_same_json!(
        native::ComponentFrame::lines(vec!["x".to_owned()]).with_done(json!({"answers": [1, 2]})),
        wasm::ComponentFrame::lines(vec!["x".to_owned()]).with_done(json!({"answers": [1, 2]}))
    );
    assert_eq!(done, r#"{"lines":["x"],"done":{"answers":[1,2]}}"#);
    assert_cross_parse!(done.as_str(), native::ComponentFrame, wasm::ComponentFrame);

    // `done: null` (upstream done(undefined)) is distinct from absent on
    // both sides.
    let null_done = assert_same_json!(
        native::ComponentFrame::lines(vec!["x".to_owned()]).with_done(Value::Null),
        wasm::ComponentFrame::lines(vec!["x".to_owned()]).with_done(Value::Null)
    );
    assert_eq!(null_done, r#"{"lines":["x"],"done":null}"#);
    assert_cross_parse!(
        null_done.as_str(),
        native::ComponentFrame,
        wasm::ComponentFrame
    );
    let native_parsed: native::ComponentFrame = serde_json::from_str(&null_done).unwrap();
    let wasm_parsed: wasm::ComponentFrame = serde_json::from_str(&null_done).unwrap();
    assert!(native_parsed.is_done() && wasm_parsed.is_done());
    assert_eq!(
        native_parsed.done.value(),
        wasm_parsed.done.value(),
        "null done value"
    );
}

// ---------------------------------------------------------------------------
// Errors / reasons / state machine
// ---------------------------------------------------------------------------

#[test]
fn interactive_ui_parity_error_kinds_and_reasons() {
    let kinds = [
        (
            native::InteractiveUiErrorKind::CapabilityDenied,
            wasm::InteractiveUiErrorKind::CapabilityDenied,
            "capabilityDenied",
        ),
        (
            native::InteractiveUiErrorKind::InvalidRequest,
            wasm::InteractiveUiErrorKind::InvalidRequest,
            "invalidRequest",
        ),
        (
            native::InteractiveUiErrorKind::UnknownMethod,
            wasm::InteractiveUiErrorKind::UnknownMethod,
            "unknownMethod",
        ),
        (
            native::InteractiveUiErrorKind::Call,
            wasm::InteractiveUiErrorKind::Call,
            "call",
        ),
        (
            native::InteractiveUiErrorKind::Internal,
            wasm::InteractiveUiErrorKind::Internal,
            "internal",
        ),
        (
            native::InteractiveUiErrorKind::HandlerError,
            wasm::InteractiveUiErrorKind::HandlerError,
            "handlerError",
        ),
        (
            native::InteractiveUiErrorKind::FuelExhausted,
            wasm::InteractiveUiErrorKind::FuelExhausted,
            "fuelExhausted",
        ),
        (
            native::InteractiveUiErrorKind::ProtocolError,
            wasm::InteractiveUiErrorKind::ProtocolError,
            "protocolError",
        ),
        (
            native::InteractiveUiErrorKind::Other("futureKind".to_owned()),
            wasm::InteractiveUiErrorKind::Other("futureKind".to_owned()),
            "futureKind",
        ),
    ];
    for (native_kind, wasm_kind, wire) in kinds {
        assert_eq!(native_kind.as_str(), wasm_kind.as_str());
        assert_eq!(native_kind.as_str(), wire);
        assert_eq!(
            native::InteractiveUiErrorKind::from_wire(wire).as_str(),
            wasm::InteractiveUiErrorKind::from_wire(wire).as_str()
        );
    }

    // Semantic helpers map to the design §2.4 kinds on both sides.
    assert_eq!(
        native::InteractiveUiError::component_already_mounted()
            .kind
            .as_str(),
        wasm::InteractiveUiError::component_already_mounted()
            .kind
            .as_str()
    );
    assert_eq!(
        native::InteractiveUiError::component_already_mounted()
            .kind
            .as_str(),
        "call"
    );
    assert_eq!(
        native::InteractiveUiError::unknown_handle(native::ComponentHandle(3))
            .kind
            .as_str(),
        wasm::InteractiveUiError::unknown_handle(wasm::ComponentHandle(3))
            .kind
            .as_str()
    );
    assert_eq!(
        native::InteractiveUiError::unknown_handle(native::ComponentHandle(3))
            .kind
            .as_str(),
        "invalidRequest"
    );
    assert_eq!(
        native::InteractiveUiError::frame_too_large("rows 5001 > 5000")
            .kind
            .as_str(),
        wasm::InteractiveUiError::frame_too_large("rows 5001 > 5000")
            .kind
            .as_str()
    );
    assert_eq!(
        native::InteractiveUiError::frame_too_large("rows 5001 > 5000")
            .kind
            .as_str(),
        "invalidRequest"
    );

    // The five dispose reasons agree on wire strings and parse behaviour.
    assert_eq!(native::DisposeReason::ALL.len(), 5);
    assert_eq!(wasm::DisposeReason::ALL.len(), 5);
    for index in 0..5 {
        let native_reason = native::DisposeReason::ALL[index];
        let wasm_reason = wasm::DisposeReason::ALL[index];
        assert_eq!(native_reason.as_str(), wasm_reason.as_str());
        assert_eq!(
            native::DisposeReason::parse(native_reason.as_str()).unwrap(),
            native_reason
        );
        assert_eq!(
            wasm::DisposeReason::parse(wasm_reason.as_str()).unwrap(),
            wasm_reason
        );
    }
    let native_error = native::DisposeReason::parse("vanished").unwrap_err();
    let wasm_error = wasm::DisposeReason::parse("vanished").unwrap_err();
    assert_eq!(native_error.kind.as_str(), wasm_error.kind.as_str());
    assert_eq!(native_error.kind.as_str(), "invalidRequest");
    assert_eq!(native_error.message, wasm_error.message);

    // State machine: terminal classification is identical.
    let states = [
        (
            native::ComponentState::Mounted,
            wasm::ComponentState::Mounted,
        ),
        (
            native::ComponentState::Blurred,
            wasm::ComponentState::Blurred,
        ),
        (native::ComponentState::Hidden, wasm::ComponentState::Hidden),
        (
            native::ComponentState::Disposing,
            wasm::ComponentState::Disposing,
        ),
        (native::ComponentState::Closed, wasm::ComponentState::Closed),
    ];
    for (native_state, wasm_state) in states {
        assert_eq!(native_state.is_terminal(), wasm_state.is_terminal());
    }
}

// ---------------------------------------------------------------------------
// Host-call argument payloads
// ---------------------------------------------------------------------------

#[test]
fn interactive_ui_parity_host_call_argument_payloads() {
    assert_same_json!(
        native::MountComponentArgs {
            options: native::MountOptions::default()
        },
        wasm::MountComponentArgs {
            options: wasm::MountOptions::default()
        }
    );
    assert_same_json!(
        native::PollComponentArgs {
            handle: native::ComponentHandle(7)
        },
        wasm::PollComponentArgs {
            handle: wasm::ComponentHandle(7)
        }
    );
    let render_json = assert_same_json!(
        native::RenderComponentArgs {
            handle: native::ComponentHandle(7),
            frame: native::ComponentFrame::lines(vec!["x".to_owned()])
        },
        wasm::RenderComponentArgs {
            handle: wasm::ComponentHandle(7),
            frame: wasm::ComponentFrame::lines(vec!["x".to_owned()])
        }
    );
    assert_eq!(render_json, r#"{"handle":7,"lines":["x"]}"#);
    assert_cross_parse!(
        render_json.as_str(),
        native::RenderComponentArgs,
        wasm::RenderComponentArgs
    );
    assert_same_json!(
        native::SetComponentHiddenArgs {
            handle: native::ComponentHandle(7),
            hidden: true
        },
        wasm::SetComponentHiddenArgs {
            handle: wasm::ComponentHandle(7),
            hidden: true
        }
    );
    assert_same_json!(
        native::WakeComponentArgs {
            handle: native::ComponentHandle(7)
        },
        wasm::WakeComponentArgs {
            handle: wasm::ComponentHandle(7)
        }
    );
    assert_same_json!(
        native::DisposeComponentArgs {
            handle: native::ComponentHandle(7)
        },
        wasm::DisposeComponentArgs {
            handle: wasm::ComponentHandle(7)
        }
    );
    let edit_json = assert_same_json!(
        native::EditExternalArgs {
            text: "draft".to_owned(),
            language: Some("markdown".to_owned())
        },
        wasm::EditExternalArgs {
            text: "draft".to_owned(),
            language: Some("markdown".to_owned())
        }
    );
    assert_eq!(edit_json, r#"{"text":"draft","language":"markdown"}"#);
    assert_same_json!(
        native::EditExternalArgs {
            text: "draft".to_owned(),
            language: None
        },
        wasm::EditExternalArgs {
            text: "draft".to_owned(),
            language: None
        }
    );
}

// ---------------------------------------------------------------------------
// Probe parity
// ---------------------------------------------------------------------------

fn native_probe(reply: Reply) -> ProbeOutcome {
    let host = FakeHost::new(vec![reply]);
    let result = native::supports_interactive_ui(&host)
        .map_err(|error| (error.kind.as_str().to_owned(), error.message));
    (result, host.calls())
}

fn wasm_probe(reply: Reply) -> ProbeOutcome {
    let host = FakeHost::new(vec![reply]);
    let result = wasm::supports_interactive_ui(&host)
        .map_err(|error| (error.kind.as_str().to_owned(), error.message));
    (result, host.calls())
}

#[test]
fn interactive_ui_parity_probe_matrix() {
    let scenarios = [
        // C0 / old host: unsupported.
        (
            Reply::Err {
                kind: "unknownMethod",
                message: "not implemented",
            },
            Ok(false),
        ),
        // Method exists, args rejected before mounting: supported.
        (
            Reply::Err {
                kind: "invalidRequest",
                message: "missing options",
            },
            Ok(true),
        ),
        // Success: supported.
        (Reply::Ok(json!({"handle": 1})), Ok(true)),
    ];
    for (reply, expected) in scenarios {
        let (native_result, native_calls) = native_probe(reply.clone());
        let (wasm_result, wasm_calls) = wasm_probe(reply);
        assert_eq!(native_result, wasm_result);
        assert_eq!(native_result, expected);
        assert_eq!(native_calls, wasm_calls, "probe call trace");
        assert_eq!(native_calls.len(), 1);
        assert_eq!(native_calls[0].0, "ui.mountComponent");
        assert_eq!(native_calls[0].1, json!({}), "probe must send empty args");
    }

    // Errors that do not prove support propagate identically.
    for reply in [
        Reply::Err {
            kind: "capabilityDenied",
            message: "requires ui",
        },
        Reply::Err {
            kind: "internal",
            message: "boom",
        },
    ] {
        let (native_result, _) = native_probe(reply.clone());
        let (wasm_result, _) = wasm_probe(reply);
        assert_eq!(native_result, wasm_result);
        assert!(native_result.is_err());
    }
}

// ---------------------------------------------------------------------------
// run_component trace parity
// ---------------------------------------------------------------------------

/// One component implementing both traits with the **complete** method set:
/// a missing/renamed callback or a changed signature in either trait is a
/// compile error here.
struct Scripted {
    width: usize,
    ticks: usize,
    done_after: usize,
}

impl Scripted {
    fn new(done_after: usize) -> Self {
        Self {
            width: 0,
            ticks: 0,
            done_after,
        }
    }
}

impl native::Component for Scripted {
    fn render(&mut self, width: usize) -> Vec<String> {
        vec![format!("w={width} ticks={}", self.ticks)]
    }
    fn handle_input(&mut self, _data: &str) {}
    fn on_focus(&mut self) {}
    fn on_blur(&mut self) {}
    fn on_tick(&mut self) {
        self.ticks += 1;
    }
    fn on_resize(&mut self, width: usize, _height: usize) {
        self.width = width;
    }
    fn on_theme(&mut self, _theme: &Value) {}
    fn on_visibility(&mut self, _hidden: bool) {}
    fn on_render(&mut self) {}
    fn on_dispose(&mut self, _reason: native::DisposeReason) {}
    fn done(&mut self) -> Option<Value> {
        (self.ticks >= self.done_after).then(|| json!({"ticks": self.ticks}))
    }
}

impl wasm::Component for Scripted {
    fn render(&mut self, width: usize) -> Vec<String> {
        vec![format!("w={width} ticks={}", self.ticks)]
    }
    fn handle_input(&mut self, _data: &str) {}
    fn on_focus(&mut self) {}
    fn on_blur(&mut self) {}
    fn on_tick(&mut self) {
        self.ticks += 1;
    }
    fn on_resize(&mut self, width: usize, _height: usize) {
        self.width = width;
    }
    fn on_theme(&mut self, _theme: &Value) {}
    fn on_visibility(&mut self, _hidden: bool) {}
    fn on_render(&mut self) {}
    fn on_dispose(&mut self, _reason: wasm::DisposeReason) {}
    fn done(&mut self) -> Option<Value> {
        (self.ticks >= self.done_after).then(|| json!({"ticks": self.ticks}))
    }
}

fn native_trace(replies: Vec<Reply>, done_after: usize) -> RunOutcome {
    let host = FakeHost::new(replies);
    let result = native::run_component(
        &host,
        Scripted::new(done_after),
        native::MountOptions::default(),
    )
    .map_err(|error| (error.kind.as_str().to_owned(), error.message));
    (result, host.calls())
}

fn wasm_trace(replies: Vec<Reply>, done_after: usize) -> RunOutcome {
    let host = FakeHost::new(replies);
    let result = wasm::run_component(
        &host,
        Scripted::new(done_after),
        wasm::MountOptions::default(),
    )
    .map_err(|error| (error.kind.as_str().to_owned(), error.message));
    (result, host.calls())
}

#[test]
fn interactive_ui_parity_run_component_trace() {
    // Normal run: resize → tick → done.
    let replies = vec![
        Reply::Ok(json!({"handle": 7})),
        Reply::Ok(json!({"event": {"type": "resize", "width": 80, "height": 24}})),
        Reply::Ok(json!({"ok": true})),
        Reply::Ok(json!({"event": {"type": "tick"}})),
        Reply::Ok(json!({"ok": true})),
    ];
    let (native_result, native_calls) = native_trace(replies.clone(), 1);
    let (wasm_result, wasm_calls) = wasm_trace(replies, 1);
    assert_eq!(native_result, wasm_result);
    assert_eq!(native_result, Ok(json!({"ticks": 1})));
    assert_eq!(native_calls, wasm_calls);
    assert_eq!(native_calls.len(), 5);
    assert_eq!(native_calls[0].0, "ui.mountComponent");
    assert_eq!(native_calls[1].0, "ui.pollComponent");
    assert_eq!(
        native_calls[2],
        (
            "ui.renderComponent".to_owned(),
            json!({"handle": 7, "lines": ["w=80 ticks=0"]})
        )
    );
    assert_eq!(
        native_calls[4],
        (
            "ui.renderComponent".to_owned(),
            json!({"handle": 7, "lines": ["w=80 ticks=1"], "done": {"ticks": 1}})
        )
    );

    // Host dispose: identical final-frame best-effort + null result.
    let replies = vec![
        Reply::Ok(json!({"handle": 8})),
        Reply::Ok(json!({"event": {"type": "dispose", "reason": "sessionReload"}})),
        Reply::Err {
            kind: "invalidRequest",
            message: "unknownHandle",
        },
    ];
    let (native_result, native_calls) = native_trace(replies.clone(), 99);
    let (wasm_result, wasm_calls) = wasm_trace(replies, 99);
    assert_eq!(native_result, wasm_result);
    assert_eq!(native_result, Ok(Value::Null));
    assert_eq!(native_calls, wasm_calls);
    assert_eq!(native_calls[2].0, "ui.renderComponent");

    // C0 host: mount answers unknownMethod, both loops return the same
    // structured error without further calls.
    let replies = vec![Reply::Err {
        kind: "unknownMethod",
        message: "C0 protocol freeze",
    }];
    let (native_result, native_calls) = native_trace(replies.clone(), 1);
    let (wasm_result, wasm_calls) = wasm_trace(replies, 1);
    assert_eq!(native_result, wasm_result);
    assert_eq!(
        native_result,
        Err(("unknownMethod".to_owned(), "C0 protocol freeze".to_owned()))
    );
    assert_eq!(native_calls, wasm_calls);
    assert_eq!(native_calls.len(), 1);

    // Unknown handle on poll propagates identically.
    let replies = vec![
        Reply::Ok(json!({"handle": 9})),
        Reply::Err {
            kind: "invalidRequest",
            message: "unknownHandle: 9",
        },
    ];
    let (native_result, native_calls) = native_trace(replies.clone(), 1);
    let (wasm_result, wasm_calls) = wasm_trace(replies, 1);
    assert_eq!(native_result, wasm_result);
    assert_eq!(
        native_result,
        Err(("invalidRequest".to_owned(), "unknownHandle: 9".to_owned()))
    );
    assert_eq!(native_calls, wasm_calls);
}

// ---------------------------------------------------------------------------
// Component::cursor() callback (V14-22 additive SDK seam)
// ---------------------------------------------------------------------------

/// Implements both `Component` traits; `cursor()` returns an explicit
/// position so the frame's `cursor` field is exercised on both carriers.
struct CursorScripted;

impl native::Component for CursorScripted {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec!["cursor-frame".to_owned()]
    }
    fn handle_input(&mut self, _data: &str) {}
    fn cursor(&self) -> Option<native::ComponentCursor> {
        Some(native::ComponentCursor { row: 1, col: 2 })
    }
    fn done(&mut self) -> Option<Value> {
        Some(json!({"cursor": true}))
    }
}

impl wasm::Component for CursorScripted {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec!["cursor-frame".to_owned()]
    }
    fn handle_input(&mut self, _data: &str) {}
    fn cursor(&self) -> Option<wasm::ComponentCursor> {
        Some(wasm::ComponentCursor { row: 1, col: 2 })
    }
    fn done(&mut self) -> Option<Value> {
        Some(json!({"cursor": true}))
    }
}

#[test]
fn interactive_ui_parity_component_cursor_callback() {
    let replies = vec![
        Reply::Ok(json!({"handle": 3})),
        Reply::Ok(json!({"event": {"type": "resize", "width": 40, "height": 10}})),
        Reply::Ok(json!({"ok": true})),
    ];
    let native_host = FakeHost::new(replies.clone());
    let native_result = native::run_component(
        &native_host,
        CursorScripted,
        native::MountOptions::default(),
    )
    .map_err(|error| (error.kind.as_str().to_owned(), error.message));
    let wasm_host = FakeHost::new(replies);
    let wasm_result =
        wasm::run_component(&wasm_host, CursorScripted, wasm::MountOptions::default())
            .map_err(|error| (error.kind.as_str().to_owned(), error.message));

    assert_eq!(native_result, wasm_result);
    assert_eq!(native_result, Ok(json!({"cursor": true})));
    assert_eq!(native_host.calls(), wasm_host.calls());
    assert_eq!(native_host.calls().len(), 3);
    assert_eq!(
        native_host.calls()[2],
        (
            "ui.renderComponent".to_owned(),
            json!({
                "handle": 3,
                "lines": ["cursor-frame"],
                "cursor": {"row": 1, "col": 2},
                "done": {"cursor": true},
            })
        )
    );
}

// ---------------------------------------------------------------------------
// NativeHostCall transport envelope
// ---------------------------------------------------------------------------

static CANNED_RESPONSE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

extern "C" fn canned_trampoline(
    _cookie: rpi_ext_host::native::PluginCookie,
    _request: abi_stable::std_types::RVec<u8>,
) -> abi_stable::std_types::RVec<u8> {
    let bytes = CANNED_RESPONSE.lock().unwrap().clone().unwrap_or_default();
    abi_stable::std_types::RVec::from(bytes)
}

#[test]
fn interactive_ui_parity_native_host_call_envelope() {
    use native::HostCall as NativeHostCallTrait;

    let transport = native::NativeHostCall::new(canned_trampoline, std::ptr::null());

    *CANNED_RESPONSE.lock().unwrap() = Some(br#"{"ok":{"handle":5}}"#.to_vec());
    let value = NativeHostCallTrait::call(&transport, "ui.mountComponent", json!({"options": {}}))
        .expect("ok envelope");
    assert_eq!(value, json!({"handle": 5}));

    *CANNED_RESPONSE.lock().unwrap() =
        Some(br#"{"error":{"kind":"unknownMethod","message":"nope"}}"#.to_vec());
    let error = NativeHostCallTrait::call(&transport, "ui.mountComponent", json!({}))
        .expect_err("error envelope");
    assert_eq!(error.kind.as_str(), "unknownMethod");
    assert_eq!(error.message, "nope");

    *CANNED_RESPONSE.lock().unwrap() = Some(b"not json".to_vec());
    let error = NativeHostCallTrait::call(&transport, "ui.mountComponent", json!({}))
        .expect_err("bad json");
    assert_eq!(error.kind.as_str(), "protocolError");
}
