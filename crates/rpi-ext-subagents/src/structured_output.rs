//! Child structured output (V19-03 S17/S28; upstream
//! `src/runs/shared/structured-output.ts` @ v0.88.1).
//!
//! The child registers a `structured_output` tool whose declared parameters
//! are deliberately permissive (`{value: {}}`): the host must not coerce
//! `"42"` into `42` before the outputSchema sees it. The tool handler — and
//! the parent, from the raw tool-call event — validates the raw JSON value
//! against the schema. Cyclic or too-deep schemas are rejected at admission
//! with the same unified validation error instead of a stack overflow.

use serde_json::{Value, json};

/// Maximum schema nesting accepted before admission fails (upstream
/// `MAX_SCHEMA_DEPTH`).
pub const MAX_SCHEMA_DEPTH: usize = 64;

pub const INVALID_SCHEMA_ERROR: &str = "invalid outputSchema: the schema is cyclic or too deep.";
pub const MISSING_OUTPUT_ERROR: &str = "Missing structured_output call; this step has outputSchema and must finish by calling structured_output.";

/// Reject cyclic `$ref` graphs and over-deep schemas before compiling.
pub fn validate_schema_shape(schema: &Value) -> Result<(), String> {
    if !schema.is_object() && !schema.is_boolean() {
        return Err(INVALID_SCHEMA_ERROR.to_string());
    }
    let mut stack: Vec<String> = Vec::new();
    walk_schema(schema, schema, 0, &mut stack)
}

fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn walk_schema(
    root: &Value,
    schema: &Value,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(INVALID_SCHEMA_ERROR.to_string());
    }
    match schema {
        Value::Bool(_) => Ok(()),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if stack.iter().any(|seen| seen == reference) {
                    return Err(INVALID_SCHEMA_ERROR.to_string());
                }
                stack.push(reference.to_string());
                if let Some(target) = resolve_local_ref(root, reference) {
                    walk_schema(root, target, depth + 1, stack)?;
                }
                stack.pop();
            }
            for value in object.values() {
                match value {
                    Value::Object(_) | Value::Array(_) => {
                        walk_schema(root, value, depth + 1, stack)?
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                walk_schema(root, item, depth + 1, stack)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Compile-time + instance validation with the shared jsonschema crate.
pub fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    validate_schema_shape(schema)?;
    let validator = jsonschema::options()
        .build(schema)
        .map_err(|error| format!("invalid outputSchema: {error}"))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|error| {
            let path = error.instance_path().to_string();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Structured output validation failed:\n  - {}",
            errors.join("\n  - ")
        ))
    }
}

/// Parse a call-level `outputSchema` value (`object`, JSON string, or false).
pub fn parse_call_schema(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Bool(false)) | None => None,
        Some(Value::Object(_)) => value.cloned(),
        Some(Value::String(raw)) if !raw.trim().is_empty() => serde_json::from_str(raw).ok(),
        _ => None,
    }
}

/// The child tool's parameter schema: permissive on purpose (raw JSON must
/// reach the handler before any coercion). The real schema rides the
/// description for the model and the parent's admission check.
pub fn tool_parameters(schema: &Value) -> Value {
    json!({
        "type": "object",
        "description": format!("Submit the final result. The value must satisfy this JSON Schema: {schema}"),
        "properties": {
            "value": {
                "description": "The raw JSON result; it is validated before any coercion.",
            }
        },
        "required": ["value"],
        "additionalProperties": false,
    })
}

/// Extract and validate the raw `value` from a `structured_output` call.
pub fn capture(schema: &Value, args: &Value) -> Result<Value, String> {
    let Some(value) = args.get("value") else {
        return Err("structured_output requires a `value` field.".to_string());
    };
    validate_value(schema, value)?;
    Ok(value.clone())
}

/// Whether a captured value is a validated structured output (used by S39's
/// length rule: a validated answer still succeeds).
/// S27 (#3191): parse typed JSON acceptance output first, then redact
/// secret-looking strings/keys/numbers recursively. Ordinary values such as
/// `KDE_FULL_SESSION=true` are left intact; invalid JSON fails safely.
#[allow(dead_code)]
pub fn parse_and_redact_typed_json(text: &str, secrets: &[String]) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("invalid typed JSON output: {error}"))?;
    Ok(redact_secrets(&value, secrets))
}

#[allow(dead_code)]
pub fn redact_secrets(value: &Value, secrets: &[String]) -> Value {
    let filtered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    let redact_text = |text: &str| -> String {
        let mut out = text.to_string();
        for secret in &filtered {
            if out.contains(*secret) {
                out = out.replace(*secret, "[redacted]");
            }
        }
        out
    };
    match value {
        Value::String(text) => Value::String(redact_text(text)),
        Value::Number(number) => {
            let text = number.to_string();
            if filtered.iter().any(|secret| text.contains(secret)) {
                Value::String("[redacted]".to_string())
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_secrets(item, secrets))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (redact_text(key), redact_secrets(value, secrets)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_string_is_not_coerced_to_a_number() {
        let schema =
            json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
        assert!(validate_value(&schema, &json!({"n": 42})).is_ok());
        assert!(validate_value(&schema, &json!({"n": "42"})).is_err());
        assert!(validate_value(&schema, &json!({"n": null})).is_err());
    }

    #[test]
    fn cyclic_ref_is_a_unified_validation_error() {
        let schema = json!({
            "$defs": {"node": {"$ref": "#/$defs/node"}},
            "$ref": "#/$defs/node"
        });
        let error = validate_schema_shape(&schema).expect_err("cycle");
        assert_eq!(error, INVALID_SCHEMA_ERROR);
    }

    #[test]
    fn too_deep_schema_is_rejected() {
        let mut schema = json!({"type": "object"});
        for _ in 0..(MAX_SCHEMA_DEPTH + 2) {
            schema = json!({"type": "object", "properties": {"next": schema}});
        }
        assert_eq!(
            validate_schema_shape(&schema).expect_err("depth"),
            INVALID_SCHEMA_ERROR
        );
    }

    #[test]
    fn typed_json_redaction_keeps_ordinary_values() {
        let value = parse_and_redact_typed_json(
            r#"{"token":"abc123","nested":{"KDE_FULL_SESSION":true,"password":"abc123"},"count":42}"#,
            &["abc123".to_string()],
        )
        .expect("valid json");
        assert_eq!(value["token"], json!("[redacted]"));
        assert_eq!(value["nested"]["KDE_FULL_SESSION"], json!(true));
        assert_eq!(value["nested"]["password"], json!("[redacted]"));
        assert_eq!(value["count"], json!(42));
        assert!(parse_and_redact_typed_json("{", &[]).is_err());
    }

    #[test]
    fn capture_returns_the_raw_value() {
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]});
        assert_eq!(
            capture(&schema, &json!({"value": {"ok": true}})).unwrap(),
            json!({"ok": true})
        );
        assert!(capture(&schema, &json!({"value": {"ok": "true"}})).is_err());
    }
}
