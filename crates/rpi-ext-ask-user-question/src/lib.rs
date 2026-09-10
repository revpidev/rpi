//! `rpi-ext-ask-user-question` — L0 native plugin, port of
//! `@juicesharp/rpiv-ask-user-question` v2.9.0+ (`juicesharp/rpiv-mono`
//! `packages/rpiv-ask-user-question/` @ `338b264c`).
//!
//! Registers the single `ask_user_question` tool: a structured questionnaire
//! the model can put to the user when it would otherwise guess, with typed
//! options instead of free-form replies. The interactive component (route C,
//! `interactive-ui-abi`) lands with Q2/Q3; Q0 freezes the contract layer:
//! tool schema/description/guidance, line-terminator normalization, validation
//! and error codes, the result envelope, the `rpiv:ask-user:*` event payloads,
//! the `before_agent_start` reconciler, XDG config and the 9 embedded locales.
//!
//! Docs: `rpi-docs/extensions/rpiv-ask-user-question/{00,01,02}.md` and the
//! task file `rpi-docs/plan/extensions/TE28-ask-user-question-contract.md`.
//! Parity harness: `scripts/ask-user-question-parity/` drives the pinned
//! upstream pure-function modules and diffs them against [`parity`].
//!
//! Native plugin runtime model (mcp-adapter / subagents / statusline
//! precedents): `rpi_extension_init` registers through the host-call handle
//! and stores the newest channel (session switches re-load the same
//! dlopen-memoized cdylib on a fresh host); `rpi_dispatch` serves
//! `toolExecute` and `event` messages synchronously.

pub mod config;
pub mod events;
pub mod golden;
pub mod i18n;
pub mod parity_cases;
pub mod reconcile;
pub mod rpc_fallback;
pub mod state;
pub mod tool;
pub mod view;

use std::io::IsTerminal;
use std::sync::{Mutex, OnceLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

/// Structured host-call failure (`{"error": {"kind", "message"}}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    /// ABI error kind (`invalidRequest`, `stale`, `capabilityDenied`, …).
    pub kind: String,
    /// Human-readable detail.
    pub message: String,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Guest → host JSON call surface (`{"call": method, "args": args}` →
/// `{"ok": ...} | {"error": {"kind", "message"}}`). Abstracted so pure logic
/// (tool/events/reconcile) is testable without the abi_stable boundary.
pub trait HostCall {
    /// Send one host call; `Ok` carries the unwrapped `ok` payload.
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError>;
}

/// The native (L0) transport over the abi_stable trampoline.
#[derive(Clone, Copy)]
pub struct NativeHostCall {
    /// `RpiHostCalls::call` handed to `rpi_extension_init`.
    call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    /// Opaque context pointer (stored as `usize` to stay `Send + Sync`).
    cookie: usize,
}

impl NativeHostCall {
    /// Build a transport from the init receipt.
    pub fn new(calls: RpiHostCalls, cookie: PluginCookie) -> Self {
        Self {
            call: calls.call,
            cookie: cookie as usize,
        }
    }
}

impl HostCall for NativeHostCall {
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
        let request = serde_json::to_vec(&json!({
            "call": method,
            "args": args,
            "seq": 0,
        }))
        .map_err(|error| HostError {
            kind: "protocolError".to_owned(),
            message: format!("request JSON: {error}"),
        })?;
        let response = (self.call)(self.cookie as PluginCookie, RVec::from(request));
        let response: Value = serde_json::from_slice(&response[..]).map_err(|error| HostError {
            kind: "protocolError".to_owned(),
            message: format!("response JSON: {error}"),
        })?;
        if let Some(error) = response.get("error") {
            return Err(HostError {
                kind: error
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("internal")
                    .to_owned(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("host error")
                    .to_owned(),
            });
        }
        Ok(response.get("ok").cloned().unwrap_or(Value::Null))
    }
}

/// Write one terminal BEL when `is_tty` (R-Q5.8 / design §4.3).
///
/// Best-effort by contract: callers ignore the `Result` — terminal attention
/// must never fail the questionnaire. BEL (`\x07`) is a non-drawing control
/// byte (`rpi_tui::utils::visible_width` treats control chars as zero columns
/// and the diff renderer tracks no glyph/cursor state for it), unlike the
/// rc.1 raw-text writes that corrupted the screen; the byte is still gated on
/// a TTY so piped/RPC transports stay clean.
pub fn emit_terminal_attention(
    is_tty: bool,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    if !is_tty {
        return Ok(());
    }
    writer.write_all(b"\x07")?;
    writer.flush()
}

/// Production wrapper: gate on `std::io::stdout().is_terminal()`.
pub fn emit_terminal_attention_stdout() {
    let _ = emit_terminal_attention(std::io::stdout().is_terminal(), &mut std::io::stdout());
}

/// Newest host channel (session switches rebind it).
static CHANNEL: OnceLock<Mutex<Option<NativeHostCall>>> = OnceLock::new();

fn channel_cell() -> &'static Mutex<Option<NativeHostCall>> {
    CHANNEL.get_or_init(|| Mutex::new(None))
}

fn store_channel(host: NativeHostCall) {
    let mut cell = channel_cell()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *cell = Some(host);
}

fn current_channel() -> Option<NativeHostCall> {
    *channel_cell()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn error_envelope(kind: &str, message: impl std::fmt::Display) -> Value {
    json!({"error": {"kind": kind, "message": message.to_string()}})
}

/// Install: read config, register the tool, attach the reconciler, store the
/// host channel. Idempotent across session switches (newest channel wins).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let host = NativeHostCall::new(calls, cookie);
    let config = config::load_config();

    if let Err(error) = host.call("registerTool", tool::tool_definition(&config)) {
        return error_envelope("init", error);
    }
    if let Err(error) = reconcile::register(&host) {
        return error_envelope("init", error);
    }
    // i18n tables are compile-time embedded; selecting the process locale here
    // keeps the first lookup warm and surfaces a broken table early.
    let locale = i18n::I18n::detect();
    tracing::info!(locale = locale.locale(), "rpiv-ask-user-question installed");
    store_channel(host);
    json!({"ok": true})
}

/// Dispatch one host → plugin message.
fn dispatch_message(message: &Value) -> Value {
    let Some(host) = current_channel() else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute")
            if message.get("toolName").and_then(Value::as_str) == Some(tool::TOOL_NAME) =>
        {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            tool::execute(&host, &params)
        }
        Some("event")
            if message.get("event").and_then(Value::as_str) == Some("before_agent_start") =>
        {
            if let Err(error) = reconcile::handle_before_agent_start(&host) {
                tracing::warn!(%error, "rpiv-ask-user-question: reconcile failed");
            }
            Value::Null
        }
        _ => Value::Null,
    }
}

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

/// Load entry (abi_stable). A panic must cross the ABI as an error envelope,
/// not unwind into the host (statusline/subagents precedent).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|panic| {
            error_envelope(
                "internal",
                format!("rpiv-ask-user-question init panicked: {panic:?}"),
            )
        });
    pack(&result)
}

/// Dispatch entry (abi_stable).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
        dispatch_message(&message)
    }))
    .unwrap_or_else(|_panic| {
        json!({
            "content": [{
                "type": "text",
                "text": "rpiv-ask-user-question panicked while handling a dispatch",
            }],
            "isError": true,
        })
    });
    pack(&result)
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

/// Test seam: install directly from Rust, bypassing the cdylib/ABI boundary
/// (mcp-adapter `install_for_test` precedent).
#[doc(hidden)]
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

/// Test seam: dispatch a message against the stored channel.
#[doc(hidden)]
pub fn dispatch_for_test(message: &Value) -> Value {
    dispatch_message(message)
}

/// Parity harness facade: exposes the pure-port functions for
/// `scripts/ask-user-question-parity` without crate internals.
pub mod parity {
    pub use crate::config::{
        format_key_spec_for_display, is_valid_collapse_key_spec, resolve_collapse_key,
        validate_guidance_fields, AskUserQuestionConfig, GuidanceFields, DEFAULT_COLLAPSE_KEY,
    };
    pub use crate::events::{build_blocked_payload, build_prompt_payload};
    pub use crate::golden::{
        frame_json as golden_frame_json, renders as golden_renders, GoldenFrame, GoldenRender,
        WIDTHS as GOLDEN_WIDTHS,
    };
    pub use crate::i18n::{match_locale, parse_locale_env, I18n, SUPPORTED_LOCALES};
    pub use crate::parity_cases::{replay_keys_case, replay_state_case};
    pub use crate::reconcile::reconcile_active_tools;
    pub use crate::rpc_fallback::{
        build_preview_block, format_option_line, has_dialog_ui, parse_index, run_rpc_questionnaire,
        DialogOutcome, DialogUi, HostUi, CUSTOM_ANSWER_TITLE, MAX_PREVIEW_CHARS,
        MULTI_SELECT_INSTRUCTIONS, MULTI_SELECT_PLACEHOLDER,
    };
    pub use crate::state::build::{build_items_for_question, QuestionItem};
    pub use crate::state::key_router::{route_key, Action, Keybindings, QuestionnaireRuntime};
    pub use crate::state::reducer::{
        apply as apply_action, result_for, state_from_json, ApplyContext, ApplyResult, Effect,
        QuestionnaireState,
    };
    pub use crate::state::row_intent::{
        is_reserved_label, label_by_kind, labels_by_kind_json, meta, reserved_label_set,
        sentinels_to_append, RowIntentMeta, RowKind, ROW_INTENT_META, SENTINEL_KINDS,
    };
    pub use crate::state::session::{mount_options, InputBuffer, QuestionnaireComponent};
    pub use crate::tool::envelope::{
        build_answer_segment, build_questionnaire_response, build_tool_result,
        format_answer_scalar, FormatAnswerVariant, DECLINE_MESSAGE, ENVELOPE_PREFIX,
        ENVELOPE_SUFFIX, NO_INPUT_PLACEHOLDER,
    };
    pub use crate::tool::normalize::{normalize_line_terminators, normalize_question_params};
    pub use crate::tool::types::{
        question_params_schema, AnswerKind, OptionData, QuestionAnswer, QuestionData,
        QuestionParams, QuestionnaireError, QuestionnaireResult, MAX_HEADER_LENGTH,
        MAX_LABEL_LENGTH, MAX_OPTIONS, MAX_QUESTIONS, MIN_OPTIONS, RESERVED_LABELS,
    };
    pub use crate::tool::validate::{
        validate_questionnaire, ValidationResult, ERROR_DUPLICATE_OPTION_LABEL,
        ERROR_DUPLICATE_QUESTION, ERROR_NO_QUESTIONS,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted trampoline: records `(method, args)` and answers from a queue.
    static REQUESTS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
    static RESPONSES: Mutex<VecDeque<Result<Value, (&'static str, &'static str)>>> =
        Mutex::new(VecDeque::new());

    extern "C" fn fake_call(_cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
        let request: Value = serde_json::from_slice(&request[..]).expect("request json");
        let method = request
            .get("call")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let args = request.get("args").cloned().unwrap_or(Value::Null);
        REQUESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((method, args));
        let response = RESPONSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
            .unwrap_or(Ok(Value::Null));
        let value = match response {
            Ok(value) => json!({"ok": value}),
            Err((kind, message)) => json!({"error": {"kind": kind, "message": message}}),
        };
        RVec::from(serde_json::to_vec(&value).expect("response json"))
    }

    fn reset_fake() {
        REQUESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        RESPONSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn queue_reply(reply: Result<Value, (&'static str, &'static str)>) {
        RESPONSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(reply);
    }

    fn requests() -> Vec<(String, Value)> {
        REQUESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The fake transport is process-global; tests in this module that touch
    /// install/dispatch are serialized through this lock.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lib_install_registers_tool_and_reconciler() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_fake();
        queue_reply(Ok(Value::Null)); // registerTool
        queue_reply(Ok(Value::Null)); // on(before_agent_start)
        let receipt = install_for_test(RpiHostCalls { call: fake_call }, std::ptr::null());
        assert_eq!(receipt, json!({"ok": true}));
        let calls = requests();
        assert_eq!(calls[0].0, "registerTool");
        assert_eq!(calls[0].1["name"], "ask_user_question");
        assert_eq!(calls[0].1["label"], "Ask User Question");
        assert_eq!(calls[0].1["parameters"]["type"], "object");
        assert_eq!(calls[1].0, "on");
        assert_eq!(calls[1].1["event"], "before_agent_start");
    }

    #[test]
    fn lib_install_failure_returns_error_envelope() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_fake();
        queue_reply(Err(("capabilityDenied", "requires tools")));
        let receipt = install_for_test(RpiHostCalls { call: fake_call }, std::ptr::null());
        assert_eq!(receipt["error"]["kind"], "init");
        assert!(receipt["error"]["message"]
            .as_str()
            .expect("message")
            .contains("capabilityDenied"));
    }

    #[test]
    fn lib_dispatch_routes_tool_execute_and_events() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_fake();
        queue_reply(Ok(Value::Null)); // registerTool
        queue_reply(Ok(Value::Null)); // on
        let _ = install_for_test(RpiHostCalls { call: fake_call }, std::ptr::null());
        reset_fake();

        // toolExecute -> execute path (ctx.hasUI + prompt event).
        queue_reply(Ok(json!(true))); // ctx.hasUI
        queue_reply(Ok(Value::Null)); // events.emit (prompt)
        queue_reply(Ok(Value::Null)); // ctx.mode
        queue_reply(Ok(Value::Null)); // events.emit (blocked:true)
        queue_reply(Err((
            "unknownMethod",
            "unknown host call: ui.mountComponent",
        )));
        queue_reply(Ok(Value::Null)); // events.emit (blocked:false)
        queue_reply(Ok(json!(false))); // ctx.hasUI (dialog-primitive probe)
        let result = dispatch_for_test(&json!({
            "kind": "toolExecute",
            "toolName": "ask_user_question",
            "toolCallId": "call-1",
            "params": {"questions": [{
                "question": "Pick?",
                "header": "Pick",
                "options": [
                    {"label": "A", "description": "a"},
                    {"label": "B", "description": "b"}
                ]
            }]},
        }));
        assert_eq!(result["details"]["error"], "no_custom_ui");
        let calls = requests();
        assert_eq!(calls[0].0, "ctx.hasUI");
        assert_eq!(calls[1].0, "events.emit");

        // A different tool name is not ours.
        reset_fake();
        assert_eq!(
            dispatch_for_test(&json!({"kind": "toolExecute", "toolName": "other"})),
            Value::Null
        );
        assert!(requests().is_empty());

        // before_agent_start event -> reconcile (strip in non-interactive mode).
        reset_fake();
        queue_reply(Ok(json!(false))); // ctx.hasUI
        queue_reply(Ok(json!(["read", "ask_user_question"]))); // getActiveTools
        queue_reply(Ok(Value::Null)); // setActiveTools
        assert_eq!(
            dispatch_for_test(
                &json!({"kind": "event", "event": "before_agent_start", "payload": {}})
            ),
            Value::Null
        );
        let calls = requests();
        assert_eq!(calls[0].0, "ctx.hasUI");
        assert_eq!(calls[1].0, "getActiveTools");
        assert_eq!(calls[2].0, "setActiveTools");
        assert_eq!(calls[2].1["toolNames"], json!(["read"]));

        // Unknown events are ignored.
        reset_fake();
        assert_eq!(
            dispatch_for_test(&json!({"kind": "event", "event": "message_end"})),
            Value::Null
        );
        assert!(requests().is_empty());
    }

    #[test]
    fn lib_terminal_attention_gates_on_tty() {
        let mut out = Vec::new();
        emit_terminal_attention(false, &mut out).expect("no tty");
        assert!(out.is_empty(), "non-TTY writes nothing (RPC/piped hosts)");
        emit_terminal_attention(true, &mut out).expect("tty");
        assert_eq!(out, b"\x07", "TTY writes exactly one BEL byte");
    }

    #[test]
    fn lib_native_host_call_wraps_envelope() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_fake();
        let host = NativeHostCall {
            call: fake_call,
            cookie: 0,
        };
        queue_reply(Ok(json!({"handle": 5})));
        assert_eq!(
            host.call("ui.mountComponent", json!({})).expect("ok"),
            json!({"handle": 5})
        );
        queue_reply(Err(("unknownMethod", "nope")));
        let error = host
            .call("ui.mountComponent", json!({}))
            .expect_err("error");
        assert_eq!(error.kind, "unknownMethod");
        assert_eq!(error.message, "nope");
        let calls = requests();
        assert_eq!(calls[0].0, "ui.mountComponent");
    }
}
