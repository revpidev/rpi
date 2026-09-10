//! Interactive custom UI ABI (ADR-0024) — native (L0) guest-side protocol
//! types.
//!
//! v0.1.4 **C0** freezes the wire shape of the seven additive `ui.*`
//! host-calls shared by the native (L0) and wasm (L1) carriers: the method
//! table, the `MountOptions`/`ComponentEvent`/`ComponentFrame` JSON schemas,
//! the lifecycle state set and the semantic error kinds. The host returns
//! `unknownMethod` until C1 (native) / C2 (wasm) land, so
//! [`supports_interactive_ui`] can probe the boundary without side effects.
//!
//! This module is the **native mirror** of `rpi_ext_sdk::interactive_ui`
//! (the wasm SDK crate cannot be depended on by native plugins, which link
//! `rpi-ext-host` for the abi_stable ABI). The two definitions are kept
//! field-for-field identical by `tests/interactive_ui_parity.rs` (G11
//! item 7 / R-U9.4).
//!
//! References (rpi-docs):
//! - `extensions/interactive-ui-abi/01-requirements.md` §3.0–§3.9 (R-U1–R-U9)
//! - `extensions/interactive-ui-abi/02-design.md` §2 (protocol), §5 (SDK)
//! - `adr/0024-interactive-custom-ui-abi.md` (decisions 1–10)
//! - `extension-abi.md` §3 / §8.4 (planning rows; C0 keeps them planning)
//!
//! [RPI-OWN]: the polling/line-frame mechanism has no upstream counterpart;
//! the `Component` trait mirrors upstream `ctx.ui.custom()`'s
//! `render(width)`/`handleInput(data)` contract (`types.ts:197-212`,
//! `tui.ts:111-135` @ `9841914`).

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

// ============================================================================
// Frozen method table (ADR-0024 decision 2; extension-abi.md §3)
// ============================================================================

/// `ui.mountComponent` — mount a component (overlay or editor region),
/// non-blocking, returns `{handle}`.
pub const METHOD_MOUNT_COMPONENT: &str = "ui.mountComponent";
/// `ui.pollComponent` — blocking wait for the next `ComponentEvent`.
pub const METHOD_POLL_COMPONENT: &str = "ui.pollComponent";
/// `ui.renderComponent` — submit a frame (`{handle, lines, cursor?, done?}`).
pub const METHOD_RENDER_COMPONENT: &str = "ui.renderComponent";
/// `ui.setComponentHidden` — collapse/expand a mounted component.
pub const METHOD_SET_COMPONENT_HIDDEN: &str = "ui.setComponentHidden";
/// `ui.wakeComponent` — wake a blocked `pollComponent` with a `render` event.
pub const METHOD_WAKE_COMPONENT: &str = "ui.wakeComponent";
/// `ui.disposeComponent` — guest-side unmount.
pub const METHOD_DISPOSE_COMPONENT: &str = "ui.disposeComponent";
/// `ui.editExternal` — host external editor (P1; lands with C3).
pub const METHOD_EDIT_EXTERNAL: &str = "ui.editExternal";

/// The seven additive host-calls frozen by C0, in method-table order.
/// Every entry is capability `ui`, additive and does **not** bump `rpiAbi`
/// (ADR-0013 / ADR-0024 decision 2).
pub const INTERACTIVE_UI_METHODS: [&str; 7] = [
    METHOD_MOUNT_COMPONENT,
    METHOD_POLL_COMPONENT,
    METHOD_RENDER_COMPONENT,
    METHOD_SET_COMPONENT_HIDDEN,
    METHOD_WAKE_COMPONENT,
    METHOD_DISPOSE_COMPONENT,
    METHOD_EDIT_EXTERNAL,
];

/// Whether `method` is one of the C0-frozen interactive UI host-calls.
pub fn is_interactive_ui_method(method: &str) -> bool {
    INTERACTIVE_UI_METHODS.contains(&method)
}

/// Default frame byte budget (`maxFrameBytes`, design §2.2 / R-U3.5).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1_048_576;
/// Default frame row limit (design §2.6: rows ≤ 5000).
pub const DEFAULT_MAX_FRAME_ROWS: usize = 5_000;
/// Default per-line byte limit (design §2.6: single line ≤ 32 KiB).
pub const DEFAULT_MAX_LINE_BYTES: usize = 32 * 1024;
/// wasm (L1) default frame byte cap (design §4.2/§4.4: total ≤ 512 KiB).
///
/// The host enforces this stricter bound on the wasm carrier even when a
/// guest asks for a larger `maxFrameBytes` (R-U7.2); native guests keep
/// [`DEFAULT_MAX_FRAME_BYTES`].
pub const WASM_DEFAULT_MAX_FRAME_BYTES: usize = 512 * 1024;
/// wasm (L1) frame row cap (design §4.2/§4.4: rows ≤ 2000).
pub const WASM_DEFAULT_MAX_FRAME_ROWS: usize = 2_000;
/// Cursor marker stripped by the host (rpi-tui `CURSOR_MARKER`,
/// `tui.rs:163`); a frame may embed it instead of `cursor:{row,col}`.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

// ============================================================================
// Error kinds (extension-abi.md §4 + design §2.4)
// ============================================================================

/// Error kind of a failed interactive UI host-call.
///
/// The first five reuse the ABI error kind table (`extension-abi.md` §4);
/// the last three are the guest-side kinds of R-U6.2 (trap/fuel/protocol).
/// Unknown host kinds round-trip through [`Self::Other`] (forward compatible,
/// never a panic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveUiErrorKind {
    /// Capability `ui` was not granted (checked before dispatch).
    CapabilityDenied,
    /// Malformed request / unknown handle / oversized frame.
    InvalidRequest,
    /// The host does not implement this method (C0/old host) — the probe
    /// signal of R-U9.2.
    UnknownMethod,
    /// Host-side action failure (e.g. `componentAlreadyMounted`).
    Call,
    /// Host internal error.
    Internal,
    /// Guest handler trapped while the host was driving it (R-U6.2).
    HandlerError,
    /// Guest exceeded its fuel budget (R-U6.2).
    FuelExhausted,
    /// Wire-level protocol failure (bad JSON, transport unavailable).
    ProtocolError,
    /// A kind this SDK does not know (forward compatible).
    Other(String),
}

impl InteractiveUiErrorKind {
    /// The wire string of this kind.
    pub fn as_str(&self) -> &str {
        match self {
            Self::CapabilityDenied => "capabilityDenied",
            Self::InvalidRequest => "invalidRequest",
            Self::UnknownMethod => "unknownMethod",
            Self::Call => "call",
            Self::Internal => "internal",
            Self::HandlerError => "handlerError",
            Self::FuelExhausted => "fuelExhausted",
            Self::ProtocolError => "protocolError",
            Self::Other(kind) => kind,
        }
    }

    /// Map a wire string onto a kind; unknown strings become [`Self::Other`].
    pub fn from_wire(kind: &str) -> Self {
        match kind {
            "capabilityDenied" => Self::CapabilityDenied,
            "invalidRequest" => Self::InvalidRequest,
            "unknownMethod" => Self::UnknownMethod,
            "call" => Self::Call,
            "internal" => Self::Internal,
            "handlerError" => Self::HandlerError,
            "fuelExhausted" => Self::FuelExhausted,
            "protocolError" => Self::ProtocolError,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for InteractiveUiErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A structured interactive UI error: `{kind, message}` as returned by the
/// host (`{"error": {"kind", "message"}}`).
///
/// `Display` renders the message only, so the pre-C0 `host_call` string
/// contract is preserved for existing callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveUiError {
    /// Error kind (never lost — the probe relies on it).
    pub kind: InteractiveUiErrorKind,
    /// Human-readable detail.
    pub message: String,
}

impl InteractiveUiError {
    /// Build an error from a kind and message.
    pub fn new(kind: InteractiveUiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Build an error from the host wire pair.
    pub fn from_host_error(kind: &str, message: impl Into<String>) -> Self {
        Self::new(InteractiveUiErrorKind::from_wire(kind), message)
    }

    /// `invalidRequest` helper.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(InteractiveUiErrorKind::InvalidRequest, message)
    }

    /// `protocolError` helper (wire/transport failures).
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(InteractiveUiErrorKind::ProtocolError, message)
    }

    /// `call` / `componentAlreadyMounted` (design §2.4; R-U1.6).
    pub fn component_already_mounted() -> Self {
        Self::new(
            InteractiveUiErrorKind::Call,
            "componentAlreadyMounted: this extension already has an active component",
        )
    }

    /// `invalidRequest` / `unknownHandle` (design §2.4).
    pub fn unknown_handle(handle: ComponentHandle) -> Self {
        Self::invalid_request(format!("unknownHandle: {}", handle.0))
    }

    /// `invalidRequest` / `frameTooLarge` (design §2.4; R-U3.5).
    pub fn frame_too_large(detail: impl fmt::Display) -> Self {
        Self::invalid_request(format!("frameTooLarge: {detail}"))
    }

    /// Whether this is the "host does not implement the method" signal.
    pub fn is_unknown_method(&self) -> bool {
        self.kind == InteractiveUiErrorKind::UnknownMethod
    }

    /// Parse `{"error": {"kind", "message"}}` (or an `{"ok": ...}` envelope).
    pub fn from_envelope(response: &Value) -> Option<Self> {
        let error = response.get("error")?;
        Some(Self::from_host_error(
            error
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("internal"),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("host error"),
        ))
    }
}

impl fmt::Display for InteractiveUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InteractiveUiError {}

// ============================================================================
// Component handle
// ============================================================================

/// Opaque component handle minted by `ui.mountComponent` (JSON number).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ComponentHandle(pub u64);

impl fmt::Display for ComponentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// MountOptions (design §2.2)
// ============================================================================

const fn default_true() -> bool {
    true
}

const fn default_max_frame_bytes() -> usize {
    DEFAULT_MAX_FRAME_BYTES
}

/// `MountOptions` — `ui.mountComponent` payload (design §2.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountOptions {
    /// `true` (default) mounts an overlay; `false` uses the editor region.
    #[serde(default = "default_true")]
    pub overlay: bool,
    /// Overlay geometry; `None` = rpi-tui defaults (center, natural size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_options: Option<OverlayOptions>,
    /// `> 0` asks the host for a periodic `tick` event (R-U5.1).
    #[serde(default)]
    pub tick_ms: u64,
    /// Key ids still routed to a hidden component (R-U2.3).
    #[serde(default)]
    pub keys_when_hidden: Vec<String>,
    /// Host strips `CURSOR_MARKER` / positions the hardware cursor (R-U3.2).
    #[serde(default = "default_true")]
    pub cursor: bool,
    /// Total frame byte budget (R-U3.5).
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
    /// Diagnostic label (host logs, R-U12.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            overlay: true,
            overlay_options: None,
            tick_ms: 0,
            keys_when_hidden: Vec::new(),
            cursor: true,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            label: None,
        }
    }
}

/// Overlay geometry (design §2.2; maps rpi-tui `OverlayOptions`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayOptions {
    /// Anchor point (default `center`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<OverlayAnchor>,
    /// Width in columns or `"N%"` of the terminal width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<SizeValue>,
    /// Minimum width in columns or `"N%"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_width: Option<SizeValue>,
    /// Maximum height in rows or `"N%"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<SizeValue>,
    /// Absolute row or percentage from the top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<SizeValue>,
    /// Absolute column or percentage from the left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<SizeValue>,
    /// Margin from the terminal edges (unset sides default to 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<Margin>,
    /// `true` = do not capture keyboard focus on mount.
    #[serde(default)]
    pub non_capturing: bool,
}

/// `OverlayAnchor` (design §2.2; rpi-tui `tui.rs:693-704`), kebab-case wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayAnchor {
    /// Center of the terminal (default).
    #[default]
    Center,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
    /// Top edge, centered.
    TopCenter,
    /// Bottom edge, centered.
    BottomCenter,
    /// Left edge, centered.
    LeftCenter,
    /// Right edge, centered.
    RightCenter,
}

/// Overlay margin; unset sides default to 0 (rpi-tui `OverlayMargin`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Margin {
    /// Left inset in columns.
    #[serde(default)]
    pub left: i32,
    /// Right inset in columns.
    #[serde(default)]
    pub right: i32,
    /// Bottom inset in rows.
    #[serde(default)]
    pub bottom: i32,
    /// Top inset in rows.
    #[serde(default)]
    pub top: i32,
}

/// Absolute columns/rows or a percentage (`"N%"`), mirroring rpi-tui
/// `SizeValue` (`tui.rs:717-721`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SizeValue {
    /// Absolute columns/rows.
    Absolute(i32),
    /// Percentage of the terminal dimension.
    Percent(f64),
}

impl Serialize for SizeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absolute(value) => serializer.serialize_i32(*value),
            Self::Percent(percent) => serializer.serialize_str(&format_percent(*percent)),
        }
    }
}

fn format_percent(percent: f64) -> String {
    if percent.fract() == 0.0 {
        format!("{}%", percent as i64)
    } else {
        format!("{percent}%")
    }
}

impl<'de> Deserialize<'de> for SizeValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;

        impl<'de> Visitor<'de> for SizeVisitor {
            type Value = SizeValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a column/row count or an \"N%\" string")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                i32::try_from(value)
                    .map(SizeValue::Absolute)
                    .map_err(|_| E::custom(format!("size out of range: {value}")))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                i32::try_from(value)
                    .map(SizeValue::Absolute)
                    .map_err(|_| E::custom(format!("size out of range: {value}")))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                if value.fract() == 0.0 {
                    self.visit_i64(value as i64)
                } else {
                    Err(E::custom(
                        "percentages must be strings (e.g. \"50%\"), not floats",
                    ))
                }
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                let Some(raw) = value.strip_suffix('%') else {
                    return Err(E::custom(format!(
                        "expected an \"N%\" string, got \"{value}\""
                    )));
                };
                raw.trim()
                    .parse::<f64>()
                    .map(SizeValue::Percent)
                    .map_err(|_| E::custom(format!("invalid percentage: \"{value}\"")))
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

// ============================================================================
// ComponentEvent (design §2.2, R-U1.2 / R-U2 / R-U5)
// ============================================================================

/// `ui.pollComponent` result: the host→guest event enumeration (9 types).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ComponentEvent {
    /// Mount/resize: `{width, height}` are the content area dimensions.
    Resize {
        /// Content width in columns.
        width: usize,
        /// Content height in rows.
        height: usize,
    },
    /// Raw key bytes (`data` keeps CSI/SS3/Kitty sequences verbatim, R-U2.1).
    Input {
        /// Raw terminal input bytes.
        data: String,
    },
    /// Component gained focus (R-U2.2).
    Focus,
    /// Component lost focus (host dialog opened, R-U2.2).
    Blur,
    /// Host tick (`tickMs > 0`; paused while hidden, R-U5.1).
    Tick,
    /// Theme changed; `theme` is the same JSON as `ui.theme` (R-U3.4).
    Theme {
        /// Theme JSON.
        theme: Value,
    },
    /// Visibility echo after `setComponentHidden` (R-U4.4).
    Visibility {
        /// `true` = now hidden.
        hidden: bool,
    },
    /// `ui.wakeComponent` fired (R-U5.2).
    Render,
    /// Host-forced unmount; `reason` is one of the five `dispose` reasons.
    Dispose {
        /// Why the host is disposing the component.
        reason: DisposeReason,
    },
}

impl ComponentEvent {
    /// The wire `type` string of this event.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Resize { .. } => "resize",
            Self::Input { .. } => "input",
            Self::Focus => "focus",
            Self::Blur => "blur",
            Self::Tick => "tick",
            Self::Theme { .. } => "theme",
            Self::Visibility { .. } => "visibility",
            Self::Render => "render",
            Self::Dispose { .. } => "dispose",
        }
    }

    /// Parse an event JSON object; malformed payloads (including an unknown
    /// `dispose.reason`) become `invalidRequest` instead of panicking.
    pub fn from_json(json: &str) -> Result<Self, InteractiveUiError> {
        serde_json::from_str(json).map_err(|error| {
            InteractiveUiError::invalid_request(format!("componentEvent: {error}"))
        })
    }

    /// Serialize this event to its wire JSON string.
    pub fn to_json(&self) -> Result<String, InteractiveUiError> {
        serde_json::to_string(self)
            .map_err(|error| InteractiveUiError::protocol(format!("componentEvent: {error}")))
    }
}

/// Why the host disposed a component (design §2.2 / R-U6.1), camelCase wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisposeReason {
    /// The tool call that mounted the component was aborted.
    ToolAbort,
    /// Session shutdown.
    SessionShutdown,
    /// Session reload (`/reload`).
    SessionReload,
    /// Extension unload.
    ExtensionUnload,
    /// Host-side grace timeout after an earlier dispose request.
    Timeout,
}

impl DisposeReason {
    /// The five frozen reasons, in design order.
    pub const ALL: [DisposeReason; 5] = [
        Self::ToolAbort,
        Self::SessionShutdown,
        Self::SessionReload,
        Self::ExtensionUnload,
        Self::Timeout,
    ];

    /// The wire string of this reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ToolAbort => "toolAbort",
            Self::SessionShutdown => "sessionShutdown",
            Self::SessionReload => "sessionReload",
            Self::ExtensionUnload => "extensionUnload",
            Self::Timeout => "timeout",
        }
    }

    /// Parse a wire string; unknown values are `invalidRequest` (R-U6.1).
    pub fn parse(value: &str) -> Result<Self, InteractiveUiError> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
            .ok_or_else(|| {
                InteractiveUiError::invalid_request(format!(
                    "dispose.reason: unknown value \"{value}\""
                ))
            })
    }
}

// ============================================================================
// ComponentFrame (design §2.2, R-U1.3 / R-U3)
// ============================================================================

/// A `done` payload that distinguishes "absent" from JSON `null`.
///
/// The protocol ends a component when the `done` key is **present**; its
/// value is recorded verbatim (R-U1.4), so `done: null` (upstream
/// `done(undefined)`) must not collapse into "no done".
#[derive(Clone, Debug, Default, PartialEq)]
pub enum DoneValue {
    /// The key is absent — keep running.
    #[default]
    Absent,
    /// The key is present; the value is the component result (may be null).
    Present(Value),
}

impl DoneValue {
    /// Whether the `done` key is absent.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The done value when present.
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Present(value) => Some(value),
            Self::Absent => None,
        }
    }

    /// Consume into the done value when present.
    pub fn into_value(self) -> Option<Value> {
        match self {
            Self::Present(value) => Some(value),
            Self::Absent => None,
        }
    }
}

impl Serialize for DoneValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for DoneValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Present)
    }
}

/// Explicit hardware-cursor position (0-based row/col).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentCursor {
    /// 0-based row.
    pub row: usize,
    /// 0-based column.
    pub col: usize,
}

/// `ui.renderComponent` frame payload (design §2.2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentFrame {
    /// Rendered lines (ANSI SGR allowed); the host clips/pads to width.
    pub lines: Vec<String>,
    /// Explicit cursor; `None` = scan for [`CURSOR_MARKER`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ComponentCursor>,
    /// Present = finish the component; value recorded verbatim (R-U1.4).
    #[serde(default, skip_serializing_if = "DoneValue::is_absent")]
    pub done: DoneValue,
}

impl ComponentFrame {
    /// A frame with only lines and no cursor/done.
    pub fn lines(lines: Vec<String>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }

    /// Mark the frame as the final one with the given result.
    pub fn with_done(mut self, value: Value) -> Self {
        self.done = DoneValue::Present(value);
        self
    }

    /// Whether this frame terminates the component.
    pub fn is_done(&self) -> bool {
        !self.done.is_absent()
    }

    /// Parse a frame JSON object; malformed payloads are `invalidRequest`.
    pub fn from_json(json: &str) -> Result<Self, InteractiveUiError> {
        serde_json::from_str(json).map_err(|error| {
            InteractiveUiError::invalid_request(format!("componentFrame: {error}"))
        })
    }

    /// Serialize this frame to its wire JSON string.
    pub fn to_json(&self) -> Result<String, InteractiveUiError> {
        serde_json::to_string(self)
            .map_err(|error| InteractiveUiError::protocol(format!("componentFrame: {error}")))
    }
}

// ============================================================================
// Lightweight line / ANSI builders (V14-22 C2; R-U7.2 / R-U9.4)
// ============================================================================
//
// wasm guests cannot link `rpi-tui` (the carrier matrix marks it ❌, design
// §4.4), so the SDK ships a dependency-free line assembler: plain text with
// SGR styling that the host composites verbatim. The helpers live in both
// protocol mirrors and behave identically, so guest rendering code can move
// between carriers unchanged.

/// SGR colour value for [`AnsiStyle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsiColor {
    /// 16-colour code (`0..=7` → `30..=37`/`40..=47`; `8..=15` →
    /// `90..=97`/`100..=107`; values are taken modulo 16).
    Basic(u8),
    /// 256-colour palette index (`38;5;N` / `48;5;N`).
    Indexed(u8),
    /// Truecolor (`38;2;R;G;B` / `48;2;R;G;B`).
    Rgb(u8, u8, u8),
}

/// Text style for [`AnsiStyle::apply`] / [`LineBuilder`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnsiStyle {
    /// Bold (`SGR 1`).
    pub bold: bool,
    /// Dim (`SGR 2`).
    pub dim: bool,
    /// Italic (`SGR 3`).
    pub italic: bool,
    /// Underline (`SGR 4`).
    pub underline: bool,
    /// Foreground colour.
    pub fg: Option<AnsiColor>,
    /// Background colour.
    pub bg: Option<AnsiColor>,
}

impl AnsiStyle {
    /// A plain style (no attributes, no colours).
    pub const fn new() -> Self {
        Self {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            fg: None,
            bg: None,
        }
    }

    /// Builder: enable bold.
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    /// Builder: enable dim.
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    /// Builder: enable italic.
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Builder: enable underline.
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Builder: set the foreground colour.
    pub const fn fg(mut self, color: AnsiColor) -> Self {
        self.fg = Some(color);
        self
    }

    /// Builder: set the background colour.
    pub const fn bg(mut self, color: AnsiColor) -> Self {
        self.bg = Some(color);
        self
    }

    /// Whether this style emits no SGR sequence at all.
    pub const fn is_plain(&self) -> bool {
        !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && self.fg.is_none()
            && self.bg.is_none()
    }

    /// The SGR prefix for this style (e.g. `"\x1b[1;38;5;196m"`); empty for
    /// a plain style.
    pub fn sgr(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.bold {
            parts.push("1".to_owned());
        }
        if self.dim {
            parts.push("2".to_owned());
        }
        if self.italic {
            parts.push("3".to_owned());
        }
        if self.underline {
            parts.push("4".to_owned());
        }
        if let Some(color) = self.fg {
            parts.extend(color_sgr(color, false));
        }
        if let Some(color) = self.bg {
            parts.extend(color_sgr(color, true));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", parts.join(";"))
        }
    }

    /// Wrap `text` in this style's SGR prefix and a reset suffix; a plain
    /// style returns `text` unchanged.
    pub fn apply(&self, text: &str) -> String {
        let sgr = self.sgr();
        if sgr.is_empty() {
            text.to_owned()
        } else {
            format!("{sgr}{text}\x1b[0m")
        }
    }
}

/// The SGR parameter list for a colour (`38;5;N` / `48;2;R;G;B` / basic).
fn color_sgr(color: AnsiColor, background: bool) -> Vec<String> {
    match color {
        AnsiColor::Basic(value) => {
            let value = value % 16;
            let code = if value < 8 {
                (if background { 40 } else { 30 }) + value
            } else {
                (if background { 100 } else { 90 }) + (value - 8)
            };
            vec![code.to_string()]
        }
        AnsiColor::Indexed(index) => {
            vec![
                (if background { "48" } else { "38" }).to_owned(),
                "5".to_owned(),
                index.to_string(),
            ]
        }
        AnsiColor::Rgb(red, green, blue) => {
            vec![
                (if background { "48" } else { "38" }).to_owned(),
                "2".to_owned(),
                red.to_string(),
                green.to_string(),
                blue.to_string(),
            ]
        }
    }
}

/// Line assembler: concatenates styled spans into one frame line without
/// linking `rpi-tui` (R-U7.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineBuilder {
    spans: Vec<(String, AnsiStyle)>,
}

impl LineBuilder {
    /// An empty line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `text` with `style`.
    pub fn push(&mut self, text: impl Into<String>, style: AnsiStyle) -> &mut Self {
        self.spans.push((text.into(), style));
        self
    }

    /// Append plain (unstyled) `text`.
    pub fn plain(&mut self, text: impl Into<String>) -> &mut Self {
        self.push(text, AnsiStyle::new())
    }

    /// Number of spans appended so far.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether no span was appended.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Build the final line (SGR sequences included).
    pub fn build(&self) -> String {
        let mut line = String::new();
        for (text, style) in &self.spans {
            line.push_str(&style.apply(text));
        }
        line
    }
}

impl std::fmt::Display for LineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.build())
    }
}

// ============================================================================
// Lifecycle states (design §2.3)
// ============================================================================

/// Component lifecycle state (design §2.3). C0 freezes the state set; the
/// host runtime that drives it lands with C1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentState {
    /// Mounted and focused: all keys route as `input`, frames render, ticks run.
    Mounted,
    /// Blurred by a host dialog: no keys, frames/ticks continue.
    Blurred,
    /// Hidden: only `keysWhenHidden` route, no render, tick paused.
    Hidden,
    /// `dispose` delivered: no keys, last frame, bounded grace.
    Disposing,
    /// Unmounted: handle is dead, further calls are `invalidRequest`.
    Closed,
}

impl ComponentState {
    /// Terminal states reject every further call on the handle
    /// (`unknownHandle` / R-U1.4 / R-U6.4).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Disposing | Self::Closed)
    }
}

// ============================================================================
// Host-call argument payloads (design §2.1)
// ============================================================================

/// `ui.mountComponent` args.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountComponentArgs {
    /// Mount options.
    pub options: MountOptions,
}

/// `ui.pollComponent` args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollComponentArgs {
    /// Target handle.
    pub handle: ComponentHandle,
}

/// `ui.renderComponent` args: `{handle, lines, cursor?, done?}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderComponentArgs {
    /// Target handle.
    pub handle: ComponentHandle,
    /// Frame payload, flattened next to `handle`.
    #[serde(flatten)]
    pub frame: ComponentFrame,
}

/// `ui.setComponentHidden` args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetComponentHiddenArgs {
    /// Target handle.
    pub handle: ComponentHandle,
    /// `true` = hide, `false` = show.
    pub hidden: bool,
}

/// `ui.wakeComponent` args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeComponentArgs {
    /// Target handle.
    pub handle: ComponentHandle,
}

/// `ui.disposeComponent` args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisposeComponentArgs {
    /// Target handle.
    pub handle: ComponentHandle,
}

/// `ui.editExternal` args (P1; lands with C3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditExternalArgs {
    /// Text to edit.
    pub text: String,
    /// Optional language hint for the external editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

// ============================================================================
// Host-call transport (same shape as the native helper)
// ============================================================================

/// Guest → host JSON call transport.
///
/// Native plugins use [`NativeHostCall`] (the abi_stable `RpiHostCalls`
/// trampoline); the wasm SDK uses `rpi_ext_sdk::interactive_ui::RpiHost`.
pub trait HostCall {
    /// Send `{"call": method, "args": args}` and return the `ok` payload or
    /// the structured host error.
    fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError>;
}

/// Native (L0) transport over the abi_stable `RpiHostCalls` trampoline.
///
/// `call`/`cookie` come from `rpi_extension_init`; keep them for the plugin
/// lifetime (the host cookie must outlive every dispatch).
#[derive(Clone, Copy)]
pub struct NativeHostCall {
    /// The `RpiHostCalls::call` function pointer.
    pub call: extern "C" fn(
        crate::native::PluginCookie,
        abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8>,
    /// The host-minted plugin cookie.
    pub cookie: crate::native::PluginCookie,
}

impl NativeHostCall {
    /// Build a transport from the init-time handles.
    pub fn new(
        call: extern "C" fn(
            crate::native::PluginCookie,
            abi_stable::std_types::RVec<u8>,
        ) -> abi_stable::std_types::RVec<u8>,
        cookie: crate::native::PluginCookie,
    ) -> Self {
        Self { call, cookie }
    }
}

impl HostCall for NativeHostCall {
    fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
        let request = serde_json::to_vec(&serde_json::json!({
            "call": method,
            "args": args,
            "seq": 0,
        }))
        .map_err(|error| InteractiveUiError::protocol(format!("host request JSON: {error}")))?;
        let response_bytes = (self.call)(self.cookie, abi_stable::std_types::RVec::from(request));
        let response: Value = serde_json::from_slice(&response_bytes[..]).map_err(|error| {
            InteractiveUiError::protocol(format!("host response JSON: {error}"))
        })?;
        match InteractiveUiError::from_envelope(&response) {
            Some(error) => Err(error),
            None => Ok(response.get("ok").cloned().unwrap_or(Value::Null)),
        }
    }
}

// ============================================================================
// Protocol helpers (thin typed wrappers over HostCall)
// ============================================================================

fn host_ok<H: HostCall + ?Sized>(
    host: &H,
    method: &str,
    args: Value,
) -> Result<Value, InteractiveUiError> {
    host.call(method, args)
}

fn expect_ok<H: HostCall + ?Sized>(
    host: &H,
    method: &str,
    args: Value,
) -> Result<(), InteractiveUiError> {
    host_ok(host, method, args).map(|_| ())
}

/// `ui.mountComponent` → handle.
pub fn mount_component<H: HostCall + ?Sized>(
    host: &H,
    options: &MountOptions,
) -> Result<ComponentHandle, InteractiveUiError> {
    let response = host_ok(
        host,
        METHOD_MOUNT_COMPONENT,
        serde_json::to_value(MountComponentArgs {
            options: options.clone(),
        })
        .map_err(|error| InteractiveUiError::protocol(format!("mountComponent args: {error}")))?,
    )?;
    match response.get("handle").cloned() {
        Some(handle) => serde_json::from_value(handle).map_err(|error| {
            InteractiveUiError::invalid_request(format!("mountComponent handle: {error}"))
        }),
        None => Err(InteractiveUiError::invalid_request(
            "mountComponent: response is missing \"handle\"",
        )),
    }
}

/// `ui.pollComponent` → next event (blocking on the host side).
pub fn poll_component<H: HostCall + ?Sized>(
    host: &H,
    handle: ComponentHandle,
) -> Result<ComponentEvent, InteractiveUiError> {
    let response = host_ok(
        host,
        METHOD_POLL_COMPONENT,
        serde_json::to_value(PollComponentArgs { handle }).map_err(|error| {
            InteractiveUiError::protocol(format!("pollComponent args: {error}"))
        })?,
    )?;
    match response.get("event").cloned() {
        Some(event) => serde_json::from_value(event).map_err(|error| {
            InteractiveUiError::invalid_request(format!("pollComponent event: {error}"))
        }),
        None => Err(InteractiveUiError::invalid_request(
            "pollComponent: response is missing \"event\"",
        )),
    }
}

/// `ui.renderComponent` → `()` (`{ok:true}`).
pub fn render_component<H: HostCall + ?Sized>(
    host: &H,
    handle: ComponentHandle,
    frame: &ComponentFrame,
) -> Result<(), InteractiveUiError> {
    expect_ok(
        host,
        METHOD_RENDER_COMPONENT,
        serde_json::to_value(RenderComponentArgs {
            handle,
            frame: frame.clone(),
        })
        .map_err(|error| InteractiveUiError::protocol(format!("renderComponent args: {error}")))?,
    )
}

/// `ui.setComponentHidden` → `()` (`{ok:true}`).
pub fn set_component_hidden<H: HostCall + ?Sized>(
    host: &H,
    handle: ComponentHandle,
    hidden: bool,
) -> Result<(), InteractiveUiError> {
    expect_ok(
        host,
        METHOD_SET_COMPONENT_HIDDEN,
        serde_json::to_value(SetComponentHiddenArgs { handle, hidden }).map_err(|error| {
            InteractiveUiError::protocol(format!("setComponentHidden args: {error}"))
        })?,
    )
}

/// `ui.wakeComponent` → `()` (`{ok:true}`).
pub fn wake_component<H: HostCall + ?Sized>(
    host: &H,
    handle: ComponentHandle,
) -> Result<(), InteractiveUiError> {
    expect_ok(
        host,
        METHOD_WAKE_COMPONENT,
        serde_json::to_value(WakeComponentArgs { handle }).map_err(|error| {
            InteractiveUiError::protocol(format!("wakeComponent args: {error}"))
        })?,
    )
}

/// `ui.disposeComponent` → `()` (`{ok:true}`).
pub fn dispose_component<H: HostCall + ?Sized>(
    host: &H,
    handle: ComponentHandle,
) -> Result<(), InteractiveUiError> {
    expect_ok(
        host,
        METHOD_DISPOSE_COMPONENT,
        serde_json::to_value(DisposeComponentArgs { handle }).map_err(|error| {
            InteractiveUiError::protocol(format!("disposeComponent args: {error}"))
        })?,
    )
}

/// `ui.editExternal` → edited text, or `None` when the user cancelled
/// (P1; the host returns `unknownMethod` until C3).
pub fn edit_external<H: HostCall + ?Sized>(
    host: &H,
    text: &str,
    language: Option<&str>,
) -> Result<Option<String>, InteractiveUiError> {
    let response = host_ok(
        host,
        METHOD_EDIT_EXTERNAL,
        serde_json::to_value(EditExternalArgs {
            text: text.to_owned(),
            language: language.map(str::to_owned),
        })
        .map_err(|error| InteractiveUiError::protocol(format!("editExternal args: {error}")))?,
    )?;
    match response.get("text").cloned() {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(other) => Err(InteractiveUiError::invalid_request(format!(
            "editExternal: text must be a string or null, got {other}"
        ))),
    }
}

// ============================================================================
// Capability probe (R-U9.2)
// ============================================================================

/// Probe whether the host implements the interactive UI ABI (R-U9.2).
///
/// Sends [`METHOD_MOUNT_COMPONENT`] with an empty argument object: a C1/C2
/// host validates args **before** creating any state and answers
/// `invalidRequest`, so the probe never mounts a component. Outcomes:
///
/// | host answer | result |
/// |---|---|
/// | `unknownMethod` (C0/old host) | `Ok(false)` |
/// | `invalidRequest` / `call` (method exists) | `Ok(true)` |
/// | success | `Ok(true)` |
/// | `capabilityDenied` / transport / internal | `Err` (never misreported as unsupported) |
///
/// The conclusion is never cached across sessions (design §3.2).
pub fn supports_interactive_ui<H: HostCall + ?Sized>(host: &H) -> Result<bool, InteractiveUiError> {
    probe_interactive_ui_method(host, METHOD_MOUNT_COMPONENT)
}

/// [`supports_interactive_ui`] with an explicit probe method (the C0 probe
/// method choice is re-confirmed in C1; see V14-20 §8).
pub fn probe_interactive_ui_method<H: HostCall + ?Sized>(
    host: &H,
    method: &str,
) -> Result<bool, InteractiveUiError> {
    match host.call(method, Value::Object(Default::default())) {
        Ok(_) => Ok(true),
        Err(error) => match error.kind {
            InteractiveUiErrorKind::UnknownMethod => Ok(false),
            InteractiveUiErrorKind::InvalidRequest | InteractiveUiErrorKind::Call => Ok(true),
            _ => Err(error),
        },
    }
}

// ============================================================================
// Guest Component trait + run loop (design §5)
// ============================================================================

/// Guest component contract (design §5), mirroring upstream
/// `Component.render(width)` / `handleInput(data)` plus the optional event
/// callbacks of the line-frame protocol.
///
/// The wasm carrier (`rpi_ext_sdk::interactive_ui::Component`) freezes the
/// identical method set (G11 item 7).
pub trait Component {
    /// Render the current state to lines (ANSI SGR allowed).
    fn render(&mut self, width: usize) -> Vec<String>;
    /// Raw key bytes for the focused component (R-U2.1).
    fn handle_input(&mut self, data: &str);
    /// Component gained focus.
    fn on_focus(&mut self) {}
    /// Component lost focus to a host dialog.
    fn on_blur(&mut self) {}
    /// Explicit hardware-cursor position for the next frame (R-U3.2).
    ///
    /// `None` (default) leaves cursor placement to a
    /// [`CURSOR_MARKER`] embedded in [`Self::render`]; `Some` is the
    /// equivalent of the frame's `cursor:{row,col}` field and wins over an
    /// embedded marker.
    fn cursor(&self) -> Option<ComponentCursor> {
        None
    }

    /// Host tick (R-U5.1).
    fn on_tick(&mut self) {}
    /// Resize to the content area dimensions (R-U3.3).
    fn on_resize(&mut self, _width: usize, _height: usize) {}
    /// Theme changed; `theme` is the same JSON as `ui.theme`.
    fn on_theme(&mut self, _theme: &Value) {}
    /// Visibility echo (R-U4.4).
    fn on_visibility(&mut self, _hidden: bool) {}
    /// `ui.wakeComponent` fired.
    fn on_render(&mut self) {}
    /// Host-forced unmount (R-U6.1).
    fn on_dispose(&mut self, _reason: DisposeReason) {}
    /// `done(result)` equivalent: `Some(value)` (including `Value::Null`)
    /// finishes the component; `None` keeps it running.
    fn done(&mut self) -> Option<Value> {
        None
    }
}

/// Drive a component through the mount/poll/render loop (design §1.1).
///
/// Returns the `done` value (verbatim, may be `Value::Null`). A host
/// `dispose` submits a final best-effort frame and returns `Value::Null`.
/// On a C0 host the very first `mountComponent` fails with `unknownMethod`;
/// callers should branch on [`supports_interactive_ui`] first (R-Q6.2).
pub fn run_component<C: Component, H: HostCall + ?Sized>(
    host: &H,
    mut component: C,
    options: MountOptions,
) -> Result<Value, InteractiveUiError> {
    let handle = mount_component(host, &options)?;
    let mut width = 0usize;
    loop {
        let event = poll_component(host, handle)?;
        match &event {
            ComponentEvent::Resize {
                width: event_width,
                height,
            } => {
                width = *event_width;
                component.on_resize(*event_width, *height);
            }
            ComponentEvent::Input { data } => component.handle_input(data),
            ComponentEvent::Focus => component.on_focus(),
            ComponentEvent::Blur => component.on_blur(),
            ComponentEvent::Tick => component.on_tick(),
            ComponentEvent::Theme { theme } => component.on_theme(theme),
            ComponentEvent::Visibility { hidden } => component.on_visibility(*hidden),
            ComponentEvent::Render => component.on_render(),
            ComponentEvent::Dispose { reason } => {
                component.on_dispose(*reason);
                let frame = ComponentFrame {
                    lines: component.render(width),
                    cursor: component.cursor(),
                    done: component
                        .done()
                        .map_or(DoneValue::Absent, DoneValue::Present),
                };
                // Final frame after dispose is best-effort (bounded grace).
                let _ = render_component(host, handle, &frame);
                return Ok(frame.done.into_value().unwrap_or(Value::Null));
            }
        }
        let frame = ComponentFrame {
            lines: component.render(width),
            cursor: component.cursor(),
            done: component
                .done()
                .map_or(DoneValue::Absent, DoneValue::Present),
        };
        let finished = frame.is_done();
        render_component(host, handle, &frame)?;
        if finished {
            return Ok(frame.done.into_value().unwrap_or(Value::Null));
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted fake transport: records calls, replays queued responses.
    struct FakeHost {
        calls: Mutex<Vec<(String, Value)>>,
        responses: Mutex<VecDeque<Result<Value, InteractiveUiError>>>,
    }

    impl FakeHost {
        fn new(responses: Vec<Result<Value, InteractiveUiError>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HostCall for FakeHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), args.clone()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(InteractiveUiError::protocol("no scripted response")))
        }
    }

    fn unknown_method() -> InteractiveUiError {
        InteractiveUiError::from_host_error("unknownMethod", "unknown host call: ui.mountComponent")
    }

    #[test]
    fn interactive_ui_method_table_is_frozen() {
        assert_eq!(INTERACTIVE_UI_METHODS.len(), 7);
        for method in INTERACTIVE_UI_METHODS {
            assert!(method.starts_with("ui."), "{method}");
            assert!(is_interactive_ui_method(method), "{method}");
        }
        assert!(!is_interactive_ui_method("ui.custom"));
        assert!(!is_interactive_ui_method("ui.onTerminalInput"));
    }

    #[test]
    fn interactive_ui_mount_options_defaults_and_round_trip() {
        let options = MountOptions::default();
        assert!(options.overlay);
        assert_eq!(options.tick_ms, 0);
        assert!(options.cursor);
        assert_eq!(options.max_frame_bytes, 1_048_576);
        assert!(options.keys_when_hidden.is_empty());
        assert_eq!(options.overlay_options, None);

        let json = serde_json::to_string(&options).unwrap();
        assert_eq!(
            json,
            r#"{"overlay":true,"tickMs":0,"keysWhenHidden":[],"cursor":true,"maxFrameBytes":1048576}"#
        );
        let parsed: MountOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, options);

        // Missing fields take the frozen defaults; unknown fields are ignored
        // (additive evolution).
        let parsed: MountOptions =
            serde_json::from_str(r#"{"label":"ask_user_question","futureField":1}"#).unwrap();
        assert_eq!(parsed.label.as_deref(), Some("ask_user_question"));
        assert!(parsed.overlay);
        assert_eq!(parsed.max_frame_bytes, DEFAULT_MAX_FRAME_BYTES);
    }

    #[test]
    fn interactive_ui_overlay_options_absolute_and_percent() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::BottomCenter),
            width: Some(SizeValue::Percent(100.0)),
            min_width: Some(SizeValue::Absolute(40)),
            max_height: Some(SizeValue::Percent(100.0)),
            row: Some(SizeValue::Percent(25.0)),
            col: Some(SizeValue::Absolute(3)),
            margin: Some(Margin {
                left: 0,
                right: 0,
                bottom: 0,
                top: 0,
            }),
            non_capturing: false,
        };
        let json = serde_json::to_string(&options).unwrap();
        assert_eq!(
            json,
            r#"{"anchor":"bottom-center","width":"100%","minWidth":40,"maxHeight":"100%","row":"25%","col":3,"margin":{"left":0,"right":0,"bottom":0,"top":0},"nonCapturing":false}"#
        );
        let parsed: OverlayOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, options);

        // Percent with a fractional value keeps its precision.
        assert_eq!(
            serde_json::to_string(&SizeValue::Percent(12.5)).unwrap(),
            r#""12.5%""#
        );
        // A bare string without '%' and a fractional float are rejected.
        assert!(serde_json::from_str::<SizeValue>(r#""40""#).is_err());
        assert!(serde_json::from_str::<SizeValue>("1.5").is_err());
    }

    #[test]
    fn interactive_ui_component_events_round_trip_byte_exact() {
        let cases: [(&str, ComponentEvent); 9] = [
            (
                r#"{"type":"resize","width":80,"height":24}"#,
                ComponentEvent::Resize {
                    width: 80,
                    height: 24,
                },
            ),
            (
                "{\"type\":\"input\",\"data\":\"\\u001b[B\"}",
                ComponentEvent::Input {
                    data: "\u{1b}[B".to_owned(),
                },
            ),
            (r#"{"type":"focus"}"#, ComponentEvent::Focus),
            (r#"{"type":"blur"}"#, ComponentEvent::Blur),
            (r#"{"type":"tick"}"#, ComponentEvent::Tick),
            (
                r#"{"type":"theme","theme":{"name":"dark"}}"#,
                ComponentEvent::Theme {
                    theme: serde_json::json!({"name": "dark"}),
                },
            ),
            (
                r#"{"type":"visibility","hidden":true}"#,
                ComponentEvent::Visibility { hidden: true },
            ),
            (r#"{"type":"render"}"#, ComponentEvent::Render),
            (
                r#"{"type":"dispose","reason":"toolAbort"}"#,
                ComponentEvent::Dispose {
                    reason: DisposeReason::ToolAbort,
                },
            ),
        ];
        for (json, event) in cases {
            assert_eq!(event.to_json().unwrap(), json, "{json}");
            assert_eq!(ComponentEvent::from_json(json).unwrap(), event, "{json}");
        }

        // Raw CSI/SS3/Kitty bytes survive verbatim (no key-name normalization).
        let kitty = ComponentEvent::Input {
            data: "\u{1b}[97;5u".to_owned(),
        };
        let json = kitty.to_json().unwrap();
        assert!(json.contains("\\u001b[97;5u"), "{json}");
        assert_eq!(ComponentEvent::from_json(&json).unwrap(), kitty);

        // Unknown event type / malformed JSON / bad dispose reason are
        // invalidRequest, never a panic.
        for bad in [
            r#"{"type":"mouse","row":1}"#,
            r#"{"type":"dispose","reason":"nope"}"#,
            r#"{"type":"input"}"#,
            "{",
        ] {
            let error = ComponentEvent::from_json(bad).unwrap_err();
            assert_eq!(error.kind, InteractiveUiErrorKind::InvalidRequest, "{bad}");
        }
    }

    #[test]
    fn interactive_ui_dispose_reason_five_values() {
        for reason in DisposeReason::ALL {
            assert_eq!(DisposeReason::parse(reason.as_str()).unwrap(), reason);
        }
        let error = DisposeReason::parse("vanished").unwrap_err();
        assert_eq!(error.kind, InteractiveUiErrorKind::InvalidRequest);
        assert!(error.message.contains("vanished"), "{error}");
    }

    #[test]
    fn interactive_ui_component_frame_cursor_done_and_marker() {
        // Explicit cursor.
        let frame = ComponentFrame {
            lines: vec!["hello".to_owned()],
            cursor: Some(ComponentCursor { row: 3, col: 7 }),
            done: DoneValue::Absent,
        };
        let json = frame.to_json().unwrap();
        assert_eq!(json, r#"{"lines":["hello"],"cursor":{"row":3,"col":7}}"#);
        assert_eq!(ComponentFrame::from_json(&json).unwrap(), frame);

        // CURSOR_MARKER inside a line survives verbatim (host strips it).
        let marker_frame = ComponentFrame::lines(vec![format!("abc{CURSOR_MARKER}def")]);
        let json = marker_frame.to_json().unwrap();
        assert!(json.contains("\\u001b_pi:c\\u0007"), "{json}");
        assert_eq!(ComponentFrame::from_json(&json).unwrap(), marker_frame);

        // `done` present (including null) vs absent.
        let done = ComponentFrame::lines(vec!["x".to_owned()])
            .with_done(serde_json::json!({"answers": [1, 2]}));
        let json = done.to_json().unwrap();
        assert_eq!(json, r#"{"lines":["x"],"done":{"answers":[1,2]}}"#);
        assert_eq!(ComponentFrame::from_json(&json).unwrap(), done);

        let null_done = ComponentFrame::lines(vec!["x".to_owned()]).with_done(Value::Null);
        let json = null_done.to_json().unwrap();
        assert_eq!(json, r#"{"lines":["x"],"done":null}"#);
        let parsed = ComponentFrame::from_json(&json).unwrap();
        assert!(parsed.is_done());
        assert_eq!(parsed.done.value(), Some(&Value::Null));

        let plain = ComponentFrame::from_json(r#"{"lines":["x"]}"#).unwrap();
        assert!(!plain.is_done());
        assert_eq!(plain.done, DoneValue::Absent);
    }

    #[test]
    fn interactive_ui_error_kinds_and_semantic_helpers() {
        assert_eq!(
            InteractiveUiError::component_already_mounted().kind,
            InteractiveUiErrorKind::Call
        );
        assert_eq!(
            InteractiveUiError::unknown_handle(ComponentHandle(9)).kind,
            InteractiveUiErrorKind::InvalidRequest
        );
        assert_eq!(
            InteractiveUiError::frame_too_large("rows 5001 > 5000").kind,
            InteractiveUiErrorKind::InvalidRequest
        );

        for kind in [
            InteractiveUiErrorKind::CapabilityDenied,
            InteractiveUiErrorKind::InvalidRequest,
            InteractiveUiErrorKind::UnknownMethod,
            InteractiveUiErrorKind::Call,
            InteractiveUiErrorKind::Internal,
            InteractiveUiErrorKind::HandlerError,
            InteractiveUiErrorKind::FuelExhausted,
            InteractiveUiErrorKind::ProtocolError,
            InteractiveUiErrorKind::Other("futureKind".to_owned()),
        ] {
            assert_eq!(InteractiveUiErrorKind::from_wire(kind.as_str()), kind);
        }

        let envelope = serde_json::json!({
            "error": {"kind": "unknownMethod", "message": "no such method"}
        });
        let error = InteractiveUiError::from_envelope(&envelope).unwrap();
        assert!(error.is_unknown_method());
        assert_eq!(error.to_string(), "no such method");
        assert!(InteractiveUiError::from_envelope(&serde_json::json!({"ok": true})).is_none());
    }

    #[test]
    fn interactive_ui_state_machine_terminal_states() {
        assert!(!ComponentState::Mounted.is_terminal());
        assert!(!ComponentState::Blurred.is_terminal());
        assert!(!ComponentState::Hidden.is_terminal());
        assert!(ComponentState::Disposing.is_terminal());
        assert!(ComponentState::Closed.is_terminal());
    }

    #[test]
    fn interactive_ui_probe_matrix() {
        // C0 / old host: unknownMethod => unsupported.
        let host = FakeHost::new(vec![Err(unknown_method())]);
        assert!(!supports_interactive_ui(&host).unwrap());
        assert_eq!(host.calls()[0].0, METHOD_MOUNT_COMPONENT);
        assert_eq!(host.calls()[0].1, serde_json::json!({}));

        // C1 host rejecting the empty probe args: method exists => supported.
        let host = FakeHost::new(vec![Err(InteractiveUiError::invalid_request(
            "mountComponent: missing options",
        ))]);
        assert!(supports_interactive_ui(&host).unwrap());

        // Success also means supported.
        let host = FakeHost::new(vec![Ok(serde_json::json!({"handle": 1}))]);
        assert!(supports_interactive_ui(&host).unwrap());

        // Capability denied and transport errors propagate (never false).
        let host = FakeHost::new(vec![Err(InteractiveUiError::from_host_error(
            "capabilityDenied",
            "requires ui",
        ))]);
        let error = supports_interactive_ui(&host).unwrap_err();
        assert_eq!(error.kind, InteractiveUiErrorKind::CapabilityDenied);

        let host = FakeHost::new(vec![Err(InteractiveUiError::protocol("transport down"))]);
        let error = supports_interactive_ui(&host).unwrap_err();
        assert_eq!(error.kind, InteractiveUiErrorKind::ProtocolError);
    }

    #[test]
    fn interactive_ui_typed_helpers_shape_calls() {
        let host = FakeHost::new(vec![
            Ok(serde_json::json!({"handle": 42})),
            Ok(serde_json::json!({"event": {"type": "tick"}})),
            Ok(serde_json::json!({"ok": true})),
            Ok(serde_json::json!({"ok": true})),
            Ok(serde_json::json!({"ok": true})),
            Ok(serde_json::json!({"ok": true})),
            Ok(serde_json::json!({"text": "edited"})),
            Ok(serde_json::json!({"text": null})),
        ]);
        let handle = mount_component(&host, &MountOptions::default()).unwrap();
        assert_eq!(handle, ComponentHandle(42));
        assert_eq!(poll_component(&host, handle).unwrap(), ComponentEvent::Tick);
        let frame = ComponentFrame::lines(vec!["line".to_owned()]);
        render_component(&host, handle, &frame).unwrap();
        set_component_hidden(&host, handle, true).unwrap();
        wake_component(&host, handle).unwrap();
        dispose_component(&host, handle).unwrap();
        assert_eq!(
            edit_external(&host, "draft", Some("markdown")).unwrap(),
            Some("edited".to_owned())
        );
        assert_eq!(edit_external(&host, "draft", None).unwrap(), None);

        let calls = host.calls();
        assert_eq!(calls[0].0, METHOD_MOUNT_COMPONENT);
        assert_eq!(
            calls[0].1,
            serde_json::json!({"options": serde_json::to_value(MountOptions::default()).unwrap()})
        );
        assert_eq!(calls[1].1, serde_json::json!({"handle": 42}));
        assert_eq!(
            calls[2].1,
            serde_json::json!({"handle": 42, "lines": ["line"]})
        );
        assert_eq!(
            calls[3].1,
            serde_json::json!({"handle": 42, "hidden": true})
        );
        assert_eq!(
            calls[6].1,
            serde_json::json!({"text": "draft", "language": "markdown"})
        );
        assert_eq!(calls[7].1, serde_json::json!({"text": "draft"}));

        // Unknown handle / missing fields surface as invalidRequest.
        let host = FakeHost::new(vec![Err(InteractiveUiError::unknown_handle(
            ComponentHandle(7),
        ))]);
        let error = poll_component(&host, ComponentHandle(7)).unwrap_err();
        assert_eq!(error.kind, InteractiveUiErrorKind::InvalidRequest);
        let host = FakeHost::new(vec![Ok(serde_json::json!({"ok": true}))]);
        assert!(mount_component(&host, &MountOptions::default()).is_err());
    }

    /// Minimal component for the loop test.
    struct ScriptedComponent {
        width: usize,
        frames: Vec<Vec<String>>,
        events: Vec<&'static str>,
        done_after: usize,
        ticks: usize,
    }

    impl ScriptedComponent {
        fn new(done_after: usize) -> Self {
            Self {
                width: 0,
                frames: Vec::new(),
                events: Vec::new(),
                done_after,
                ticks: 0,
            }
        }
    }

    impl Component for ScriptedComponent {
        fn render(&mut self, width: usize) -> Vec<String> {
            let line = format!("w={width} events={}", self.events.len());
            self.frames.push(vec![line.clone()]);
            vec![line]
        }

        fn handle_input(&mut self, _data: &str) {
            self.events.push("input");
        }

        fn on_resize(&mut self, width: usize, _height: usize) {
            self.width = width;
            self.events.push("resize");
        }

        fn on_tick(&mut self) {
            self.ticks += 1;
            self.events.push("tick");
        }

        fn on_dispose(&mut self, _reason: DisposeReason) {
            self.events.push("dispose");
        }

        fn done(&mut self) -> Option<Value> {
            if self.ticks >= self.done_after {
                Some(serde_json::json!({"ticks": self.ticks}))
            } else {
                None
            }
        }
    }

    #[test]
    fn interactive_ui_run_component_loop() {
        let host = FakeHost::new(vec![
            Ok(serde_json::json!({"handle": 7})),
            Ok(serde_json::json!({"event": {"type": "resize", "width": 80, "height": 24}})),
            Ok(serde_json::json!({"ok": true})),
            Ok(serde_json::json!({"event": {"type": "tick"}})),
            Ok(serde_json::json!({"ok": true})),
        ]);
        let component = ScriptedComponent::new(1);
        let result = run_component(&host, component, MountOptions::default()).unwrap();
        assert_eq!(result, serde_json::json!({"ticks": 1}));
        let calls = host.calls();
        assert_eq!(calls[0].0, METHOD_MOUNT_COMPONENT);
        assert_eq!(calls[1].0, METHOD_POLL_COMPONENT);
        assert_eq!(
            calls[2].0, METHOD_RENDER_COMPONENT,
            "render after resize before next poll"
        );
        assert_eq!(
            calls[2].1,
            serde_json::json!({"handle": 7, "lines": ["w=80 events=1"]})
        );
        assert_eq!(calls[3].0, METHOD_POLL_COMPONENT);
        assert_eq!(
            calls[4].1,
            serde_json::json!({
                "handle": 7,
                "lines": ["w=80 events=2"],
                "done": {"ticks": 1},
            })
        );
        assert_eq!(calls.len(), 5);

        // Dispose path: final frame is best-effort, result is null.
        let host = FakeHost::new(vec![
            Ok(serde_json::json!({"handle": 8})),
            Ok(serde_json::json!({
                "event": {"type": "dispose", "reason": "sessionReload"}
            })),
            Err(InteractiveUiError::unknown_handle(ComponentHandle(8))),
        ]);
        let result =
            run_component(&host, ScriptedComponent::new(99), MountOptions::default()).unwrap();
        assert_eq!(result, Value::Null);
        assert_eq!(host.calls()[2].0, METHOD_RENDER_COMPONENT);
    }

    #[test]
    fn interactive_ui_line_builders_and_carrier_limits() {
        // Plain styles are a passthrough (no SGR noise).
        assert_eq!(AnsiStyle::new().sgr(), "");
        assert_eq!(AnsiStyle::new().apply("plain"), "plain");
        assert!(AnsiStyle::new().is_plain());

        // Attribute order is fixed: bold/dim/italic/underline/fg/bg.
        let styled = AnsiStyle::new()
            .bold()
            .underline()
            .fg(AnsiColor::Indexed(196))
            .bg(AnsiColor::Basic(4));
        assert_eq!(styled.sgr(), "\x1b[1;4;38;5;196;44m");
        assert_eq!(styled.apply("x"), "\x1b[1;4;38;5;196;44mx\x1b[0m");

        // Basic colours: 0..=7 normal, 8..=15 bright, modulo 16.
        assert_eq!(AnsiStyle::new().fg(AnsiColor::Basic(2)).sgr(), "\x1b[32m");
        assert_eq!(AnsiStyle::new().bg(AnsiColor::Basic(9)).sgr(), "\x1b[101m");
        assert_eq!(AnsiStyle::new().fg(AnsiColor::Basic(17)).sgr(), "\x1b[31m");
        assert_eq!(
            AnsiStyle::new().fg(AnsiColor::Rgb(1, 2, 3)).sgr(),
            "\x1b[38;2;1;2;3m"
        );
        assert_eq!(
            AnsiStyle::new().bg(AnsiColor::Rgb(1, 2, 3)).sgr(),
            "\x1b[48;2;1;2;3m"
        );

        // LineBuilder concatenates spans in order.
        let mut line = LineBuilder::new();
        assert!(line.is_empty());
        line.plain("ab").push("CD", AnsiStyle::new().bold());
        assert_eq!(line.len(), 2);
        assert_eq!(line.build(), "ab\x1b[1mCD\x1b[0m");
        assert_eq!(line.to_string(), line.build());

        // Carrier limit constants (R-U7.2): wasm is stricter than native.
        assert_eq!(WASM_DEFAULT_MAX_FRAME_BYTES, 512 * 1024);
        assert_eq!(WASM_DEFAULT_MAX_FRAME_ROWS, 2_000);
        const { assert!(WASM_DEFAULT_MAX_FRAME_BYTES < DEFAULT_MAX_FRAME_BYTES) };
        const { assert!(WASM_DEFAULT_MAX_FRAME_ROWS < DEFAULT_MAX_FRAME_ROWS) };
    }

    #[test]
    fn interactive_ui_handle_round_trip() {
        assert_eq!(serde_json::to_string(&ComponentHandle(7)).unwrap(), "7");
        assert_eq!(
            serde_json::from_str::<ComponentHandle>("7").unwrap(),
            ComponentHandle(7)
        );
    }
}
