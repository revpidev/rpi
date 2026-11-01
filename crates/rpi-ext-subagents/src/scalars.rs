//! Declared-scalar normalization for tool parameters (FR-J R3).
//!
//! Port of the intent behind pi-subagents v0.83.0 `#3060`
//! (`src/extension/subagent-options.ts:63-77`, `3a63e0ad`): providers such as
//! vLLM/LiteLLM-served models send nested scalars as strings
//! (`{"async":"false","timeoutMs":"120000"}`), and TypeBox's `Value.Convert`
//! lowers them to the declared type before validation.
//!
//! rpi has no `options` bag — every field is a declared top-level parameter or
//! an item field of `tasks`/`steps` — so the same conversion runs here before
//! the tool reads the params:
//! - top-level properties whose schema type is `boolean` / `integer` accept
//!   `"true"`/`"false"` and a trimmed integer string;
//! - `tasks[]` / `steps[]` items convert the declared item scalars
//!   (`worktree`, `progress`, `timeoutMs`, `maxRuntimeMs`).
//!
//! Only a string that fully parses to the declared type is converted; anything
//! else is left untouched so the normal validation still reports the real
//! problem, and ordinary string fields are never touched. The upstream
//! "given at the top level and in options with different values" conflict check
//! has no rpi counterpart (there is no second place) — registered [N/A-struct].

use serde_json::Value;

/// Item fields with a declared scalar type inside `tasks[]` / `steps[]`.
const ITEM_BOOLEAN_FIELDS: [&str; 2] = ["worktree", "progress"];
const ITEM_INTEGER_FIELDS: [&str; 2] = ["timeoutMs", "maxRuntimeMs"];

/// `"true"`/`"false"` → boolean; anything else stays a string.
fn convert_boolean(value: &Value) -> Option<Value> {
    match value.as_str()?.trim() {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => None,
    }
}

/// A trimmed integer string → number; anything else stays a string.
fn convert_integer(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let parsed: i64 = text.parse().ok()?;
    Some(Value::from(parsed))
}

fn convert_field(value: &Value, kind: &str) -> Value {
    match kind {
        "boolean" => convert_boolean(value).unwrap_or_else(|| value.clone()),
        "integer" => convert_integer(value).unwrap_or_else(|| value.clone()),
        _ => value.clone(),
    }
}

/// Convert one object's declared scalar fields (`schema_type` per key).
fn normalize_object(
    object: &serde_json::Map<String, Value>,
    declared: &dyn Fn(&str) -> Option<&'static str>,
) -> serde_json::Map<String, Value> {
    let mut out = object.clone();
    for (key, value) in object {
        let Some(kind) = declared(key) else {
            continue;
        };
        let converted = convert_field(value, kind);
        if converted != *value {
            out.insert(key.clone(), converted);
        }
    }
    out
}

/// Normalize the tool params against the schema's declared top-level types and
/// the fixed item types for `tasks`/`steps`.
pub fn normalize_declared_scalars(params: &Value, schema: &Value) -> Value {
    let Some(object) = params.as_object() else {
        return params.clone();
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    let declared = |key: &str| -> Option<&'static str> {
        let shape = properties?.get(key)?;
        let kind = shape.get("type").and_then(Value::as_str)?;
        match kind {
            "boolean" => Some("boolean"),
            "integer" => Some("integer"),
            _ => None,
        }
    };
    let mut out = normalize_object(object, &declared);

    for list_field in ["tasks", "steps"] {
        let Some(list) = object.get(list_field).and_then(Value::as_array) else {
            continue;
        };
        let items: Vec<Value> = list
            .iter()
            .map(|item| {
                let Some(item_object) = item.as_object() else {
                    return item.clone();
                };
                let item_declared = |key: &str| -> Option<&'static str> {
                    if ITEM_BOOLEAN_FIELDS.contains(&key) {
                        Some("boolean")
                    } else if ITEM_INTEGER_FIELDS.contains(&key) {
                        Some("integer")
                    } else {
                        None
                    }
                };
                Value::Object(normalize_object(item_object, &item_declared))
            })
            .collect();
        let converted = Value::Array(items);
        if converted != *object.get(list_field).unwrap_or(&Value::Null) {
            out.insert(list_field.to_string(), converted);
        }
    }

    Value::Object(out)
}

/// Test/CLI helper: the subagent tool's own schema, normalized once.
pub fn normalize_subagent_params(params: &Value) -> Value {
    normalize_declared_scalars(params, &crate::tool::tool_parameters_schema())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "async": {"type": "boolean"},
                "timeoutMs": {"type": "integer"},
                "model": {"type": "string"},
                "tasks": {"type": "array"},
                "steps": {"type": "array"}
            }
        })
    }

    #[test]
    fn converts_stringified_top_level_scalars_only() {
        let params = json!({
            "agent": "scout",
            "async": "false",
            "timeoutMs": " 120000 ",
            "model": "true",
            "unknown": "false"
        });
        let out = normalize_declared_scalars(&params, &schema());
        assert_eq!(out["async"], json!(false));
        assert_eq!(out["timeoutMs"], json!(120000));
        // String fields and unknown keys are untouched.
        assert_eq!(out["model"], json!("true"));
        assert_eq!(out["unknown"], json!("false"));
        // Values that do not fully parse stay as they were.
        let params = json!({"async": "yes", "timeoutMs": "12s"});
        let out = normalize_declared_scalars(&params, &schema());
        assert_eq!(out["async"], json!("yes"));
        assert_eq!(out["timeoutMs"], json!("12s"));
    }

    #[test]
    fn converts_declared_item_scalars_in_tasks_and_steps() {
        let params = json!({
            "tasks": [
                {"agent": "scout", "task": "a", "worktree": "true", "timeoutMs": "5000"},
                {"agent": "scout", "task": "b"}
            ],
            "steps": [
                {"agent": "worker", "task": "c", "progress": "false", "maxRuntimeMs": "9000"}
            ]
        });
        let out = normalize_declared_scalars(&params, &schema());
        assert_eq!(out["tasks"][0]["worktree"], json!(true));
        assert_eq!(out["tasks"][0]["timeoutMs"], json!(5000));
        assert_eq!(out["tasks"][1], json!({"agent": "scout", "task": "b"}));
        assert_eq!(out["steps"][0]["progress"], json!(false));
        assert_eq!(out["steps"][0]["maxRuntimeMs"], json!(9000));
    }

    #[test]
    fn normalizes_against_the_real_subagent_schema() {
        let out = normalize_subagent_params(&json!({
            "agent": "scout",
            "task": "x",
            "async": "false",
            "timeoutMs": "5000",
            "worktree": "true",
            "tasks": [{"agent": "scout", "task": "y", "worktree": "true"}]
        }));
        assert_eq!(out["async"], json!(false));
        assert_eq!(out["timeoutMs"], json!(5000));
        assert_eq!(out["worktree"], json!(true));
        assert_eq!(out["tasks"][0]["worktree"], json!(true));
    }

    #[test]
    fn leaves_non_objects_and_undeclared_lists_alone() {
        assert_eq!(
            normalize_declared_scalars(&json!("text"), &schema()),
            json!("text")
        );
        let params = json!({"other": [{"worktree": "true"}]});
        assert_eq!(
            normalize_declared_scalars(&params, &schema()),
            params,
            "only tasks/steps items are normalized"
        );
    }
}
