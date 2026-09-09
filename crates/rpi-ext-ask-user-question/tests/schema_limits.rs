//! TE28 §8 item 4 / R-Q2.4: the host validates tool arguments against the
//! registered `parameters` schema BEFORE `execute`
//! (`rpi-agent/src/agent_loop.rs:1291` → `rpi-ai` `validate_tool_arguments`),
//! so the schema-level limits (`header`/`label` `maxLength`, `questions`
//! `minItems`/`maxItems`, `options` `minItems`/`maxItems`, required fields)
//! are intercepted there and the runtime validator does not repeat them.
//!
//! This test drives the exact schema `tool::tool_definition` registers.

use rpi_ai::types::{Tool, ToolCall};
use rpi_ext_ask_user_question::parity::question_params_schema;
use serde_json::{json, Value};

fn tool() -> Tool {
    Tool {
        name: "ask_user_question".to_owned(),
        description: "ask".to_owned(),
        parameters: question_params_schema(),
        constrained_sampling: None,
    }
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "c1".to_owned(),
        name: "ask_user_question".to_owned(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        thought_signature: None,
        namespace: None,
    }
}

fn question(header: &str, label: &str) -> Value {
    json!({
        "question": "Q?",
        "header": header,
        "options": [
            {"label": label, "description": "a"},
            {"label": "B", "description": "b"}
        ]
    })
}

fn rejection(arguments: Value) -> String {
    rpi_ai::utils::validation::validate_tool_arguments(&tool(), &call(arguments))
        .expect_err("schema must reject")
}

#[test]
fn schema_limits_are_intercepted_before_execute() {
    // Valid baseline.
    rpi_ai::utils::validation::validate_tool_arguments(
        &tool(),
        &call(json!({"questions": [question("Pick", "A")]})),
    )
    .expect("baseline valid");

    // header maxLength 16 (17 chars rejected).
    let error = rejection(json!({"questions": [question("0123456789ABCDEFG", "A")]}));
    assert!(error.contains("Validation failed"), "{error}");
    assert!(error.contains("header"), "{error}");
    assert!(error.contains("16"), "{error}");

    // label maxLength 60 (61 chars rejected).
    let error = rejection(json!({"questions": [question("Pick", &"x".repeat(61))]}));
    assert!(error.contains("Validation failed"), "{error}");
    assert!(error.contains("label"), "{error}");

    // questions minItems 1 / maxItems 4.
    let error = rejection(json!({"questions": []}));
    assert!(error.contains("questions"), "{error}");
    let five: Vec<Value> = (0..5).map(|_| question("Pick", "A")).collect();
    let error = rejection(json!({"questions": five}));
    assert!(error.contains("questions"), "{error}");

    // options minItems 2 / maxItems 4.
    let error = rejection(json!({"questions": [{
        "question": "Q?",
        "header": "Pick",
        "options": [{"label": "A", "description": "a"}]
    }]}));
    assert!(error.contains("options"), "{error}");
    let five_options: Vec<Value> = (0..5)
        .map(|index| json!({"label": format!("O{index}"), "description": "d"}))
        .collect();
    let error = rejection(json!({"questions": [{
        "question": "Q?",
        "header": "Pick",
        "options": five_options
    }]}));
    assert!(error.contains("options"), "{error}");

    // Required fields.
    let error = rejection(json!({"questions": [{"question": "Q?", "header": "Pick"}]}));
    assert!(error.contains("options"), "{error}");
    let error = rejection(json!({}));
    assert!(error.contains("questions"), "{error}");
}
