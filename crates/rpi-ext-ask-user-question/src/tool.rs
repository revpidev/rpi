//! Tool definition + `execute` orchestration.
//!
//! Port of upstream `packages/rpiv-ask-user-question/ask-user-question.ts` @
//! `338b264c` for the Q0/Q1 surface: registration payload (schema/description/
//! promptSnippet/promptGuidelines with config overrides), the fixed
//! `execute` order (normalize → `ctx.hasUI` guard → validate → `prompt` event
//! → branch) and the envelope shapes for every failure exit.
//!
//! Branch wiring (TE29): RPC hosts (`ctx.mode == "rpc"`, upstream issue #78)
//! route to the sequential dialog walker up front; every other host falls to
//! the `resolveUndefinedResult` contract — dialog primitives → walker,
//! otherwise `no_custom_ui`. TE30 replaces the static component-unavailable
//! arm with the interactive component (`ui.custom` equivalent); the
//! surrounding order and guards stay.

use serde_json::{json, Value};

pub mod envelope;
pub mod normalize;
pub mod types;
pub mod validate;

use crate::config::{
    default_prompt_guidelines, default_prompt_snippet, AskUserQuestionConfig,
    DEFAULT_TOOL_DESCRIPTION,
};
use crate::events;
use crate::i18n::I18n;
use crate::reconcile::ASK_USER_QUESTION_TOOL_NAME;
use crate::rpc_fallback;
use crate::tool::envelope::build_tool_result;
use crate::tool::normalize::normalize_question_params;
use crate::tool::types::{
    question_params_schema, QuestionParams, QuestionnaireError, QuestionnaireResult,
};
use crate::tool::validate::{validate_questionnaire, ValidationResult};
use crate::{HostCall, HostError};

/// Canonical tool name (`ASK_USER_QUESTION_TOOL_NAME`, re-exported for
/// call sites that only depend on `tool`).
pub const TOOL_NAME: &str = ASK_USER_QUESTION_TOOL_NAME;

/// `ERROR_NO_UI` (upstream literal).
pub const ERROR_NO_UI: &str = "Error: UI not available (running in non-interactive mode)";

/// `ERROR_NO_CUSTOM_UI` (upstream literal).
pub const ERROR_NO_CUSTOM_UI: &str = "Error: this client cannot render the questionnaire (custom UI is unavailable, e.g. RPC/ACP hosts such as Zed or Paseo). The user never saw the questions — do NOT treat this as a decline. Ask the questions as plain chat text instead, without using this tool.";

/// Build the `registerTool` payload with config overrides applied
/// (`registerAskUserQuestionTool`): guidance fields replace the defaults only
/// when non-empty (validated by [`crate::config::validate_guidance_fields`]).
pub fn tool_definition(config: &AskUserQuestionConfig) -> Value {
    let guidance = &config.guidance;
    json!({
        "name": TOOL_NAME,
        "label": "Ask User Question",
        "description": guidance.description.clone().unwrap_or_else(|| DEFAULT_TOOL_DESCRIPTION.to_owned()),
        "promptSnippet": guidance.prompt_snippet.clone().unwrap_or_else(default_prompt_snippet),
        "promptGuidelines": guidance.prompt_guidelines.clone().unwrap_or_else(default_prompt_guidelines),
        "parameters": question_params_schema(),
    })
}

fn has_ui(host: &dyn HostCall) -> bool {
    host.call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// `(ctx as { mode?: string }).mode` — hosts that predate `ctx.mode` (or a
/// failing probe) answer `None`, which is simply "not rpc" (the upstream
/// backstop below then catches those builds).
fn host_mode(host: &dyn HostCall) -> Option<String> {
    host.call("ctx.mode", json!({}))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
}

fn failure_result(error: QuestionnaireError, message: &str) -> Value {
    build_tool_result(
        message,
        serde_json::to_value(QuestionnaireResult::failure(error)).unwrap_or(Value::Null),
    )
}

/// Dialog transport failure inside the walker (upstream: the dialog promise
/// rejects and propagates out of `execute`, which the host wraps into an
/// errored tool result). rpi surfaces the same shape inline: `isError` + the
/// failure detail (the handler-error convention, requirements 附录 B).
/// Component transport/registry failure (upstream: the `custom()` promise
/// rejects and propagates out of `execute`, which the host wraps into an
/// errored tool result). Same `isError` shape as [`host_error_result`] with
/// component-specific wording.
fn component_error_result(error: &HostError) -> Value {
    let mut result = build_tool_result(
        format!("Error: ask_user_question component failed: {error}"),
        json!({ "answers": [], "cancelled": true }),
    );
    if let Some(object) = result.as_object_mut() {
        object.insert("isError".to_owned(), Value::Bool(true));
    }
    result
}

fn host_error_result(error: &HostError) -> Value {
    let mut result = build_tool_result(
        format!("Error: ask_user_question host dialog failed: {error}"),
        json!({ "answers": [], "cancelled": true }),
    );
    if let Some(object) = result.as_object_mut() {
        object.insert("isError".to_owned(), Value::Bool(true));
    }
    result
}

/// Defensive exit for params that do not match the registered schema. The
/// host validates tool arguments against `parameters` before `execute`
/// (rpi-agent `validate_tool_arguments`), so this arm is unreachable through
/// the agent loop; direct callers (tests/parity) still get a structured
/// result instead of a panic.
fn invalid_params_result(error: &serde_json::Error) -> Value {
    let mut result = build_tool_result(
        format!("Error: invalid ask_user_question parameters: {error}"),
        json!({ "answers": [], "cancelled": true }),
    );
    if let Some(object) = result.as_object_mut() {
        object.insert("isError".to_owned(), Value::Bool(true));
    }
    result
}

/// Execute the tool against a host (`execute`). Returns the tool result JSON
/// (`{content, details, isError?}`).
pub fn execute(host: &dyn HostCall, params: &Value) -> Value {
    // Line-terminator normalization runs once here, ahead of validation, so
    // every downstream consumer — validator, TUI, RPC walker, envelope,
    // prompt event — sees the same clean text (#192).
    let typed: QuestionParams = match serde_json::from_value(params.clone()) {
        Ok(typed) => typed,
        Err(error) => return invalid_params_result(&error),
    };
    let typed = normalize_question_params(&typed);

    // Non-interactive host backstop (the reconciler normally strips the tool
    // first).
    if !has_ui(host) {
        return failure_result(QuestionnaireError::NoUi, ERROR_NO_UI);
    }

    let validation = validate_questionnaire(&typed);
    if let ValidationResult::Failed { error, message } = validation {
        return failure_result(error, &message);
    }

    // Emit the prompt event for external listeners (notification plugins
    // etc.) before the questionnaire opens. Upstream would propagate a
    // listener throw; in rpi `events.emit` is fire-and-forget and a host
    // failure is logged, never fatal to the tool call.
    if let Err(error) = events::emit_prompt(host, &typed) {
        tracing::warn!(
            kind = %error.kind,
            message = %error.message,
            "rpiv-ask-user-question: prompt event emit failed"
        );
    }

    // RPC hosts (VSCode pendant, ACP clients like Zed/Paseo — upstream issue
    // #78): `ui.custom` cannot render there, but the select/input dialog
    // sub-protocol works; hosts advertising `ctx.mode` (rpi always does)
    // route to the sequential dialog walker up front, skipping the component
    // path entirely.
    if host_mode(host).as_deref() == Some("rpc")
        && rpc_fallback::has_dialog_ui(Some(&rpc_fallback::HostCallUi::probe(host)))
    {
        return run_rpc_path_with(
            host,
            &typed,
            &I18n::detect(),
            crate::emit_terminal_attention_stdout,
        );
    }

    // TUI component path (TE30): mount the interactive component and drive
    // the poll/render loop. A host without the interactive-UI ABI (C0/old
    // host) answers `unknownMethod` on the first mount — upstream's
    // `ctx.ui.custom()` resolving undefined — and falls through to the
    // `resolveUndefinedResult` contract below (dialog primitives → walker,
    // otherwise `no_custom_ui`).
    resolve_undefined_result_with(host, &typed, &I18n::detect())
}

/// `runRpcPath` — the RPC walker bracketed by the blocked-event pair, with
/// the terminal BEL between `blocked:true` and the first dialog. The
/// closing emit always runs (upstream `finally`), even when a dialog
/// transport fails. The BEL emitter is injected so tests can pin the
/// ordering without touching stdout.
fn run_rpc_path_with(
    host: &dyn HostCall,
    typed: &QuestionParams,
    i18n: &I18n,
    emit_bel: impl FnOnce(),
) -> Value {
    emit_blocked_or_warn(host, true);
    let outcome = {
        emit_bel();
        let mut ui = rpc_fallback::HostCallDialogUi::new(host);
        rpc_fallback::run_rpc_questionnaire(&mut ui, typed, i18n)
    };
    emit_blocked_or_warn(host, false);
    match outcome {
        Ok(result) => crate::tool::envelope::build_questionnaire_response(Some(&result), typed),
        Err(error) => {
            tracing::warn!(
                kind = %error.kind,
                message = %error.message,
                "rpiv-ask-user-question: rpc dialog host call failed"
            );
            host_error_result(&error)
        }
    }
}

/// `resolveUndefinedResult` (`ask-user-question.ts:236-247`): try the
/// interactive component first; a host that cannot render it (`unknownMethod`)
/// falls to the dialog primitives or tells the model the user never saw the
/// questions.
///
/// The component attempt is bracketed by the `rpiv:ask-user:blocked` event
/// pair (upstream `try/finally`); the fallback walker below runs bare, like
/// upstream's `resolveUndefinedResult`. The terminal BEL between
/// `blocked:true` and the mount is Q3 (FR-Q3-G); Q2 only keeps the event
/// bracket.
fn resolve_undefined_result_with(
    host: &dyn HostCall,
    typed: &QuestionParams,
    i18n: &I18n,
) -> Value {
    emit_blocked_or_warn(host, true);
    let component = crate::state::session::run(host, typed, i18n, &crate::config::load_config());
    emit_blocked_or_warn(host, false);

    match component {
        Ok(result) => crate::tool::envelope::build_questionnaire_response(Some(&result), typed),
        Err(error) if error.is_unknown_method() => resolve_without_component(host, typed, i18n),
        Err(error) => {
            tracing::warn!(
                kind = %error.kind,
                message = %error.message,
                "rpiv-ask-user-question: component host call failed"
            );
            component_error_result(&HostError {
                kind: error.kind.as_str().to_owned(),
                message: error.message,
            })
        }
    }
}

/// The dialog-primitive / `no_custom_ui` fallback (upstream
/// `resolveUndefinedResult`).
fn resolve_without_component(host: &dyn HostCall, typed: &QuestionParams, i18n: &I18n) -> Value {
    if rpc_fallback::has_dialog_ui(Some(&rpc_fallback::HostCallUi::probe(host))) {
        let mut ui = rpc_fallback::HostCallDialogUi::new(host);
        match rpc_fallback::run_rpc_questionnaire(&mut ui, typed, i18n) {
            Ok(result) => crate::tool::envelope::build_questionnaire_response(Some(&result), typed),
            Err(error) => {
                tracing::warn!(
                    kind = %error.kind,
                    message = %error.message,
                    "rpiv-ask-user-question: fallback dialog host call failed"
                );
                host_error_result(&error)
            }
        }
    } else {
        failure_result(QuestionnaireError::NoCustomUi, ERROR_NO_CUSTOM_UI)
    }
}

fn emit_blocked_or_warn(host: &dyn HostCall, active: bool) {
    if let Err(error) = events::emit_blocked(host, active) {
        tracing::warn!(
            kind = %error.kind,
            message = %error.message,
            "rpiv-ask-user-question: blocked event emit failed"
        );
    }
}

/// Emit the `blocked` bracket around a wait (`runRpcPath` since TE29;
/// TE30 reuses it for the component). Exposed so the Q0 contract test can
/// assert the pair is well-formed.
pub fn emit_blocked(host: &dyn HostCall, active: bool) -> Result<(), HostError> {
    events::emit_blocked(host, active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostCall, HostError};
    use std::sync::Mutex;

    /// Scripted host: records calls, answers from a method -> value map.
    #[derive(Default)]
    struct FakeHost {
        calls: Mutex<Vec<(String, Value)>>,
        replies: Mutex<Vec<(String, Result<Value, HostError>)>>,
    }

    impl FakeHost {
        fn new(replies: &[(&str, Value)]) -> Self {
            let host = FakeHost::default();
            {
                let mut queue = host.replies.lock().expect("queue");
                for (method, value) in replies {
                    queue.push(((*method).to_owned(), Ok(value.clone())));
                }
            }
            host
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    /// One scripted reply: `Ok(value)` or an ABI error `(kind, message)`.
    type ScriptEntry<'a> = (&'a str, Result<Value, (&'static str, &'static str)>);

    /// Scripted host with per-method `Ok`/`Err` replies (error entries model
    /// the ABI `unknownMethod`/`internal` envelopes).
    fn host_script(entries: Vec<ScriptEntry<'_>>) -> FakeHost {
        let host = FakeHost::default();
        {
            let mut queue = host.replies.lock().expect("queue");
            for (method, value) in entries {
                queue.push((
                    method.to_owned(),
                    value.map_err(|(kind, message)| HostError {
                        kind: kind.to_owned(),
                        message: message.to_owned(),
                    }),
                ));
            }
        }
        host
    }

    fn unknown_method() -> (&'static str, &'static str) {
        ("unknownMethod", "unknown host call: ui.mountComponent")
    }

    impl HostCall for FakeHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
            self.calls
                .lock()
                .expect("calls")
                .push((method.to_owned(), args.clone()));
            let mut queue = self.replies.lock().expect("queue");
            match queue.iter().position(|(name, _)| name == method) {
                Some(index) => queue.remove(index).1,
                None => Ok(Value::Null),
            }
        }
    }

    /// Timeline wrapper over [`FakeHost`] — labels each host call so the
    /// BEL position can be asserted between the blocked emits.
    struct RecordingHost<'a> {
        inner: &'a FakeHost,
        timeline: &'a Mutex<Vec<String>>,
    }

    impl HostCall for RecordingHost<'_> {
        fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
            let label = if method == "events.emit" {
                format!(
                    "events.emit:{}:{}",
                    args["event"].as_str().unwrap_or("?"),
                    args["payload"]["active"]
                )
            } else {
                method.to_owned()
            };
            self.timeline.lock().expect("timeline").push(label);
            self.inner.call(method, args)
        }
    }

    fn valid_params() -> Value {
        json!({
            "questions": [
                {
                    "question": "Pick one?",
                    "header": "Pick",
                    "options": [
                        {"label": "A", "description": "a"},
                        {"label": "B", "description": "b"}
                    ]
                }
            ]
        })
    }

    #[test]
    fn tool_definition_defaults_and_overrides() {
        let definition = tool_definition(&AskUserQuestionConfig::default());
        assert_eq!(definition["name"], "ask_user_question");
        assert_eq!(definition["label"], "Ask User Question");
        assert_eq!(definition["description"], DEFAULT_TOOL_DESCRIPTION);
        assert!(definition["promptSnippet"]
            .as_str()
            .expect("snippet")
            .contains("up to 4 structured questions"));
        assert_eq!(
            definition["promptGuidelines"]
                .as_array()
                .expect("guidelines")
                .len(),
            4
        );
        assert_eq!(definition["parameters"]["type"], "object");

        let config = AskUserQuestionConfig {
            guidance: crate::config::GuidanceFields {
                description: Some("custom description".to_owned()),
                prompt_snippet: Some("custom snippet".to_owned()),
                prompt_guidelines: Some(vec!["g".to_owned()]),
            },
            collapse_key: None,
        };
        let definition = tool_definition(&config);
        assert_eq!(definition["description"], "custom description");
        assert_eq!(definition["promptSnippet"], "custom snippet");
        assert_eq!(definition["promptGuidelines"], json!(["g"]));
    }

    #[test]
    fn tool_execute_no_ui_backstop_returns_no_ui_envelope() {
        let host = FakeHost::new(&[("ctx.hasUI", json!(false))]);
        let result = execute(&host, &valid_params());
        assert_eq!(result["content"][0]["text"], ERROR_NO_UI);
        assert_eq!(result["details"]["cancelled"], json!(true));
        assert_eq!(result["details"]["error"], json!("no_ui"));
        assert_eq!(result["details"]["answers"], json!([]));
        // Normalization ran before the guard (no prompt event was emitted).
        assert_eq!(
            host.calls()
                .iter()
                .map(|(m, _)| m.as_str())
                .collect::<Vec<_>>(),
            vec!["ctx.hasUI"]
        );
    }

    #[test]
    fn tool_execute_validation_failure_emits_no_prompt_event() {
        let host = FakeHost::new(&[("ctx.hasUI", json!(true))]);
        let mut params = valid_params();
        params["questions"][0]["options"] = json!([{"label": "A", "description": "a"}]);
        let result = execute(&host, &params);
        assert_eq!(
            result["content"][0]["text"],
            "Error: Each question requires at least 2 options"
        );
        assert_eq!(result["details"]["error"], json!("empty_options"));
        assert_eq!(
            host.calls()
                .iter()
                .map(|(m, _)| m.as_str())
                .collect::<Vec<_>>(),
            vec!["ctx.hasUI"],
            "validation fails before the prompt event"
        );
    }

    #[test]
    fn tool_execute_emits_prompt_then_no_custom_ui_when_dialogs_unavailable() {
        // hasUI guard passes, but the dialog-primitive probe answers false
        // (the rpi equivalent of upstream `hasDialogUI(ctx.ui) === false`) —
        // the `no_custom_ui` envelope with the byte-identical literal.
        let host = host_script(vec![
            ("ctx.hasUI", Ok(json!(true))),
            ("events.emit", Ok(json!(null))), // prompt
            ("ctx.mode", Ok(json!("tui"))),
            ("events.emit", Ok(json!(null))), // blocked:true
            ("ui.mountComponent", Err(unknown_method())),
            ("events.emit", Ok(json!(null))), // blocked:false
            ("ctx.hasUI", Ok(json!(false))),  // dialog-primitive probe
        ]);
        let result = execute(&host, &valid_params());
        assert_eq!(result["content"][0]["text"], ERROR_NO_CUSTOM_UI);
        assert_eq!(result["details"]["error"], json!("no_custom_ui"));
        assert_eq!(result["details"]["cancelled"], json!(true));
        let calls = host.calls();
        let methods: Vec<&str> = calls.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(
            methods,
            vec![
                "ctx.hasUI",
                "events.emit",
                "ctx.mode",
                "events.emit",
                "ui.mountComponent",
                "events.emit",
                "ctx.hasUI"
            ]
        );
        assert_eq!(calls[1].1["event"], "rpiv:ask-user:prompt");
        assert_eq!(
            calls[1].1["payload"]["questions"][0]["options"][0]["hasPreview"],
            json!(false)
        );
        assert_eq!(calls[3].1["payload"], json!({ "active": true }));
        assert_eq!(calls[5].1["payload"], json!({ "active": false }));
    }

    /// RPC hosts route to the dialog walker: `ctx.mode == "rpc"` + both
    /// primitives available → `ui.select`, never a component mount
    /// (upstream "does NOT call ctx.ui.custom in RPC mode").
    #[test]
    fn rpc_mode_uses_walker() {
        let host = FakeHost::new(&[
            ("ctx.hasUI", json!(true)),
            ("events.emit", json!(null)),
            ("ctx.mode", json!("rpc")),
            ("ctx.hasUI", json!(true)),
            ("events.emit", json!(null)),
            ("ui.select", json!("1. A — a")),
            ("events.emit", json!(null)),
        ]);
        let result = execute(&host, &valid_params());
        assert_eq!(
            result["content"][0]["text"],
            "User has answered your questions: \"Pick one?\"=\"A\". You can now continue with the user's answers in mind."
        );
        assert_eq!(result["details"]["cancelled"], json!(false));
        assert_eq!(result["details"]["answers"][0]["kind"], json!("option"));
        assert_eq!(result["details"]["answers"][0]["answer"], json!("A"));
        let calls = host.calls();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            vec![
                "ctx.hasUI",
                "events.emit",
                "ctx.mode",
                "ctx.hasUI",
                "events.emit",
                "ui.select",
                "events.emit"
            ]
        );
        assert!(
            !methods.contains(&"ui.custom"),
            "no component path in RPC mode"
        );
        let select_args = &calls[5].1;
        assert_eq!(select_args["options"][0], "1. A — a");
        assert_eq!(select_args["options"][1], "2. B — b");
        // Sentinel row number + locale-sourced label (same table the walker
        // used — stable under any test env).
        let sentinel = I18n::detect().display_label(crate::state::row_intent::RowKind::Other);
        assert_eq!(select_args["options"][2], json!(format!("3. {sentinel}")));
    }

    /// `runRpcPath` ordering (upstream pins: blocked:true → BEL → first
    /// dialog → blocked:false), and the BEL emitter is invoked exactly once.
    #[test]
    fn rpc_mode_blocked_and_bel_bracket_order() {
        let host = FakeHost::new(&[
            ("events.emit", json!(null)),
            ("ui.select", json!("1. A — a")),
            ("events.emit", json!(null)),
        ]);
        let timeline: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let recording = RecordingHost {
            inner: &host,
            timeline: &timeline,
        };
        let mut bel_count = 0;
        let params: QuestionParams = serde_json::from_value(valid_params()).expect("params");
        let result = run_rpc_path_with(&recording, &params, &I18n::for_locale("en"), || {
            bel_count += 1;
            timeline.lock().expect("timeline").push("BEL".to_owned());
        });
        assert_eq!(
            bel_count, 1,
            "exactly one BEL between blocked:true and the first dialog"
        );
        assert_eq!(
            result["content"][0]["text"],
            "User has answered your questions: \"Pick one?\"=\"A\". You can now continue with the user's answers in mind."
        );
        let timeline = timeline.lock().expect("timeline").clone();
        assert_eq!(
            timeline,
            vec![
                "events.emit:rpiv:ask-user:blocked:true",
                "BEL",
                "ui.select",
                "events.emit:rpiv:ask-user:blocked:false",
            ]
        );
    }

    /// A dialog transport failure surfaces as an `isError` result and the
    /// closing `blocked:false` still runs (upstream `finally`).
    #[test]
    fn rpc_dialog_transport_failure_returns_is_error_with_closing_blocked() {
        let host = FakeHost::default();
        {
            let mut queue = host.replies.lock().expect("queue");
            queue.push(("events.emit".to_owned(), Ok(json!(null))));
            queue.push((
                "ui.select".to_owned(),
                Err(HostError {
                    kind: "capabilityDenied".to_owned(),
                    message: "ui.select requires capability ui".to_owned(),
                }),
            ));
            queue.push(("events.emit".to_owned(), Ok(json!(null))));
        }
        let params: QuestionParams = serde_json::from_value(valid_params()).expect("params");
        let result = run_rpc_path_with(&host, &params, &I18n::for_locale("en"), || {});
        assert_eq!(result["isError"], json!(true));
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("host dialog failed"));
        let calls = host.calls();
        assert_eq!(calls[0].1["payload"], json!({ "active": true }));
        assert_eq!(calls[2].1["payload"], json!({ "active": false }));
    }

    /// Non-RPC host whose component mount answers `unknownMethod` falls to
    /// the bare walker: the component attempt carries the blocked bracket
    /// (upstream `try/finally` around `ctx.ui.custom`), the fallback walker
    /// itself runs bare inside `resolveUndefinedResult`.
    #[test]
    fn component_unknown_method_falls_back_to_walker_bare() {
        let host = host_script(vec![
            ("ctx.hasUI", Ok(json!(true))),
            ("events.emit", Ok(json!(null))), // prompt
            ("ctx.mode", Ok(json!("tui"))),
            ("events.emit", Ok(json!(null))), // blocked:true
            ("ui.mountComponent", Err(unknown_method())),
            ("events.emit", Ok(json!(null))), // blocked:false
            ("ctx.hasUI", Ok(json!(true))),   // dialog-primitive probe
            ("ui.select", Ok(json!(null))),
        ]);
        let result = execute(&host, &valid_params());
        assert_eq!(
            result["content"][0]["text"],
            "User declined to answer questions"
        );
        assert_eq!(result["details"]["cancelled"], json!(true));
        let calls = host.calls();
        let emit_count = calls
            .iter()
            .filter(|(method, _)| method == "events.emit")
            .count();
        assert_eq!(
            emit_count, 3,
            "prompt + the component attempt's blocked pair"
        );
        assert_eq!(calls[7].0, "ui.select");
    }

    /// TE30 component path: a TUI host answers mount/poll/render; the user
    /// confirms the first option; the envelope carries the answer and no
    /// dialog primitive is touched.
    #[test]
    fn component_path_answers_without_touching_dialogs() {
        let host = host_script(vec![
            ("ctx.hasUI", Ok(json!(true))),
            ("events.emit", Ok(json!(null))), // prompt
            ("ctx.mode", Ok(json!("tui"))),
            ("events.emit", Ok(json!(null))), // blocked:true
            ("ui.mountComponent", Ok(json!({"handle": 7}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "resize", "width": 80, "height": 24}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "
"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            ("events.emit", Ok(json!(null))), // blocked:false
        ]);
        let result = execute(&host, &valid_params());
        assert_eq!(
            result["content"][0]["text"],
            "User has answered your questions: \"Pick one?\"=\"A\". You can now continue with the user's answers in mind."
        );
        assert_eq!(result["details"]["cancelled"], json!(false));
        assert_eq!(result["details"]["answers"][0]["answer"], json!("A"));
        let calls = host.calls();
        let methods: Vec<&str> = calls.iter().map(|(method, _)| method.as_str()).collect();
        assert!(
            !methods.contains(&"ui.select"),
            "no walker on the component path"
        );
        assert!(!methods.contains(&"ui.custom"));
        // Mount options: bottom-center overlay, tickMs 0, cursor on.
        assert_eq!(calls[4].1["options"]["overlay"], json!(true));
        assert_eq!(
            calls[4].1["options"]["overlayOptions"]["anchor"],
            json!("bottom-center")
        );
        assert_eq!(calls[4].1["options"]["tickMs"], json!(0));
        assert_eq!(calls[4].1["options"]["cursor"], json!(true));
        assert_eq!(calls[4].1["options"]["keysWhenHidden"], json!(["ctrl+]"]));
        // Two frames: the first contains the question, the second carries
        // `done` with the answer.
        let renders: Vec<&Value> = calls
            .iter()
            .filter(|(method, _)| method == "ui.renderComponent")
            .map(|(_, args)| args)
            .collect();
        assert_eq!(renders.len(), 2);
        let first_lines = renders[0]["lines"].as_array().expect("lines");
        assert!(first_lines
            .iter()
            .any(|line| line.as_str().is_some_and(|line| line.contains("Pick one?"))));
        assert_eq!(renders[1]["done"]["answers"][0]["answer"], json!("A"));
    }

    /// A component transport failure that is not `unknownMethod` surfaces as
    /// an `isError` tool result (the host wraps a thrown `execute` upstream)
    /// and never falls through to the walker.
    #[test]
    fn component_host_error_returns_is_error_without_walker() {
        let host = host_script(vec![
            ("ctx.hasUI", Ok(json!(true))),
            ("events.emit", Ok(json!(null))), // prompt
            ("ctx.mode", Ok(json!("tui"))),
            ("events.emit", Ok(json!(null))), // blocked:true
            ("ui.mountComponent", Err(("internal", "registry exploded"))),
            ("events.emit", Ok(json!(null))), // blocked:false
        ]);
        let result = execute(&host, &valid_params());
        assert_eq!(result["isError"], json!(true));
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("component failed: internal: registry exploded"));
        let methods: Vec<String> = host
            .calls()
            .iter()
            .map(|(method, _)| method.clone())
            .collect();
        assert!(!methods.iter().any(|method| method == "ui.select"));
    }

    /// `no_custom_ui` literal is byte-identical to upstream
    /// `ERROR_NO_CUSTOM_UI` (`ask-user-question.ts:79`).
    #[test]
    fn unknown_method_without_dialogs_returns_no_custom_ui() {
        assert_eq!(
            ERROR_NO_CUSTOM_UI,
            "Error: this client cannot render the questionnaire (custom UI is unavailable, e.g. RPC/ACP hosts such as Zed or Paseo). The user never saw the questions — do NOT treat this as a decline. Ask the questions as plain chat text instead, without using this tool."
        );
        assert_eq!(
            ERROR_NO_UI,
            "Error: UI not available (running in non-interactive mode)"
        );
    }

    /// FR-Q1-H: the same walker outcome through the RPC branch and the
    /// fallback branch produces the identical envelope.
    #[test]
    fn envelope_identical_across_paths() {
        let params: QuestionParams = serde_json::from_value(valid_params()).expect("params");
        let i18n = I18n::for_locale("en");

        let rpc_host = FakeHost::new(&[
            ("events.emit", json!(null)),
            ("ui.select", json!("2. B — b")),
            ("events.emit", json!(null)),
        ]);
        let rpc = run_rpc_path_with(&rpc_host, &params, &i18n, || {});

        let fallback_host = host_script(vec![
            ("events.emit", Ok(json!(null))), // blocked:true
            ("ui.mountComponent", Err(unknown_method())),
            ("events.emit", Ok(json!(null))), // blocked:false
            ("ctx.hasUI", Ok(json!(true))),
            ("ui.select", Ok(json!("2. B — b"))),
        ]);
        let fallback = resolve_undefined_result_with(&fallback_host, &params, &i18n);

        assert_eq!(rpc, fallback);
        assert_eq!(
            rpc["content"][0]["text"],
            "User has answered your questions: \"Pick one?\"=\"B\". You can now continue with the user's answers in mind."
        );
    }

    #[test]
    fn tool_execute_normalizes_before_validation_and_event() {
        let host = FakeHost::new(&[("ctx.hasUI", json!(true))]);
        let mut params = valid_params();
        params["questions"][0]["question"] = json!("Pick\r\none?");
        params["questions"][0]["options"][0]["label"] = json!("A\r");
        let _ = execute(&host, &params);
        let calls = host.calls();
        let payload = &calls[1].1["payload"];
        assert_eq!(payload["questions"][0]["question"], "Pick\none?");
        assert_eq!(payload["questions"][0]["options"][0]["label"], "A");
    }
    #[test]
    fn tool_execute_invalid_params_returns_structured_error() {
        let host = FakeHost::new(&[("ctx.hasUI", json!(true))]);
        let result = execute(&host, &json!({"questions": "not an array"}));
        assert_eq!(result["isError"], json!(true));
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .starts_with("Error: invalid ask_user_question parameters"));
        assert_eq!(host.calls().len(), 0, "no host call before params parse");
    }

    #[test]
    fn tool_emit_blocked_pair_is_well_formed() {
        let host = FakeHost::new(&[]);
        emit_blocked(&host, true).expect("blocked true");
        emit_blocked(&host, false).expect("blocked false");
        let calls = host.calls();
        assert_eq!(calls[0].1["event"], "rpiv:ask-user:blocked");
        assert_eq!(calls[0].1["payload"], json!({"active": true}));
        assert_eq!(calls[1].1["payload"], json!({"active": false}));
    }
}
