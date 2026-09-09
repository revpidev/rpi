//! Tool contract constants, JSON Schema and wire types.
//!
//! Port of upstream `packages/rpiv-ask-user-question/tool/types.ts` @
//! `338b264c` (v2.9.0+). The `parameters` JSON Schema is byte-equal to the
//! TypeBox output (`QuestionParamsSchema`) — asserted by
//! `scripts/ask-user-question-parity` and `types_schema_matches_upstream`.
//!
//! Upstream references:
//! - `tool/types.ts` (constants, `OptionSchema`/`QuestionSchema`/`QuestionsSchema`/
//!   `QuestionParamsSchema`, `RESERVED_LABELS`, `SENTINEL_LABELS`)
//! - `state/row-intent.ts` (`ROW_INTENT_META`/`LABELS_BY_KIND`, re-sourced below)

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Maximum questions per invocation (`MAX_QUESTIONS`).
pub const MAX_QUESTIONS: usize = 4;
/// Minimum options per question (`MIN_OPTIONS`).
pub const MIN_OPTIONS: usize = 2;
/// Maximum options per question (`MAX_OPTIONS`).
pub const MAX_OPTIONS: usize = 4;
/// Maximum `header` length in characters (`MAX_HEADER_LENGTH`).
pub const MAX_HEADER_LENGTH: usize = 16;
/// Maximum option `label` length in characters (`MAX_LABEL_LENGTH`).
pub const MAX_LABEL_LENGTH: usize = 60;

/// Reserved labels, order pinned by upstream `types.test.ts:292`
/// (`["Other", other, next]`) — consumers indexing `RESERVED_LABELS[i]` must
/// see the same order. `"Other"` has no runtime kind (CC-parity reservation);
/// the other two are the runtime sentinel labels from
/// [`crate::state::row_intent::ROW_INTENT_META`].
pub const RESERVED_LABELS: [&str; 3] = ["Other", "Type something.", "Next"];

/// Runtime sentinel labels keyed by kind (`SENTINEL_LABELS`).
pub const SENTINEL_OTHER_LABEL: &str = "Type something.";
/// `next` sentinel label.
pub const SENTINEL_NEXT_LABEL: &str = "Next";

/// `OptionSchema` (TypeBox → JSON Schema).
fn option_schema() -> Value {
    json!({
        "type": "object",
        "required": ["label", "description"],
        "properties": {
            "label": {
                "type": "string",
                "maxLength": MAX_LABEL_LENGTH,
                "description": format!("MAX {MAX_LABEL_LENGTH} CHARACTERS — hard limit, requests over the limit are rejected. The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice."),
            },
            "description": {
                "type": "string",
                "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications.",
            },
            "preview": {
                "type": "string",
                "description": "Optional preview content rendered when this option is focused. Use for mockups, code snippets, or visual comparisons that help users compare options. See the tool description for the expected content format.",
            },
        },
    })
}

/// `QuestionSchema` (TypeBox → JSON Schema).
fn question_schema() -> Value {
    json!({
        "type": "object",
        "required": ["question", "header", "options"],
        "properties": {
            "question": {
                "type": "string",
                "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\"",
            },
            "header": {
                "type": "string",
                "maxLength": MAX_HEADER_LENGTH,
                "description": format!("MAX {MAX_HEADER_LENGTH} CHARACTERS — hard limit, requests over the limit are rejected. Very short chip/tag shown next to the question. Examples: \"Auth method\", \"Library\", \"Approach\"."),
            },
            "options": {
                "type": "array",
                "items": option_schema(),
                "minItems": MIN_OPTIONS,
                "maxItems": MAX_OPTIONS,
                "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). The 'Type something.' row is appended automatically — do NOT author it.",
            },
            "multiSelect": {
                "type": "boolean",
                "default": false,
                "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive.",
            },
        },
    })
}

/// `QuestionParamsSchema` (TypeBox → JSON Schema) — the `registerTool`
/// `parameters` payload. Insertion order matches TypeBox's `JSON.stringify`
/// (byte parity; `serde_json` preserves object order).
pub fn question_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["questions"],
        "properties": {
            "questions": {
                "type": "array",
                "items": question_schema(),
                "minItems": 1,
                "maxItems": MAX_QUESTIONS,
                "description": "Questions to ask the user (1-4 questions)",
            },
        },
    })
}

/// One author-defined option (`OptionData`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OptionData {
    /// Display text (≤ [`MAX_LABEL_LENGTH`] characters, schema-enforced).
    pub label: String,
    /// One-line explanation.
    pub description: String,
    /// Optional rich preview markdown; absent stays absent (`"preview" in option`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One question (`QuestionData`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionData {
    /// Full question text.
    pub question: String,
    /// Short chip/tag (≤ [`MAX_HEADER_LENGTH`] characters).
    pub header: String,
    /// 2–4 author-defined options.
    pub options: Vec<OptionData>,
    /// `Some(true)` = multi-select; absent/`false` = single-select.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_select: Option<bool>,
}

/// Tool parameters (`QuestionParams`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionParams {
    /// 1–[`MAX_QUESTIONS`] questions.
    pub questions: Vec<QuestionData>,
}

/// Answer-intent discriminator (`QuestionAnswer.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    /// User picked one author-defined option; `answer` is the label.
    Option,
    /// User typed free text via the `Type something.` row.
    Custom,
    /// User committed multi-select choices; `selected` carries the labels.
    Multi,
}

/// One answer in the result envelope (`QuestionAnswer`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAnswer {
    /// Index into `params.questions`.
    pub question_index: usize,
    /// The question text (normalized) this answer belongs to.
    pub question: String,
    /// Answer variant.
    pub kind: AnswerKind,
    /// Scalar answer (`null` when the variant has no scalar form).
    pub answer: Option<String>,
    /// Chosen labels for `kind: "multi"`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
    /// Per-question note; key absent when empty (conditional spread upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Matched option's `preview` markdown; single-select only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Runtime/validation error code (`QuestionnaireError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionnaireError {
    /// `ctx.hasUI == false`.
    NoUi,
    /// Host cannot render the component and has no `select`/`input`.
    NoCustomUi,
    /// `questions` is empty.
    NoQuestions,
    /// A question has fewer than [`MIN_OPTIONS`] options.
    EmptyOptions,
    /// More than [`MAX_QUESTIONS`] questions.
    TooManyQuestions,
    /// Two questions share the same text.
    DuplicateQuestion,
    /// Two options in one question share a label.
    DuplicateOptionLabel,
    /// An option uses a reserved label.
    ReservedLabel,
    /// Upstream jiti module-cache failures ([DEFER] in rpi — kept for wire
    /// completeness so a future host can carry the code).
    SessionLoadFailed,
    /// Upstream jiti module-cache staleness ([DEFER] in rpi).
    StaleModuleCache,
}

impl QuestionnaireError {
    /// The wire code (`snake_case`, matches upstream union members).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoUi => "no_ui",
            Self::NoCustomUi => "no_custom_ui",
            Self::NoQuestions => "no_questions",
            Self::EmptyOptions => "empty_options",
            Self::TooManyQuestions => "too_many_questions",
            Self::DuplicateQuestion => "duplicate_question",
            Self::DuplicateOptionLabel => "duplicate_option_label",
            Self::ReservedLabel => "reserved_label",
            Self::SessionLoadFailed => "session_load_failed",
            Self::StaleModuleCache => "stale_module_cache",
        }
    }
}

/// The tool result (`QuestionnaireResult` for `details`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionnaireResult {
    /// Answers collected so far (partial submissions allowed).
    pub answers: Vec<QuestionAnswer>,
    /// `true` when the questionnaire was declined/cancelled/failed.
    pub cancelled: bool,
    /// Global note authored on the Submit tab; key absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_note: Option<String>,
    /// Error code for validation/runtime failures; absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<QuestionnaireError>,
}

impl QuestionnaireResult {
    /// A validation/runtime failure result (`cancelled: true`, empty answers).
    pub fn failure(error: QuestionnaireError) -> Self {
        Self {
            answers: Vec::new(),
            cancelled: true,
            global_note: None,
            error: Some(error),
        }
    }
}

/// `isQuestionnaireResult` guard (upstream `types.ts`) — used by the RPC
/// walker in TE29; kept here so the contract is frozen with the types.
pub fn is_questionnaire_result(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("answers").is_some_and(Value::is_array)
        && object.get("cancelled").is_some_and(Value::is_boolean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_constants_match_upstream() {
        assert_eq!(MAX_QUESTIONS, 4);
        assert_eq!(MIN_OPTIONS, 2);
        assert_eq!(MAX_OPTIONS, 4);
        assert_eq!(MAX_HEADER_LENGTH, 16);
        assert_eq!(MAX_LABEL_LENGTH, 60);
        assert_eq!(RESERVED_LABELS, ["Other", "Type something.", "Next"]);
        assert_eq!(SENTINEL_OTHER_LABEL, "Type something.");
        assert_eq!(SENTINEL_NEXT_LABEL, "Next");
    }

    /// Schema shape against the upstream TypeBox output (`tool/types.ts`).
    /// Byte-level parity with the live TypeBox module is enforced by
    /// `scripts/ask-user-question-parity` (group `schema`, canonical-JSON
    /// diff); this test pins the drift-prone literals in-tree.
    #[test]
    fn types_schema_matches_upstream() {
        let schema = question_params_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["questions"]));
        let questions = &schema["properties"]["questions"];
        assert_eq!(questions["type"], "array");
        assert_eq!(questions["minItems"], 1);
        assert_eq!(questions["maxItems"], MAX_QUESTIONS);
        assert_eq!(
            questions["description"],
            "Questions to ask the user (1-4 questions)"
        );
        let question = &questions["items"];
        assert_eq!(
            question["required"],
            json!(["question", "header", "options"])
        );
        assert_eq!(
            question["properties"]["header"]["maxLength"],
            MAX_HEADER_LENGTH
        );
        assert_eq!(question["properties"]["options"]["minItems"], MIN_OPTIONS);
        assert_eq!(question["properties"]["options"]["maxItems"], MAX_OPTIONS);
        assert_eq!(
            question["properties"]["multiSelect"]["default"],
            json!(false)
        );
        let option = &question["properties"]["options"]["items"];
        assert_eq!(option["required"], json!(["label", "description"]));
        assert_eq!(option["properties"]["label"]["maxLength"], MAX_LABEL_LENGTH);
        assert!(option["properties"]["preview"]["description"]
            .as_str()
            .expect("preview description")
            .starts_with("Optional preview content"));
        // Every property carries a model-facing description (the upstream
        // TypeBox schemas all do).
        for (name, property) in option["properties"].as_object().expect("option properties") {
            assert!(
                property
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some(),
                "option.{name} description"
            );
        }
    }

    #[test]
    fn types_question_params_round_trip_preserves_absent_fields() {
        let json = r#"{"questions":[{"question":"Q?","header":"H","options":[{"label":"A","description":"a"},{"label":"B","description":"b","preview":"p"}],"multiSelect":true}]}"#;
        let params: QuestionParams = serde_json::from_str(json).expect("parse");
        assert_eq!(params.questions[0].multi_select, Some(true));
        assert_eq!(params.questions[0].options[0].preview, None);
        assert_eq!(
            serde_json::to_string(&params).expect("serialize"),
            json,
            "field presence/order preserved"
        );

        let absent: QuestionParams = serde_json::from_str(
            r#"{"questions":[{"question":"Q?","header":"H","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}"#,
        )
        .expect("parse absent multiSelect");
        assert_eq!(absent.questions[0].multi_select, None);
        assert_eq!(
            serde_json::to_string(&absent).expect("serialize"),
            r#"{"questions":[{"question":"Q?","header":"H","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}"#,
            "absent multiSelect stays absent"
        );
    }

    #[test]
    fn types_error_codes_match_upstream_union() {
        let codes = [
            (QuestionnaireError::NoUi, "no_ui"),
            (QuestionnaireError::NoCustomUi, "no_custom_ui"),
            (QuestionnaireError::NoQuestions, "no_questions"),
            (QuestionnaireError::EmptyOptions, "empty_options"),
            (QuestionnaireError::TooManyQuestions, "too_many_questions"),
            (QuestionnaireError::DuplicateQuestion, "duplicate_question"),
            (
                QuestionnaireError::DuplicateOptionLabel,
                "duplicate_option_label",
            ),
            (QuestionnaireError::ReservedLabel, "reserved_label"),
            (QuestionnaireError::SessionLoadFailed, "session_load_failed"),
            (QuestionnaireError::StaleModuleCache, "stale_module_cache"),
        ];
        for (error, code) in codes {
            assert_eq!(error.as_str(), code);
            assert_eq!(
                serde_json::to_value(error).expect("serialize"),
                Value::String(code.to_owned())
            );
        }
    }

    #[test]
    fn types_is_questionnaire_result_guard() {
        assert!(is_questionnaire_result(
            &json!({"answers": [], "cancelled": false})
        ));
        assert!(!is_questionnaire_result(&json!({"answers": []})));
        assert!(!is_questionnaire_result(&json!(null)));
    }
}
