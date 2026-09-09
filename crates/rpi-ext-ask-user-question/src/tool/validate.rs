//! Pure runtime validator for `QuestionParams`.
//!
//! Port of upstream `packages/rpiv-ask-user-question/tool/validate-questionnaire.ts`
//! @ `338b264c`. Covers every guard except `no_ui` (which depends on
//! `ctx.hasUI` and stays inline at the call site). `reserved_label` MUST
//! short-circuit before `duplicate_option_label` (upstream `:17`).

use crate::tool::types::{
    QuestionParams, QuestionnaireError, MAX_QUESTIONS, MIN_OPTIONS, RESERVED_LABELS,
};

/// `ERROR_NO_QUESTIONS` (upstream literal).
pub const ERROR_NO_QUESTIONS: &str = "Error: At least one question is required";
/// `ERROR_TOO_MANY_QUESTIONS` (upstream literal, `MAX_QUESTIONS` interpolated).
pub fn error_too_many_questions() -> String {
    format!("Error: At most {MAX_QUESTIONS} questions are allowed per invocation")
}
/// `ERROR_DUPLICATE_QUESTION` (upstream literal).
pub const ERROR_DUPLICATE_QUESTION: &str =
    "Error: Question text must be unique within an invocation";
/// `ERROR_TOO_FEW_OPTIONS` (upstream literal, `MIN_OPTIONS` interpolated).
pub fn error_too_few_options() -> String {
    format!("Error: Each question requires at least {MIN_OPTIONS} options")
}
/// `ERROR_RESERVED_LABEL` (upstream literal, `RESERVED_LABELS.join(", ")`).
pub fn error_reserved_label() -> String {
    format!(
        "Error: Option label is reserved ({})",
        RESERVED_LABELS.join(", ")
    )
}
/// `ERROR_DUPLICATE_OPTION_LABEL` (upstream literal).
pub const ERROR_DUPLICATE_OPTION_LABEL: &str =
    "Error: Option labels must be unique within a question";

/// Outcome of [`validate_questionnaire`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationResult {
    /// All guards passed.
    Ok,
    /// First failing guard (upstream returns on the first failure).
    Failed {
        /// Wire error code.
        error: QuestionnaireError,
        /// Model-facing message (byte-equal to upstream).
        message: String,
    },
}

impl ValidationResult {
    /// Whether the params passed validation.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// The error code when validation failed.
    pub fn error(&self) -> Option<QuestionnaireError> {
        match self {
            Self::Ok => None,
            Self::Failed { error, .. } => Some(*error),
        }
    }

    /// The model-facing message when validation failed.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Failed { message, .. } => Some(message),
        }
    }
}

fn is_reserved(label: &str) -> bool {
    RESERVED_LABELS.contains(&label)
}

/// Validate the normalized params. Guard order (upstream verbatim):
/// `no_questions` → `too_many_questions` → `duplicate_question` (all
/// questions) → per question `empty_options` → per option `reserved_label`
/// before `duplicate_option_label`.
pub fn validate_questionnaire(params: &QuestionParams) -> ValidationResult {
    if params.questions.is_empty() {
        return ValidationResult::Failed {
            error: QuestionnaireError::NoQuestions,
            message: ERROR_NO_QUESTIONS.to_owned(),
        };
    }
    if params.questions.len() > MAX_QUESTIONS {
        return ValidationResult::Failed {
            error: QuestionnaireError::TooManyQuestions,
            message: error_too_many_questions(),
        };
    }

    let mut seen_questions: Vec<&str> = Vec::new();
    for question in &params.questions {
        if seen_questions.contains(&question.question.as_str()) {
            return ValidationResult::Failed {
                error: QuestionnaireError::DuplicateQuestion,
                message: ERROR_DUPLICATE_QUESTION.to_owned(),
            };
        }
        seen_questions.push(&question.question);
    }

    for question in &params.questions {
        if question.options.len() < MIN_OPTIONS {
            return ValidationResult::Failed {
                error: QuestionnaireError::EmptyOptions,
                message: error_too_few_options(),
            };
        }
        let mut seen_labels: Vec<&str> = Vec::new();
        for option in &question.options {
            if is_reserved(&option.label) {
                return ValidationResult::Failed {
                    error: QuestionnaireError::ReservedLabel,
                    message: error_reserved_label(),
                };
            }
            if seen_labels.contains(&option.label.as_str()) {
                return ValidationResult::Failed {
                    error: QuestionnaireError::DuplicateOptionLabel,
                    message: ERROR_DUPLICATE_OPTION_LABEL.to_owned(),
                };
            }
            seen_labels.push(&option.label);
        }
    }

    ValidationResult::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::{OptionData, QuestionData};

    fn option(label: &str) -> OptionData {
        OptionData {
            label: label.to_owned(),
            description: "d".to_owned(),
            preview: None,
        }
    }

    fn question(text: &str, labels: &[&str]) -> QuestionData {
        QuestionData {
            question: text.to_owned(),
            header: "H".to_owned(),
            options: labels.iter().map(|l| option(l)).collect(),
            multi_select: None,
        }
    }

    fn params(questions: Vec<QuestionData>) -> QuestionParams {
        QuestionParams { questions }
    }

    #[test]
    fn validate_matches_upstream_validate_questionnaire() {
        // Happy path (also the 1-question / 2-option lower boundaries).
        assert_eq!(
            validate_questionnaire(&params(vec![question("Q", &["A", "B"])])),
            ValidationResult::Ok
        );
        // no_questions
        let failure = validate_questionnaire(&params(vec![]));
        assert_eq!(failure.error(), Some(QuestionnaireError::NoQuestions));
        assert_eq!(failure.message(), Some(ERROR_NO_QUESTIONS));
        // too_many_questions (5 > MAX_QUESTIONS)
        let failure = validate_questionnaire(&params(
            (0..5)
                .map(|i| question(&format!("Q{i}"), &["A", "B"]))
                .collect(),
        ));
        assert_eq!(failure.error(), Some(QuestionnaireError::TooManyQuestions));
        assert_eq!(
            failure.message(),
            Some("Error: At most 4 questions are allowed per invocation")
        );
        // duplicate_question (duplicate check spans all questions first)
        let failure = validate_questionnaire(&params(vec![
            question("Q", &["A", "B"]),
            question("Q", &["C", "D"]),
        ]));
        assert_eq!(failure.error(), Some(QuestionnaireError::DuplicateQuestion));
        assert_eq!(failure.message(), Some(ERROR_DUPLICATE_QUESTION));
        // empty_options (1 option < MIN_OPTIONS)
        let failure = validate_questionnaire(&params(vec![question("Q", &["A"])]));
        assert_eq!(failure.error(), Some(QuestionnaireError::EmptyOptions));
        assert_eq!(
            failure.message(),
            Some("Error: Each question requires at least 2 options")
        );
        // reserved_label
        let failure = validate_questionnaire(&params(vec![question("Q", &["A", "Next"])]));
        assert_eq!(failure.error(), Some(QuestionnaireError::ReservedLabel));
        assert_eq!(
            failure.message(),
            Some("Error: Option label is reserved (Other, Type something., Next)")
        );
        // duplicate_option_label
        let failure = validate_questionnaire(&params(vec![question("Q", &["A", "A"])]));
        assert_eq!(
            failure.error(),
            Some(QuestionnaireError::DuplicateOptionLabel)
        );
        assert_eq!(failure.message(), Some(ERROR_DUPLICATE_OPTION_LABEL));
    }

    /// `reserved_label` MUST short-circuit before `duplicate_option_label`
    /// (upstream `validate-questionnaire.ts:17`).
    #[test]
    fn validate_reserved_label_precedes_duplicate_option_label() {
        let failure = validate_questionnaire(&params(vec![question("Q", &["Next", "Next"])]));
        assert_eq!(failure.error(), Some(QuestionnaireError::ReservedLabel));
    }

    /// The duplicate-question scan runs across all questions before the
    /// per-question option guards (upstream order).
    #[test]
    fn validate_duplicate_question_precedes_empty_options() {
        let failure = validate_questionnaire(&params(vec![
            question("Q", &["A"]),
            question("Q", &["A", "B"]),
        ]));
        assert_eq!(failure.error(), Some(QuestionnaireError::DuplicateQuestion));
    }

    #[test]
    fn validate_result_helpers() {
        let ok = validate_questionnaire(&params(vec![question("Q", &["A", "B"])]));
        assert!(ok.is_ok());
        assert_eq!(ok.error(), None);
        assert_eq!(ok.message(), None);

        let failed = validate_questionnaire(&params(vec![]));
        assert!(!failed.is_ok());
        assert_eq!(failed.error(), Some(QuestionnaireError::NoQuestions));
    }
}
