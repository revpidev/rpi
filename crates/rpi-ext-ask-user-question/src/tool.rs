//! Tool definition + `execute` orchestration.
//!
//! Port of upstream `packages/rpiv-ask-user-question/ask-user-question.ts` @
//! `338b264c` for the Q0 surface: registration payload (schema/description/
//! promptSnippet/promptGuidelines with config overrides), the fixed
//! `execute` order (normalize → `ctx.hasUI` guard → validate → `prompt` event
//! → branch) and the envelope shapes for every failure exit.
//!
//! Branch wiring: Q0 has no component (TE30) and no RPC walker (TE29), so the
//! post-event branch returns the `no_custom_ui` envelope — the upstream
//! contract for "host cannot render and has no `select`/`input`". TE29/TE30
//! replace that single arm; the surrounding order and guards are final.

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
use crate::reconcile::ASK_USER_QUESTION_TOOL_NAME;
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

fn failure_result(error: QuestionnaireError, message: &str) -> Value {
    build_tool_result(
        message,
        serde_json::to_value(QuestionnaireResult::failure(error)).unwrap_or(Value::Null),
    )
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

    // Q0 branch placeholder: TE29 wires the RPC dialog walker
    // (`ctx.mode == "rpc"` / `unknownMethod` fallback), TE30 the interactive
    // component. Until a renderer exists, the honest answer is the
    // upstream `no_custom_ui` envelope.
    failure_result(QuestionnaireError::NoCustomUi, ERROR_NO_CUSTOM_UI)
}

/// Emit the `blocked` bracket around a wait (used by TE29/TE30). Exposed here
/// so the Q0 contract test can assert the pair is well-formed; `execute` does
/// not wait yet.
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
    fn tool_execute_emits_prompt_then_q0_placeholder() {
        let host = FakeHost::new(&[("ctx.hasUI", json!(true))]);
        let result = execute(&host, &valid_params());
        assert_eq!(result["content"][0]["text"], ERROR_NO_CUSTOM_UI);
        assert_eq!(result["details"]["error"], json!("no_custom_ui"));
        assert_eq!(result["details"]["cancelled"], json!(true));
        let calls = host.calls();
        assert_eq!(calls[0].0, "ctx.hasUI");
        assert_eq!(calls[1].0, "events.emit");
        assert_eq!(calls[1].1["event"], "rpiv:ask-user:prompt");
        assert_eq!(
            calls[1].1["payload"]["questions"][0]["options"][0]["hasPreview"],
            json!(false)
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
