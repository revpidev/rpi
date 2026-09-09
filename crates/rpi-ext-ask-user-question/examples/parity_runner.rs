//! Rust leg of the ask-user-question parity harness (TE28 G3/G12).
//!
//!   cargo build -p rpi-ext-ask-user-question --example parity_runner
//!   target/debug/examples/parity_runner <group> <fixture.json>
//!
//! Prints one `{"name", "output"}` JSON line per fixture case; `run-parity.mjs`
//! diffs the lines against the upstream tsx leg. Groups: `schema`,
//! `normalize`, `validate`, `envelope`, `row-intent`.

use std::fs;

use rpi_ext_ask_user_question::parity::{
    labels_by_kind_json, meta, normalize_question_params, question_params_schema,
    reserved_label_set, sentinels_to_append, validate_questionnaire, OptionData, QuestionData,
    QuestionParams, QuestionnaireResult, RowKind, ValidationResult, MAX_HEADER_LENGTH,
    MAX_LABEL_LENGTH, MAX_OPTIONS, MAX_QUESTIONS, MIN_OPTIONS, RESERVED_LABELS,
};
use serde_json::{json, Value};

fn parse_params(input: &Value) -> QuestionParams {
    serde_json::from_value(input.clone()).expect("fixture params")
}

fn schema_output() -> Value {
    json!({
        "schema": question_params_schema(),
        "constants": {
            "MAX_QUESTIONS": MAX_QUESTIONS,
            "MIN_OPTIONS": MIN_OPTIONS,
            "MAX_OPTIONS": MAX_OPTIONS,
            "MAX_HEADER_LENGTH": MAX_HEADER_LENGTH,
            "MAX_LABEL_LENGTH": MAX_LABEL_LENGTH,
        },
        "reservedLabels": RESERVED_LABELS,
        "sentinelLabels": {
            "other": rpi_ext_ask_user_question::tool::types::SENTINEL_OTHER_LABEL,
            "next": rpi_ext_ask_user_question::tool::types::SENTINEL_NEXT_LABEL,
        },
    })
}

fn row_intent_question(input: &Value) -> QuestionData {
    QuestionData {
        question: "Q?".to_owned(),
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
        multi_select: input.get("multiSelect").and_then(Value::as_bool),
    }
}

fn row_intent_output(name: &str, input: &Value) -> Value {
    if name == "constants" {
        return json!({
            "reservedLabels": reserved_label_set(),
            "labelsByKind": labels_by_kind_json(),
            "meta": {
                "option": meta(RowKind::Option).to_json(),
                "other": meta(RowKind::Other).to_json(),
                "next": meta(RowKind::Next).to_json(),
            },
            "sentinelKinds": ["other", "next"],
        });
    }
    let sentinels: Vec<&str> = sentinels_to_append(&row_intent_question(input))
        .into_iter()
        .map(|kind| kind.as_str())
        .collect();
    json!({ "sentinelsToAppend": sentinels })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: parity_runner <group> <fixture.json>");
        std::process::exit(2);
    }
    let group = args[1].as_str();
    let fixtures: Value =
        serde_json::from_str(&fs::read_to_string(&args[2]).expect("fixture file"))
            .expect("fixture json");
    let cases = fixtures
        .get(group)
        .and_then(|group| group.get("cases"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("group {group} has no cases"));

    for case in cases {
        let name = case.get("name").and_then(Value::as_str).expect("case name");
        let input = case.get("input").cloned().unwrap_or(Value::Null);
        let output = match group {
            "schema" => schema_output(),
            "normalize" => serde_json::to_value(normalize_question_params(&parse_params(&input)))
                .expect("normalize json"),
            "validate" => match validate_questionnaire(&parse_params(&input)) {
                ValidationResult::Ok => json!({"ok": true}),
                ValidationResult::Failed { error, message } => {
                    json!({"ok": false, "error": error, "message": message})
                }
            },
            "envelope" => {
                let params = parse_params(&input.get("params").cloned().unwrap_or(Value::Null));
                let result: Option<QuestionnaireResult> = input
                    .get("result")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()).expect("fixture result"));
                rpi_ext_ask_user_question::parity::build_questionnaire_response(
                    result.as_ref(),
                    &params,
                )
            }
            "row-intent" => row_intent_output(name, &input),
            other => panic!("unknown group: {other}"),
        };
        println!("{}", json!({"name": name, "output": output}));
    }
}
