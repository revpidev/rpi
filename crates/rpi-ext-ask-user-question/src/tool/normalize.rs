//! Line-terminator normalization at the tool boundary.
//!
//! Port of upstream `packages/rpiv-ask-user-question/tool/normalize-params.ts`
//! @ `0fdf4f8` (issue #192). Some models serialize a bare carriage return
//! inside tool-call string arguments at token boundaries where the text was
//! meant to be contiguous (`GEMBA\r_LOG\r_FILE` for `GEMBA_LOG_FILE`). A raw
//! CR is a cursor-control byte, not text: pi-tui ≤0.80 writes it straight
//! into the row (later text overwrites the pointer/number) and pi-tui ≥0.84
//! splits `wrapTextWithAnsi` on `\r` (one option fragments into stacked
//! rows). Both symptoms have the same fix at our boundary:
//!
//! - `\r\n` → `\n` keeps genuine multi-line content (preview markdown) intact;
//! - a lone `\r` is deleted — never a space (phantom gaps inside words) and
//!   never `\n` (reintroduces the vertical fragmentation). This matches
//!   pi-coding-agent's own `normalizeDisplayText`.
//!
//! [`normalize_question_params`] runs once at tool entry, BEFORE
//! [`crate::tool::validate::validate_questionnaire`], so the reserved-label
//! and duplicate-label guards compare the text the user will actually see
//! (`"Other\r"` must not slip past `reserved_label`), and so the TUI, the RPC
//! dialog walker, the envelope echo and the `rpi:ask-user:prompt` payload all
//! carry the same clean text. Pure: the input is never mutated.

use crate::tool::types::{OptionData, QuestionData, QuestionParams};
use serde_json::Value;

/// Pi's `prepareArguments` hook: undo double-encoded tool arguments before
/// schema validation (`prepareQuestionArguments`, normalize-params.ts
/// @ v2.13.8, #244). Some tool-call parsers (vLLM/LiteLLM-served models)
/// intermittently emit nested structures as JSON strings — `questions`
/// itself, or its elements. Only a string at an array/object position is
/// decoded, and only when it parses to the expected shape; anything else is
/// passed through untouched so validation still reports the real problem.
/// The same applies one level down to `options`. Well-formed arguments are
/// returned as-is.
pub fn prepare_question_arguments(args: Value) -> Value {
    let Some(object) = args.as_object() else {
        return args;
    };
    let Some(questions) = object.get("questions").cloned().and_then(decode_array) else {
        return args;
    };
    let decoded: Vec<Value> = questions
        .into_iter()
        .map(|question| {
            let question = decode_object(question);
            let Some(question_object) = question.as_object() else {
                return question;
            };
            match question_object
                .get("options")
                .cloned()
                .and_then(decode_array)
            {
                Some(options) => {
                    let mut question = question;
                    if let Some(map) = question.as_object_mut() {
                        map.insert(
                            "options".to_owned(),
                            Value::Array(options.into_iter().map(decode_object).collect()),
                        );
                    }
                    question
                }
                None => question,
            }
        })
        .collect();
    let mut out = object.clone();
    out.insert("questions".to_owned(), Value::Array(decoded));
    Value::Object(out)
}

/// A JSON string holding an array → that array; anything else unchanged.
fn decode_array(value: Value) -> Option<Vec<Value>> {
    let parsed = parse_json(&value).unwrap_or(value);
    match parsed {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

/// A JSON string holding an object → that object; anything else unchanged.
fn decode_object(value: Value) -> Value {
    match parse_json(&value) {
        Some(parsed) if parsed.is_object() => parsed,
        _ => value,
    }
}

fn parse_json(value: &Value) -> Option<Value> {
    let Value::String(text) = value else {
        return None;
    };
    serde_json::from_str(text).ok()
}

/// Normalize line terminators in one model-supplied text field.
pub fn normalize_line_terminators(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "")
}

fn normalize_option(option: &OptionData) -> OptionData {
    OptionData {
        label: normalize_line_terminators(&option.label),
        description: normalize_line_terminators(&option.description),
        // Keys absent from the input (e.g. an omitted `preview`) stay absent
        // so `"preview" in option` checks and `hasPreview` derivations are
        // unchanged.
        preview: option.preview.as_deref().map(normalize_line_terminators),
    }
}

fn normalize_question(question: &QuestionData) -> QuestionData {
    QuestionData {
        question: normalize_line_terminators(&question.question),
        header: normalize_line_terminators(&question.header),
        options: question.options.iter().map(normalize_option).collect(),
        multi_select: question.multi_select,
    }
}

/// Return a copy of the params with every user-facing string field
/// (`question`, `header`, `options[].label`, `options[].description`,
/// `options[].preview`) line-terminator-normalized.
pub fn normalize_question_params(params: &QuestionParams) -> QuestionParams {
    QuestionParams {
        questions: params.questions.iter().map(normalize_question).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prepare_question_arguments_decodes_double_encoded_params() {
        // Well-formed arguments are returned as-is.
        let well_formed = json!({
            "questions": [{
                "question": "Q?", "header": "H",
                "options": [{"label": "A", "description": "a"}]
            }]
        });
        assert_eq!(prepare_question_arguments(well_formed.clone()), well_formed);
        // `questions` itself arrives JSON-encoded as a string.
        let encoded = json!({
            "questions": "[{\"question\": \"Q?\", \"header\": \"H\", \"options\": [{\"label\": \"A\", \"description\": \"a\"}]}]"
        });
        let decoded = prepare_question_arguments(encoded);
        assert_eq!(decoded["questions"][0]["question"], json!("Q?"));
        // One element and its `options` arrive JSON-encoded as strings.
        let nested = json!({
            "questions": ["{\"question\": \"Q?\", \"header\": \"H\", \"options\": \"[{\\\"label\\\": \\\"A\\\", \\\"description\\\": \\\"a\\\"}]\"}"]
        });
        let decoded = prepare_question_arguments(nested);
        assert_eq!(decoded["questions"][0]["question"], json!("Q?"));
        assert_eq!(decoded["questions"][0]["options"][0]["label"], json!("A"));
        // Non-JSON strings and non-object args pass through untouched, so
        // validation still reports the real problem.
        let invalid = json!({"questions": ["not json"]});
        assert_eq!(prepare_question_arguments(invalid.clone()), invalid);
        assert_eq!(prepare_question_arguments(json!("plain")), json!("plain"));
        assert_eq!(prepare_question_arguments(json!(null)), json!(null));
    }

    #[test]
    fn normalize_line_terminators_matches_upstream() {
        assert_eq!(normalize_line_terminators("a\r\nb"), "a\nb");
        assert_eq!(normalize_line_terminators("a\rb"), "ab");
        assert_eq!(normalize_line_terminators("a\r\r\nb"), "a\nb");
        assert_eq!(normalize_line_terminators("plain"), "plain");
        // Astral (emoji) text passes through unchanged.
        assert_eq!(normalize_line_terminators("🦀\r\n🦀"), "🦀\n🦀");
    }

    #[test]
    fn normalize_question_params_touches_every_string_field_only() {
        let params = QuestionParams {
            questions: vec![QuestionData {
                question: "Q\r\n1".to_owned(),
                header: "H\r1".to_owned(),
                options: vec![
                    OptionData {
                        label: "A\r".to_owned(),
                        description: "d\r\n1".to_owned(),
                        preview: Some("p\r1".to_owned()),
                    },
                    OptionData {
                        label: "B".to_owned(),
                        description: "d".to_owned(),
                        preview: None,
                    },
                ],
                multi_select: Some(true),
            }],
        };
        let normalized = normalize_question_params(&params);
        let q = &normalized.questions[0];
        assert_eq!(q.question, "Q\n1");
        assert_eq!(q.header, "H1");
        assert_eq!(q.options[0].label, "A");
        assert_eq!(q.options[0].description, "d\n1");
        assert_eq!(q.options[0].preview.as_deref(), Some("p1"));
        assert_eq!(q.options[1].preview, None, "absent preview stays absent");
        assert_eq!(q.multi_select, Some(true), "non-string field untouched");
        // Pure: the input is unchanged.
        assert_eq!(params.questions[0].question, "Q\r\n1");
    }
}
