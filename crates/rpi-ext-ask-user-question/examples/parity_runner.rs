//! Rust leg of the ask-user-question parity harness (TE28 G3/G12).
//!
//!   cargo build -p rpi-ext-ask-user-question --example parity_runner
//!   target/debug/examples/parity_runner <group> <fixture.json>
//!
//! Prints one `{"name", "output"}` JSON line per fixture case; `run-parity.mjs`
//! diffs the lines against the upstream tsx leg. Groups: `schema`,
//! `normalize`, `validate`, `envelope`, `row-intent`, `rpc`.

use std::fs;

use rpi_ext_ask_user_question::parity::{
    has_dialog_ui, labels_by_kind_json, meta, normalize_question_params, question_params_schema,
    reserved_label_set, run_rpc_questionnaire, sentinels_to_append, validate_questionnaire,
    DialogOutcome, DialogUi, HostUi, OptionData, QuestionData, QuestionParams, QuestionnaireResult,
    RowKind, ValidationResult, MAX_HEADER_LENGTH, MAX_LABEL_LENGTH, MAX_OPTIONS, MAX_QUESTIONS,
    MIN_OPTIONS, RESERVED_LABELS,
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

/// TE29 `rpc` group — the dialog walker against a scripted UI (the upstream
/// leg's mock pops the same replies; the snapshot i18n bridge is the identity
/// fallback, so the Rust leg pins the `en` table).
struct ScriptEntry {
    reply: Option<String>,
    cancel: bool,
}

struct ScriptedDialogUi {
    script: std::collections::VecDeque<ScriptEntry>,
    calls: Vec<Value>,
}

impl ScriptedDialogUi {
    fn from_fixtures(script: &[Value]) -> Self {
        Self {
            script: script
                .iter()
                .map(|entry| ScriptEntry {
                    reply: entry
                        .get("reply")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    cancel: entry
                        .get("cancel")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect(),
            calls: Vec::new(),
        }
    }

    fn next(&mut self) -> DialogOutcome {
        // An exhausted script = dismissed (the upstream mock default).
        match self.script.pop_front() {
            Some(entry) if !entry.cancel => Ok(entry.reply),
            _ => Ok(None),
        }
    }
}

impl DialogUi for ScriptedDialogUi {
    fn select(&mut self, title: &str, options: &[String]) -> DialogOutcome {
        self.calls.push(json!({
            "method": "select",
            "title": title,
            "options": options,
        }));
        self.next()
    }

    fn input(&mut self, title: &str, placeholder: Option<&str>) -> DialogOutcome {
        self.calls.push(json!({
            "method": "input",
            "title": title,
            "placeholder": placeholder,
        }));
        self.next()
    }
}

/// Flagged probe surface (upstream `hasDialogUI` judgment table).
struct FlagHostUi {
    select: bool,
    input: bool,
}

impl HostUi for FlagHostUi {
    fn select_available(&self) -> bool {
        self.select
    }

    fn input_available(&self) -> bool {
        self.input
    }
}

fn rpc_output(input: &Value) -> Value {
    if let Some(probe) = input.get("probe") {
        let ui = probe.as_object().map(|probe| FlagHostUi {
            select: probe
                .get("select")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            input: probe.get("input").and_then(Value::as_bool).unwrap_or(false),
        });
        return json!({ "hasDialogUI": has_dialog_ui(ui.as_ref().map(|u| u as &dyn HostUi)) });
    }
    let params: QuestionParams =
        serde_json::from_value(input.get("params").cloned().unwrap_or(Value::Null))
            .expect("fixture params");
    let script = input
        .get("script")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ui = ScriptedDialogUi::from_fixtures(&script);
    let i18n = rpi_ext_ask_user_question::parity::I18n::for_locale("en");
    let result = run_rpc_questionnaire(&mut ui, &params, &i18n).expect("scripted ui never fails");
    json!({ "calls": ui.calls, "result": result })
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
            "rpc" => rpc_output(&input),
            other => panic!("unknown group: {other}"),
        };
        println!("{}", json!({"name": name, "output": output}));
    }
}
