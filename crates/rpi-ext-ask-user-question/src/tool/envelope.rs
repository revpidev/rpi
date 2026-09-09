//! Result envelope + answer scalar formatting.
//!
//! Port of upstream `packages/rpiv-ask-user-question/tool/response-envelope.ts`
//! and `tool/format-answer.ts` @ `338b264c`. Both are pure of
//! `(result, params)` / `(answer)` and are the LLM-facing contract: text and
//! `details` must stay byte/field-identical to upstream (R-Q3, 附录 C).

use serde_json::{json, Value};

use crate::tool::types::{AnswerKind, QuestionAnswer, QuestionParams, QuestionnaireResult};

/// `DECLINE_MESSAGE` (upstream literal).
pub const DECLINE_MESSAGE: &str = "User declined to answer questions";
/// `ENVELOPE_PREFIX` (upstream literal).
pub const ENVELOPE_PREFIX: &str = "User has answered your questions:";
/// `ENVELOPE_SUFFIX` (upstream literal).
pub const ENVELOPE_SUFFIX: &str = "You can now continue with the user's answers in mind.";
/// `NO_INPUT_PLACEHOLDER` (upstream literal).
pub const NO_INPUT_PLACEHOLDER: &str = "(no input)";

/// `FormatAnswerVariant` — retained on the signature for upstream stability
/// (all branches ignore it today).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatAnswerVariant {
    /// Chat/envelope rendering.
    Envelope,
    /// Dialog summary rendering (no live caller in Q0).
    Summary,
}

/// Format a `QuestionAnswer` to its scalar string form
/// (`formatAnswerScalar`). Switch is exhaustive — non-void return enforces
/// every variant is handled.
pub fn format_answer_scalar(answer: &QuestionAnswer, _variant: FormatAnswerVariant) -> String {
    match answer.kind {
        AnswerKind::Multi => answer
            .selected
            .as_ref()
            .filter(|selected| !selected.is_empty())
            .map(|selected| selected.join(", "))
            .unwrap_or_else(|| NO_INPUT_PLACEHOLDER.to_owned()),
        AnswerKind::Custom => answer
            .answer
            .as_ref()
            .filter(|text| !text.is_empty())
            .cloned()
            .unwrap_or_else(|| NO_INPUT_PLACEHOLDER.to_owned()),
        // `a.answer ?? NO_INPUT_PLACEHOLDER`: an empty-string option answer
        // stays empty (only null/absent becomes the placeholder).
        AnswerKind::Option => answer
            .answer
            .clone()
            .unwrap_or_else(|| NO_INPUT_PLACEHOLDER.to_owned()),
    }
}

/// Format one answer segment (`buildAnswerSegment`). The `"Q"="A"` shape and
/// the optional `selected preview:` / `user notes:` suffixes are pinned by
/// the envelope fixtures.
pub fn build_answer_segment(answer: &QuestionAnswer) -> String {
    let mut parts = vec![format!(
        "\"{}\"=\"{}\"",
        answer.question,
        format_answer_scalar(answer, FormatAnswerVariant::Envelope)
    )];
    if let Some(preview) = answer.preview.as_ref().filter(|text| !text.is_empty()) {
        parts.push(format!("selected preview: {preview}"));
    }
    if let Some(notes) = answer.notes.as_ref().filter(|text| !text.is_empty()) {
        parts.push(format!("user notes: {notes}"));
    }
    format!("{}.", parts.join(". "))
}

/// Wrap text + details into the tool result shape (`buildToolResult`).
pub fn build_tool_result(text: impl Into<String>, details: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "details": details,
    })
}

/// Canonical decline envelope: `User declined to answer questions` + the
/// result's answers/globalNote when a result rode along.
fn decline_envelope(result: Option<&QuestionnaireResult>) -> Value {
    let answers = result
        .map(|result| serde_json::to_value(&result.answers).unwrap_or_else(|_| json!([])))
        .unwrap_or_else(|| json!([]));
    let mut details = json!({
        "answers": answers,
        "cancelled": true,
    });
    if let Some(global_note) = result
        .and_then(|result| result.global_note.as_ref())
        .filter(|note| !note.is_empty())
    {
        details["globalNote"] = json!(global_note);
    }
    build_tool_result(DECLINE_MESSAGE, details)
}

/// Map a `QuestionnaireResult` (or `None`/cancelled) to the LLM-facing tool
/// envelope (`buildQuestionnaireResponse`). Cancelled and "no segments" both
/// fall to [`DECLINE_MESSAGE`] so the model sees a single canonical "didn't
/// answer" signal regardless of why. The `global note:` segment is pushed
/// before the zero-segments check, so a note-bearing submit with zero answers
/// still yields the answered envelope.
pub fn build_questionnaire_response(
    result: Option<&QuestionnaireResult>,
    params: &QuestionParams,
) -> Value {
    let Some(result) = result else {
        return decline_envelope(None);
    };
    if result.cancelled {
        // Decline text stays canonical even when a global note rides the
        // cancelled result; the note survives in `details` (like partial
        // `answers`) for replay consumers.
        return decline_envelope(Some(result));
    }

    let mut segments: Vec<String> = Vec::new();
    for (index, _question) in params.questions.iter().enumerate() {
        if let Some(answer) = result
            .answers
            .iter()
            .find(|answer| answer.question_index == index)
        {
            segments.push(build_answer_segment(answer));
        }
    }
    // Global note rides after the per-question segments: raw multiline echo
    // (no reformatting), trailing period mirroring `buildAnswerSegment`.
    if let Some(global_note) = result.global_note.as_ref().filter(|note| !note.is_empty()) {
        segments.push(format!("global note: {global_note}."));
    }
    if segments.is_empty() {
        return build_tool_result(
            DECLINE_MESSAGE,
            json!({
                "answers": serde_json::to_value(&result.answers).unwrap_or_else(|_| json!([])),
                "cancelled": true,
            }),
        );
    }
    let text = format!("{ENVELOPE_PREFIX} {} {ENVELOPE_SUFFIX}", segments.join(" "));
    build_tool_result(text, serde_json::to_value(result).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::{OptionData, QuestionData};

    fn params(questions: &[(&str, bool)]) -> QuestionParams {
        QuestionParams {
            questions: questions
                .iter()
                .map(|(text, multi)| QuestionData {
                    question: (*text).to_owned(),
                    header: "H".to_owned(),
                    options: vec![
                        OptionData {
                            label: "A".to_owned(),
                            description: "a".to_owned(),
                            preview: None,
                        },
                        OptionData {
                            label: "B".to_owned(),
                            description: "b".to_owned(),
                            preview: None,
                        },
                    ],
                    multi_select: Some(*multi),
                })
                .collect(),
        }
    }

    fn option_answer(index: usize, question: &str, answer: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            question_index: index,
            question: question.to_owned(),
            kind: AnswerKind::Option,
            answer: answer.map(str::to_owned),
            selected: None,
            notes: None,
            preview: None,
        }
    }

    #[test]
    fn envelope_matches_upstream_response_envelope() {
        let params = params(&[("Q1?", false), ("Q2?", true)]);

        // Answered single option.
        let result = QuestionnaireResult {
            answers: vec![option_answer(0, "Q1?", Some("A"))],
            cancelled: false,
            global_note: None,
            error: None,
        };
        let value = build_questionnaire_response(Some(&result), &params);
        assert_eq!(
            value["content"][0]["text"],
            json!("User has answered your questions: \"Q1?\"=\"A\". You can now continue with the user's answers in mind.")
        );
        assert_eq!(value["details"]["cancelled"], json!(false));
        assert_eq!(value["details"]["answers"][0]["kind"], json!("option"));

        // Cancelled / absent / no segments -> canonical decline.
        for result in [
            None,
            Some(&QuestionnaireResult::failure(
                crate::tool::types::QuestionnaireError::NoQuestions,
            )),
        ] {
            let value = build_questionnaire_response(result, &params);
            assert_eq!(value["content"][0]["text"], json!(DECLINE_MESSAGE));
            assert_eq!(value["details"]["cancelled"], json!(true));
        }
        let empty = QuestionnaireResult {
            answers: vec![],
            cancelled: false,
            global_note: None,
            error: None,
        };
        assert_eq!(
            build_questionnaire_response(Some(&empty), &params)["content"][0]["text"],
            json!(DECLINE_MESSAGE)
        );

        // Global note with zero answers still counts as answered.
        let note_only = QuestionnaireResult {
            answers: vec![],
            cancelled: false,
            global_note: Some("hello\nworld".to_owned()),
            error: None,
        };
        let value = build_questionnaire_response(Some(&note_only), &params);
        assert_eq!(
            value["content"][0]["text"],
            json!(format!(
                "{ENVELOPE_PREFIX} global note: hello\nworld. {ENVELOPE_SUFFIX}"
            ))
        );

        // Cancelled with a global note: decline text, note preserved in details.
        let cancelled_note = QuestionnaireResult {
            answers: vec![],
            cancelled: true,
            global_note: Some("kept".to_owned()),
            error: None,
        };
        let value = build_questionnaire_response(Some(&cancelled_note), &params);
        assert_eq!(value["content"][0]["text"], json!(DECLINE_MESSAGE));
        assert_eq!(value["details"]["globalNote"], json!("kept"));
    }

    #[test]
    fn envelope_answer_segment_variants_and_placeholders() {
        let mut multi = option_answer(0, "Q", None);
        multi.kind = AnswerKind::Multi;
        multi.selected = Some(vec!["A".to_owned(), "B".to_owned()]);
        assert_eq!(build_answer_segment(&multi), "\"Q\"=\"A, B\".");

        let mut multi_empty = option_answer(0, "Q", None);
        multi_empty.kind = AnswerKind::Multi;
        multi_empty.selected = Some(vec![]);
        assert_eq!(build_answer_segment(&multi_empty), "\"Q\"=\"(no input)\".");

        let mut custom = option_answer(0, "Q", Some("typed"));
        custom.kind = AnswerKind::Custom;
        assert_eq!(build_answer_segment(&custom), "\"Q\"=\"typed\".");

        let mut custom_empty = option_answer(0, "Q", Some(""));
        custom_empty.kind = AnswerKind::Custom;
        assert_eq!(build_answer_segment(&custom_empty), "\"Q\"=\"(no input)\".");

        // `kind: "option"` keeps an empty string (upstream `answer ?? NO_INPUT`).
        let mut option_empty = option_answer(0, "Q", Some(""));
        option_empty.kind = AnswerKind::Option;
        assert_eq!(build_answer_segment(&option_empty), "\"Q\"=\"\".");

        let mut null_option = option_answer(0, "Q", None);
        null_option.kind = AnswerKind::Option;
        assert_eq!(build_answer_segment(&null_option), "\"Q\"=\"(no input)\".");

        // Suffixes: preview then notes, in that order.
        let mut suffixed = option_answer(0, "Q", Some("A"));
        suffixed.preview = Some("preview md".to_owned());
        suffixed.notes = Some("note".to_owned());
        assert_eq!(
            build_answer_segment(&suffixed),
            "\"Q\"=\"A\". selected preview: preview md. user notes: note."
        );
        // Empty-string preview/notes do not emit suffixes.
        let mut empty_suffix = option_answer(0, "Q", Some("A"));
        empty_suffix.preview = Some(String::new());
        empty_suffix.notes = Some(String::new());
        assert_eq!(build_answer_segment(&empty_suffix), "\"Q\"=\"A\".");
    }

    #[test]
    fn envelope_partial_submission_skips_unanswered_questions() {
        let params = params(&[("Q1?", false), ("Q2?", false)]);
        let result = QuestionnaireResult {
            answers: vec![option_answer(1, "Q2?", Some("B"))],
            cancelled: false,
            global_note: None,
            error: None,
        };
        let value = build_questionnaire_response(Some(&result), &params);
        assert_eq!(
            value["content"][0]["text"],
            json!("User has answered your questions: \"Q2?\"=\"B\". You can now continue with the user's answers in mind.")
        );
    }
}
