//! Inbound answer validation (ADR-0043 / upstream `tool/external-answer.ts`).
//!
//! Turns an untrusted `rpi:ask-user:answer` payload into the
//! [`QuestionnaireResult`] the dialog would have produced. Pure of
//! `(questions, payload)`: the answer channel validates before settling, and a
//! failure never changes questionnaire state.
//!
//! Strict by design (the contract can be relaxed later without breaking
//! emitters, never tightened): `invalid_payload` is a wrong shape or type,
//! `invalid_answer` a well-formed answer that does not fit these questions.

use serde_json::{Map, Value};

use crate::tool::types::{AnswerKind, QuestionAnswer, QuestionData, QuestionnaireResult};

/// One failed attempt's machine-readable code + human reason.
pub type Reject = (String, String);

/// Frame-safety bound for inbound free text. Upstream has no explicit cap;
/// the rpi guest settles by rendering a `done` frame (1 MiB / 32 KiB-line
/// host limit), so an unbounded answer could be accepted (`ok`) and then fail
/// to render. 8 KiB leaves ample room for notes/global notes.
pub const MAX_EXTERNAL_TEXT_BYTES: usize = 8 * 1024;

fn text_too_long(text: &str) -> bool {
    text.len() > MAX_EXTERNAL_TEXT_BYTES
}

fn reject(code: &str, reason: impl Into<String>) -> Reject {
    (code.to_owned(), reason.into())
}

/// `Number.isInteger`: JSON `0` and `0.0` are both integers upstream. Rust's
/// `as_i64` only accepts the integer representation, so askq used to reject
/// the integral-float spelling as `invalid_payload`.
fn json_integer(value: &Value) -> Option<f64> {
    let number = if let Some(integer) = value.as_i64() {
        integer as f64
    } else {
        value.as_f64()?
    };
    (number.is_finite() && number.fract() == 0.0).then_some(number)
}

fn is_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// A note as the TUI stores it: trimmed, absent when blank.
fn note_of(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(str::to_string)
}

/// Resolve one answer entry (`resolveEntry`).
fn resolve_entry(
    question: &QuestionData,
    index: usize,
    entry: &Map<String, Value>,
) -> Result<QuestionAnswer, Reject> {
    let option_indexes = match entry.get("optionIndexes") {
        None => None,
        Some(Value::Array(items)) => {
            let mut indexes = Vec::with_capacity(items.len());
            for item in items {
                let Some(value) = json_integer(item) else {
                    return Err(reject(
                        "invalid_payload",
                        format!("question {index}: optionIndexes must be an array of integers"),
                    ));
                };
                indexes.push(value);
            }
            Some(indexes)
        }
        Some(_) => {
            return Err(reject(
                "invalid_payload",
                format!("question {index}: optionIndexes must be an array of integers"),
            ));
        }
    };
    let text = match entry.get("text") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(reject(
                "invalid_payload",
                format!("question {index}: text must be a string"),
            ));
        }
    };
    let notes = match entry.get("notes") {
        None => None,
        Some(Value::String(notes)) => Some(notes.clone()),
        Some(_) => {
            return Err(reject(
                "invalid_payload",
                format!("question {index}: notes must be a string"),
            ));
        }
    };

    if text.as_deref().is_some_and(text_too_long) {
        return Err(reject(
            "invalid_payload",
            format!("question {index}: text exceeds {MAX_EXTERNAL_TEXT_BYTES} bytes"),
        ));
    }
    if notes.as_deref().is_some_and(text_too_long) {
        return Err(reject(
            "invalid_payload",
            format!("question {index}: notes exceed {MAX_EXTERNAL_TEXT_BYTES} bytes"),
        ));
    }

    let invalid = |reason: String| reject("invalid_answer", format!("question {index}: {reason}"));
    let picked_values = option_indexes.unwrap_or_default();
    let option_count = question.options.len() as f64;
    if let Some(out) = picked_values
        .iter()
        .find(|value| **value < 0.0 || **value >= option_count)
    {
        return Err(invalid(format!(
            "option index {out} out of range (0..{})",
            question.options.len().saturating_sub(1)
        )));
    }
    let picked: Vec<i64> = picked_values.iter().map(|value| *value as i64).collect();
    let mut sorted = picked.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != picked.len() {
        return Err(invalid("option index repeated".to_string()));
    }
    if let Some(text) = text.as_deref()
        && text.trim().is_empty()
    {
        return Err(invalid("text is blank".to_string()));
    }
    if picked.is_empty() && text.is_none() {
        return Err(invalid("needs optionIndexes or text".to_string()));
    }

    let note = note_of(notes.as_deref());
    let base = (index, question.question.clone());
    if question.multi_select == Some(true) {
        let selected: Vec<String> = question
            .options
            .iter()
            .enumerate()
            .filter(|(position, _)| picked.contains(&(*position as i64)))
            .map(|(_, option)| option.label.clone())
            .collect();
        return Ok(QuestionAnswer {
            question_index: base.0,
            question: base.1,
            kind: AnswerKind::Multi,
            answer: text,
            selected: Some(selected),
            notes: note,
            preview: None,
        });
    }
    if !picked.is_empty() && text.is_some() {
        return Err(invalid(
            "single-select takes optionIndexes or text, not both".to_string(),
        ));
    }
    if picked.len() > 1 {
        return Err(invalid(format!(
            "single-select takes one option, got {}",
            picked.len()
        )));
    }
    if let Some(text) = text {
        return Ok(QuestionAnswer {
            question_index: base.0,
            question: base.1,
            kind: AnswerKind::Custom,
            answer: Some(text),
            selected: None,
            notes: note,
            preview: None,
        });
    }
    let option = &question.options[picked[0] as usize];
    Ok(QuestionAnswer {
        question_index: base.0,
        question: base.1,
        kind: AnswerKind::Option,
        answer: Some(option.label.clone()),
        selected: None,
        notes: note,
        preview: option.preview.clone().filter(|preview| !preview.is_empty()),
    })
}

/// Turn an inbound payload (`AskUserAnswerEventPayload`, still untrusted) into
/// a [`QuestionnaireResult`]. All-or-nothing: any failure rejects the attempt.
pub fn resolve_external_answer(
    questions: &[QuestionData],
    payload: &Value,
) -> Result<QuestionnaireResult, Reject> {
    let Some(payload) = is_record(payload) else {
        return Err(reject("invalid_payload", "payload must be an object"));
    };
    let cancel = match payload.get("cancel") {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => return Err(reject("invalid_payload", "cancel must be a boolean")),
    };
    let global_note_raw = match payload.get("globalNote") {
        None => None,
        Some(Value::String(note)) => Some(note.clone()),
        Some(_) => {
            return Err(reject("invalid_payload", "globalNote must be a string"));
        }
    };
    if global_note_raw.as_deref().is_some_and(text_too_long) {
        return Err(reject(
            "invalid_payload",
            format!("globalNote exceeds {MAX_EXTERNAL_TEXT_BYTES} bytes"),
        ));
    }
    let global_note = note_of(global_note_raw.as_deref());

    if cancel == Some(true) {
        if let Some(answers) = payload.get("answers")
            && !(answers.is_array() && answers.as_array().is_some_and(Vec::is_empty))
        {
            return Err(reject("invalid_payload", "cancel takes no answers"));
        }
        return Ok(QuestionnaireResult {
            answers: Vec::new(),
            cancelled: true,
            global_note,
            error: None,
        });
    }

    let Some(answers) = payload.get("answers").and_then(Value::as_array) else {
        return Err(reject("invalid_payload", "answers must be an array"));
    };

    let mut by_question: std::collections::BTreeMap<usize, &Map<String, Value>> =
        std::collections::BTreeMap::new();
    for entry in answers {
        let Some(entry) = is_record(entry) else {
            return Err(reject("invalid_payload", "each answer must be an object"));
        };
        let Some(index_value) = entry.get("questionIndex").and_then(json_integer) else {
            return Err(reject(
                "invalid_payload",
                "questionIndex must be an integer",
            ));
        };
        if index_value < 0.0 || index_value >= questions.len() as f64 {
            return Err(reject(
                "invalid_answer",
                format!(
                    "questionIndex {index_value} out of range (0..{})",
                    questions.len().saturating_sub(1)
                ),
            ));
        }
        let index = index_value as usize;
        if by_question.insert(index, entry).is_some() {
            return Err(reject(
                "invalid_answer",
                format!("question {index} answered twice"),
            ));
        }
    }

    let mut resolved = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        let Some(entry) = by_question.get(&index) else {
            return Err(reject(
                "invalid_answer",
                format!("no answer for question {index} (\"{}\")", question.header),
            ));
        };
        resolved.push(resolve_entry(question, index, entry)?);
    }
    Ok(QuestionnaireResult {
        answers: resolved,
        cancelled: false,
        global_note,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;
    use serde_json::json;

    fn option(label: &str) -> OptionData {
        OptionData {
            label: label.to_owned(),
            description: label.to_lowercase(),
            preview: None,
        }
    }

    fn questions() -> Vec<QuestionData> {
        vec![
            QuestionData {
                question: "Pick one".to_owned(),
                header: "H".to_owned(),
                options: vec![option("A"), option("B")],
                multi_select: None,
            },
            QuestionData {
                question: "Pick many".to_owned(),
                header: "HM".to_owned(),
                options: vec![option("X"), option("Y")],
                multi_select: Some(true),
            },
        ]
    }

    #[test]
    fn accepts_a_complete_all_or_nothing_answer() {
        let result = resolve_external_answer(
            &questions(),
            &json!({
                "toolCallId": "call-1",
                "answers": [
                    {"questionIndex": 0, "optionIndexes": [1], "notes": "  keep  "},
                    {"questionIndex": 1, "optionIndexes": [0, 1], "text": " own "}
                ],
                "globalNote": " overall "
            }),
        )
        .expect("valid");
        assert!(!result.cancelled);
        assert_eq!(result.answers.len(), 2);
        assert_eq!(result.answers[0].kind, AnswerKind::Option);
        assert_eq!(result.answers[0].answer.as_deref(), Some("B"));
        assert_eq!(result.answers[0].notes.as_deref(), Some("keep"));
        assert_eq!(result.answers[1].kind, AnswerKind::Multi);
        assert_eq!(
            result.answers[1].selected.as_deref(),
            Some(["X".to_string(), "Y".to_string()].as_slice())
        );
        assert_eq!(result.answers[1].answer.as_deref(), Some(" own "));
        assert_eq!(result.global_note.as_deref(), Some("overall"));
    }

    #[test]
    fn cancel_takes_no_answers_and_rejects_any() {
        let result =
            resolve_external_answer(&questions(), &json!({"cancel": true, "globalNote": "n"}))
                .expect("cancel");
        assert!(result.cancelled);
        assert_eq!(result.global_note.as_deref(), Some("n"));
        let (code, _) = resolve_external_answer(
            &questions(),
            &json!({"cancel": true, "answers": [{"questionIndex": 0}]}),
        )
        .expect_err("cancel with answers");
        assert_eq!(code, "invalid_payload");
    }

    #[test]
    fn accepts_integral_json_floats_for_indexes() {
        let result = resolve_external_answer(
            &questions(),
            &json!({
                "answers": [
                    {"questionIndex": 0.0, "optionIndexes": [1.0]},
                    {"questionIndex": 1.0, "optionIndexes": [0.0, 1.0]}
                ]
            }),
        )
        .expect("integral floats are integers upstream");
        assert_eq!(result.answers[0].answer.as_deref(), Some("B"));
        assert_eq!(
            result.answers[1].selected.as_deref(),
            Some(["X".to_string(), "Y".to_string()].as_slice())
        );
    }

    #[test]
    fn fractional_and_out_of_range_numbers_keep_their_error_codes() {
        let (code, _) = resolve_external_answer(
            &questions(),
            &json!({"answers": [
                {"questionIndex": 0.5},
                {"questionIndex": 1, "text": "x"}
            ]}),
        )
        .expect_err("fractional index");
        assert_eq!(code, "invalid_payload");
        let (code, _) = resolve_external_answer(
            &questions(),
            &json!({"answers": [
                {"questionIndex": 0, "optionIndexes": [0.5]},
                {"questionIndex": 1, "text": "x"}
            ]}),
        )
        .expect_err("fractional option index");
        assert_eq!(code, "invalid_payload");
        let (code, _) = resolve_external_answer(
            &questions(),
            &json!({"answers": [
                {"questionIndex": 0, "optionIndexes": [1e20]},
                {"questionIndex": 1, "text": "x"}
            ]}),
        )
        .expect_err("out of range");
        assert_eq!(code, "invalid_answer");
    }

    #[test]
    fn rejects_shape_and_range_problems() {
        for (payload, code) in [
            (json!(null), "invalid_payload"),
            (json!({"cancel": "yes"}), "invalid_payload"),
            (json!({"cancel": null}), "invalid_payload"),
            (json!({"globalNote": null}), "invalid_payload"),
            (
                json!({"answers": [
                    {"questionIndex": 0, "text": null},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_payload",
            ),
            (json!({"globalNote": 3}), "invalid_payload"),
            (json!({}), "invalid_payload"),
            (json!({"answers": [{"questionIndex": 0}]}), "invalid_answer"),
            (
                json!({"answers": [{"questionIndex": 9, "optionIndexes": [0]}]}),
                "invalid_answer",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "optionIndexes": [0]},
                    {"questionIndex": 0, "optionIndexes": [1]},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_answer",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "optionIndexes": [0, 1]},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_answer",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "optionIndexes": [3]},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_answer",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "text": "   "},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_answer",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "text": "x".repeat(MAX_EXTERNAL_TEXT_BYTES + 1)},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_payload",
            ),
            (
                json!({"answers": [
                    {"questionIndex": 0, "optionIndexes": [0], "notes": "x".repeat(MAX_EXTERNAL_TEXT_BYTES + 1)},
                    {"questionIndex": 1, "text": "x"}
                ]}),
                "invalid_payload",
            ),
            (
                json!({
                    "answers": [
                        {"questionIndex": 0, "optionIndexes": [0]},
                        {"questionIndex": 1, "text": "x"}
                    ],
                    "globalNote": "x".repeat(MAX_EXTERNAL_TEXT_BYTES + 1)
                }),
                "invalid_payload",
            ),
        ] {
            let (actual, _) =
                resolve_external_answer(&questions(), &payload).expect_err(&format!("{payload}"));
            assert_eq!(actual, code, "{payload}");
        }
    }
}
