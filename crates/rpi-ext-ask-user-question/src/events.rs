//! Public event contract (`rpiv:ask-user:prompt` / `rpiv:ask-user:blocked`).
//!
//! Port of upstream `packages/rpiv-ask-user-question/events.ts` @ `338b264c`.
//! Stability policy (upstream header, applies to every `rpiv:*` event):
//! channel names are immutable; payload changes are append-only; breaking
//! changes require a new channel; payloads are JSON-safe.
//!
//! `prompt` is emitted after validation and before the questionnaire opens;
//! `blocked {active:true}` brackets the wait and `{active:false}` runs in the
//! `finally` path. Q0 lands the payload builders + emit helpers; the wait
//! bracket is wired by TE29 (RPC walker) / TE30 (component), which is why
//! `tool::execute` does not emit `blocked` yet.

use serde_json::{json, Value};

use crate::tool::types::QuestionParams;
use crate::HostCall;

/// `ASK_USER_PROMPT_EVENT` — emitted while the questionnaire is about to open.
pub const ASK_USER_PROMPT_EVENT: &str = "rpiv:ask-user:prompt";
/// `ASK_USER_BLOCKED_EVENT` — emitted while awaiting user input.
pub const ASK_USER_BLOCKED_EVENT: &str = "rpiv:ask-user:blocked";

/// Build the `prompt` payload: `{questions:[{question,header,multiSelect,options:[{label,description,hasPreview}]}]}`.
///
/// `hasPreview` is derived exactly like upstream: `typeof preview === "string"
/// && preview.length > 0` (content itself is not shipped).
pub fn build_prompt_payload(params: &QuestionParams) -> Value {
    let questions: Vec<Value> = params
        .questions
        .iter()
        .map(|question| {
            let options: Vec<Value> = question
                .options
                .iter()
                .map(|option| {
                    json!({
                        "label": option.label,
                        "description": option.description,
                        "hasPreview": option.preview.as_ref().is_some_and(|preview| !preview.is_empty()),
                    })
                })
                .collect();
            json!({
                "question": question.question,
                "header": question.header,
                "multiSelect": question.multi_select.unwrap_or(false),
                "options": options,
            })
        })
        .collect();
    json!({ "questions": questions })
}

/// Build the `blocked` payload: `{active}`.
pub fn build_blocked_payload(active: bool) -> Value {
    json!({ "active": active })
}

/// Emit [`ASK_USER_PROMPT_EVENT`] through the host (`events.emit`).
///
/// Wire form follows the rpi ABI (`{"channel", "data"}` —
/// `extension-abi.md` §3, the `pi.events.emit(channel, payload)` upstream
/// shape; `rpi-ext-subagents` precedent) rather than the JS call-site names.
/// Found by the V14-24 pilot e2e: the earlier `{"event", "payload"}` args
/// never reached the `rpiv:ask-user:*` channels on a real host.
pub fn emit_prompt(host: &dyn HostCall, params: &QuestionParams) -> Result<(), crate::HostError> {
    host.call(
        "events.emit",
        json!({ "channel": ASK_USER_PROMPT_EVENT, "data": build_prompt_payload(params) }),
    )
    .map(|_| ())
}

/// Emit [`ASK_USER_BLOCKED_EVENT`] through the host (`events.emit`);
/// same `channel`/`data` wire form as [`emit_prompt`].
pub fn emit_blocked(host: &dyn HostCall, active: bool) -> Result<(), crate::HostError> {
    host.call(
        "events.emit",
        json!({ "channel": ASK_USER_BLOCKED_EVENT, "data": build_blocked_payload(active) }),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::{OptionData, QuestionData};

    fn params() -> QuestionParams {
        QuestionParams {
            questions: vec![
                QuestionData {
                    question: "Q1?".to_owned(),
                    header: "One".to_owned(),
                    options: vec![
                        OptionData {
                            label: "A".to_owned(),
                            description: "a".to_owned(),
                            preview: Some("md".to_owned()),
                        },
                        OptionData {
                            label: "B".to_owned(),
                            description: "b".to_owned(),
                            preview: Some(String::new()),
                        },
                    ],
                    multi_select: Some(true),
                },
                QuestionData {
                    question: "Q2?".to_owned(),
                    header: "Two".to_owned(),
                    options: vec![
                        OptionData {
                            label: "C".to_owned(),
                            description: "c".to_owned(),
                            preview: None,
                        },
                        OptionData {
                            label: "D".to_owned(),
                            description: "d".to_owned(),
                            preview: None,
                        },
                    ],
                    multi_select: None,
                },
            ],
        }
    }

    #[test]
    fn events_prompt_payload_matches_upstream_shape() {
        let payload = build_prompt_payload(&params());
        assert_eq!(
            payload,
            json!({
                "questions": [
                    {
                        "question": "Q1?",
                        "header": "One",
                        "multiSelect": true,
                        "options": [
                            {"label": "A", "description": "a", "hasPreview": true},
                            {"label": "B", "description": "b", "hasPreview": false}
                        ]
                    },
                    {
                        "question": "Q2?",
                        "header": "Two",
                        "multiSelect": false,
                        "options": [
                            {"label": "C", "description": "c", "hasPreview": false},
                            {"label": "D", "description": "d", "hasPreview": false}
                        ]
                    }
                ]
            })
        );
        // JSON-safe: round-trips through a string.
        let text = serde_json::to_string(&payload).expect("serialize");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("parse"),
            payload
        );
    }

    #[test]
    fn events_blocked_payload_is_boolean_only() {
        assert_eq!(build_blocked_payload(true), json!({"active": true}));
        assert_eq!(build_blocked_payload(false), json!({"active": false}));
    }

    #[test]
    fn events_names_are_immutable() {
        assert_eq!(ASK_USER_PROMPT_EVENT, "rpiv:ask-user:prompt");
        assert_eq!(ASK_USER_BLOCKED_EVENT, "rpiv:ask-user:blocked");
    }
}
