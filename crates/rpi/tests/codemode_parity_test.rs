//! Byte-parity tests for the model-visible codemode description
//! (V16-07 FR-H R1 hard-parity surface). The expected strings are generated
//! from the pinned upstream (`external/pi` @ a13d35a74) — see
//! `tests/fixtures/README.md`.

use std::collections::{HashMap, HashSet};

use rpi::extensions::codemode::description::{
    CodemodeDescriptionOptions, create_codemode_description, describe_script_call,
};
use rpi::extensions::codemode::tool::ToolInfo;
use rpi_ext_host::types::ToolNamespace;
use serde_json::Value;

fn tool_info(value: &Value) -> ToolInfo {
    ToolInfo {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        parameters: value.get("parameters").cloned().unwrap_or(Value::Null),
        output_schema: value
            .get("outputSchema")
            .filter(|value| !value.is_null())
            .cloned(),
        exposure: rpi_ext_host::types::ToolExposure::Direct,
        namespace: None,
    }
}

#[test]
fn description_and_script_call_text_match_upstream_byte_for_byte() {
    // Both sides render `<packageDir>/docs/codemode.md`; the fixture was
    // generated with this directory.
    rpi_test_env::set_var("RPI_PACKAGE_DIR", "/tmp/rpi-codemode-docs");
    let fixture: Value = serde_json::from_str(include_str!("fixtures/codemode-description.json"))
        .expect("fixture parses");

    for case in fixture["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("case name");
        let tools: Vec<ToolInfo> = case["tools"]
            .as_array()
            .map(|tools| tools.iter().map(tool_info).collect())
            .unwrap_or_default();
        let options_value = &case["options"];
        let mut options = CodemodeDescriptionOptions {
            models: options_value
                .get("models")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..Default::default()
        };
        if let Some(budget) = options_value.get("inlineBudget").and_then(Value::as_u64) {
            options.inline_budget = Some(budget as usize);
        }
        if let Some(deferred) = options_value.get("deferred").and_then(Value::as_array) {
            options.deferred = deferred
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<HashSet<_>>();
        }
        if let Some(namespaces) = options_value.get("namespaces").and_then(Value::as_array) {
            let mut map: HashMap<String, ToolNamespace> = HashMap::new();
            for entry in namespaces {
                let tool = entry
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let namespace = entry.get("namespace").expect("namespace");
                map.insert(
                    tool.to_owned(),
                    ToolNamespace {
                        name: namespace
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        description: namespace
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        instructions: namespace
                            .get("instructions")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                );
            }
            options.namespaces = map;
        }
        let actual = create_codemode_description(&tools, &options);
        let expected = case["description"].as_str().expect("description");
        assert_eq!(actual, expected, "case {name}");
    }

    for case in fixture["scriptCalls"].as_array().expect("scriptCalls") {
        let tool = tool_info(case);
        let actual = describe_script_call(&tool);
        assert_eq!(
            actual,
            case["text"].as_str().expect("script call text"),
            "scriptCall {}",
            case["name"]
        );
    }

    for case in fixture["describeOutputs"]
        .as_array()
        .expect("describeOutputs")
    {
        let schema = case.get("outputSchema");
        let actual = rpi::extensions::codemode::description::describe_output(schema);
        assert_eq!(
            actual,
            case["text"].as_str().expect("describe output text"),
            "describeOutput {}",
            case["name"]
        );
    }
}
