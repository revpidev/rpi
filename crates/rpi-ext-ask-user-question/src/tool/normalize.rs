//! Line-terminator normalization at the tool boundary.
//!
//! Port of upstream `packages/rpiv-ask-user-question/tool/normalize-params.ts`
//! @ `338b264c` (issue #192). Some models serialize a bare carriage return
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
//! dialog walker, the envelope echo and the `rpiv:ask-user:prompt` payload all
//! carry the same clean text. Pure: the input is never mutated.

use crate::tool::types::{OptionData, QuestionData, QuestionParams};

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
