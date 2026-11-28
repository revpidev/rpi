//! Tool-call argument validation and coercion against JSON Schema, ported
//! from `packages/ai/src/utils/validation.ts` and
//! `packages/schema/src/coerce.ts` @ b4afdddf.
//!
//! Optional-null cleanup precedes TypeBox-style structural conversion and
//! Ajv-style primitive coercion. Validators are compiled once per schema
//! content generation with local unresolved-reference handling.
//!
//! Validation wording comes from the `jsonschema` crate, not TypeBox; the
//! surrounding message format (`Validation failed for tool "…"` +
//! `- path: message` lines + `Received arguments:` pretty JSON) is ported
//! exactly. Registered as deviation D-006.

mod coercion;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::Value;

use crate::types::{Tool, ToolCall};

fn get_schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

fn is_integer_value(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.is_i64()
                || number.is_u64()
                || number.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
        }
        _ => false,
    }
}

fn matches_json_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => is_integer_value(value),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // JSON has no JS BigInt/undefined values. These TypeBox arms accept
        // and preserve the incoming JSON value instead of fabricating one.
        "bigint" | "undefined" => true,
        "void" => false,
        _ => false,
    }
}

fn collect_schema_identifiers(schema: &Value, identifiers: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            for key in ["$id", "$anchor"] {
                if let Some(identifier) = map.get(key).and_then(Value::as_str) {
                    identifiers.push(identifier.to_owned());
                }
            }
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "const" | "enum" | "default" | "example" | "examples"
                ) {
                    continue;
                }
                collect_schema_identifiers(child, identifiers);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_schema_identifiers(item, identifiers);
            }
        }
        _ => {}
    }
}

fn reference_resolves(reference: &str, root: &Value, identifiers: &[String]) -> bool {
    if reference.is_empty() || reference == "#" {
        return true;
    }
    if let Some(pointer) = reference.strip_prefix('#')
        && pointer.starts_with('/')
    {
        return root.pointer(pointer).is_some();
    }
    identifiers.iter().any(|identifier| {
        identifier == reference
            || identifier.ends_with(&format!("/{reference}"))
            || identifier.ends_with(&format!("#{reference}"))
    })
}

fn find_named_schema<'a>(schema: &'a Value, reference: &str) -> Option<&'a Value> {
    if reference.is_empty() || reference.starts_with('#') {
        return None;
    }
    match schema {
        Value::Object(map) => {
            for key in ["$id", "$anchor"] {
                if map
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|identifier| {
                        identifier == reference
                            || identifier.ends_with(&format!("/{reference}"))
                            || identifier.ends_with(&format!("#{reference}"))
                    })
                {
                    return Some(schema);
                }
            }
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "const" | "enum" | "default" | "example" | "examples"
                ) {
                    continue;
                }
                if let Some(found) = find_named_schema(child, reference) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_named_schema(item, reference)),
        _ => None,
    }
}

fn schema_contains_reference(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => {
            if map.contains_key("$ref") {
                return true;
            }
            map.iter().any(|(key, child)| {
                !matches!(
                    key.as_str(),
                    "const" | "enum" | "default" | "example" | "examples"
                ) && schema_contains_reference(child)
            })
        }
        Value::Array(items) => items.iter().any(schema_contains_reference),
        _ => false,
    }
}

fn normalize_schema_node(schema: &mut Value, root: &Value, identifiers: &[String]) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if !reference_resolves(reference, root, identifiers) {
            *schema = serde_json::json!({"not": {}});
            return;
        }
        if let Some(target) = find_named_schema(root, reference)
            && !schema_contains_reference(target)
        {
            *schema = target.clone();
        }
    }
    let Some(map) = schema.as_object_mut() else {
        return;
    };
    if let Some(kind) = map.get("type").and_then(Value::as_str) {
        match kind {
            "bigint" | "undefined" => {
                map.remove("type");
            }
            "void" => {
                *schema = serde_json::json!({"not": {}});
                return;
            }
            _ => {}
        }
    } else if let Some(Value::Array(kinds)) = map.get_mut("type") {
        if kinds
            .iter()
            .any(|kind| matches!(kind.as_str(), Some("bigint") | Some("undefined")))
        {
            map.remove("type");
        } else {
            kinds.retain(|kind| kind.as_str() != Some("void"));
            if kinds.is_empty() {
                *schema = serde_json::json!({"not": {}});
                return;
            }
        }
    }
    for (key, child) in map {
        if matches!(
            key.as_str(),
            "const" | "enum" | "default" | "example" | "examples"
        ) {
            continue;
        }
        normalize_schema_node(child, root, identifiers);
    }
}

fn normalized_schema(schema: &Value) -> Value {
    let mut normalized = schema.clone();
    let mut identifiers = Vec::new();
    collect_schema_identifiers(schema, &mut identifiers);
    normalize_schema_node(&mut normalized, schema, &identifiers);
    normalized
}

fn contains_prefix_items(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => {
            if map.contains_key("prefixItems") {
                return true;
            }
            map.iter().any(|(key, child)| {
                !matches!(
                    key.as_str(),
                    "const" | "enum" | "default" | "example" | "examples"
                ) && contains_prefix_items(child)
            })
        }
        Value::Array(items) => items.iter().any(contains_prefix_items),
        _ => false,
    }
}

fn validation_options(schema: &Value) -> jsonschema::ValidationOptions<'static> {
    let options = jsonschema::options();
    if schema.get("$schema").is_some() {
        options
    } else {
        options.with_draft(if contains_prefix_items(schema) {
            jsonschema::Draft::Draft202012
        } else {
            jsonschema::Draft::Draft7
        })
    }
}

/// Compile the shared validator after applying pi-schema's extended type and
/// unresolved-reference rules. Draft 7 preserves tuple `items` arrays;
/// `prefixItems` selects 2020-12 unless the schema declares its own dialect.
pub fn compile_schema_validator(schema: &Value) -> Result<jsonschema::Validator, String> {
    validation_options(schema)
        .build(&normalized_schema(schema))
        .map_err(|error| error.to_string())
}

struct CompiledSchema {
    schema: Value,
    validators: jsonschema::ValidatorMap,
}

impl CompiledSchema {
    fn new(schema: &Value) -> Result<Self, String> {
        let validators = validation_options(schema)
            .build_map(&normalized_schema(schema))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            schema: schema.clone(),
            validators,
        })
    }

    fn root(&self) -> &jsonschema::Validator {
        &self.validators["#"]
    }
}

const SCHEMA_CACHE_CAPACITY: usize = 64;

#[derive(Default)]
struct CompiledSchemaCache {
    entries: HashMap<String, Arc<CompiledSchema>>,
    insertion_order: VecDeque<String>,
}

static COMPILED_SCHEMAS: LazyLock<Mutex<CompiledSchemaCache>> =
    LazyLock::new(|| Mutex::new(CompiledSchemaCache::default()));

fn get_compiled_schema(schema: &Value) -> Result<Arc<CompiledSchema>, String> {
    let key = serde_json::to_string(schema).unwrap_or_default();
    if let Some(compiled) = lock_cache().entries.get(&key).map(Arc::clone) {
        return Ok(compiled);
    }
    let compiled = Arc::new(CompiledSchema::new(schema)?);
    let mut cache = lock_cache();
    if let Some(existing) = cache.entries.get(&key) {
        return Ok(Arc::clone(existing));
    }
    if cache.entries.len() >= SCHEMA_CACHE_CAPACITY
        && let Some(oldest) = cache.insertion_order.pop_front()
    {
        cache.entries.remove(&oldest);
    }
    cache.insertion_order.push_back(key.clone());
    cache.entries.insert(key, Arc::clone(&compiled));
    Ok(compiled)
}

fn lock_cache() -> std::sync::MutexGuard<'static, CompiledSchemaCache> {
    COMPILED_SCHEMAS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn format_json_pointer(pointer: &str) -> String {
    if pointer.is_empty() {
        return "root".to_owned();
    }
    pointer
        .strip_prefix('/')
        .unwrap_or(pointer)
        .split('/')
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

fn format_validation_path(error: &jsonschema::ValidationError) -> String {
    let base_path = format_json_pointer(&error.instance_path().to_string());
    if let jsonschema::error::ValidationErrorKind::Required { property } = &error.kind() {
        let property = property
            .as_str()
            .map(format_json_pointer)
            .unwrap_or_default();
        if !property.is_empty() {
            return if base_path == "root" {
                property
            } else {
                format!("{base_path}.{property}")
            };
        }
    }
    base_path
}

/// `validateToolCall`: finds a tool by name and validates the call arguments.
pub fn validate_tool_call(tools: &[Tool], tool_call: &ToolCall) -> Result<Value, String> {
    let Some(tool) = tools.iter().find(|t| t.name == tool_call.name) else {
        return Err(format!("Tool \"{}\" not found", tool_call.name));
    };
    validate_tool_arguments(tool, tool_call)
}

/// Validates and coerces tool-call arguments against the tool's JSON schema.
pub fn validate_tool_arguments(tool: &Tool, tool_call: &ToolCall) -> Result<Value, String> {
    if let Some(error) = &tool_call.arguments_error {
        return Err(format!(
            "Invalid arguments for tool \"{}\": {error}",
            tool_call.name
        ));
    }
    let compiled = get_compiled_schema(&tool.parameters)?;
    let args = coercion::coerce(&compiled, Value::Object(tool_call.arguments.clone()));
    if compiled.root().is_valid(&args) {
        return Ok(args);
    }

    let errors = compiled
        .root()
        .iter_errors(&args)
        .map(|error| format!("  - {}: {}", format_validation_path(&error), error))
        .collect::<Vec<_>>()
        .join("\n");
    let errors = if errors.is_empty() {
        "Unknown validation error".to_owned()
    } else {
        errors
    };

    let received = serde_json::to_string_pretty(&tool_call.arguments)
        .unwrap_or_else(|_| "[unserializable]".to_owned());
    Err(format!(
        "Validation failed for tool \"{}\":\n{errors}\n\nReceived arguments:\n{received}",
        tool_call.name
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(parameters: Value) -> Tool {
        Tool {
            name: "bash".to_owned(),
            description: "run".to_owned(),
            parameters,
            constrained_sampling: None,
        }
    }

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".to_owned(),
            name: "bash".to_owned(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }
    }

    fn coerced(schema: Value, input: Value) -> Value {
        let compiled = get_compiled_schema(&schema).expect("schema compiles");
        coercion::coerce(&compiled, input)
    }

    fn assert_coerces(schema: Value, input: Value, expected: Value) {
        let compiled = get_compiled_schema(&schema).expect("schema compiles");
        let output = coercion::coerce(&compiled, input);
        assert_eq!(output, expected);
        assert!(
            compiled.root().is_valid(&output),
            "coerced output must validate: {}",
            compiled
                .root()
                .iter_errors(&output)
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    #[test]
    fn malformed_final_arguments_fail_before_schema_validation() {
        let error = validate_tool_arguments(
            &tool(json!({"type": "object"})),
            &ToolCall {
                arguments_error: Some(
                    "Unexpected token\n\nReceived arguments:\n{\"x\":".to_owned(),
                ),
                ..call(json!({}))
            },
        )
        .expect_err("malformed arguments must not execute");
        assert_eq!(
            error,
            "Invalid arguments for tool \"bash\": Unexpected token\n\nReceived arguments:\n{\"x\":"
        );
    }

    /// "treats null as omission for optional non-nullable properties"
    /// (validation.test.ts:101-124, `7915cdac6`): optional non-nullable
    /// nulls drop (recursively); nullable-union nulls survive.
    #[test]
    fn test_normalize_optional_nulls_drops_non_nullable_optionals() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "number"},
                "nullable": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "metadata": {
                    "type": "object",
                    "properties": {"enabled": {"type": "boolean"}},
                    "required": []
                }
            },
            "required": ["path"]
        }));
        let out = validate_tool_arguments(
            &tool,
            &call(json!({
                "path": "file.txt",
                "offset": null,
                "nullable": null,
                "metadata": {"enabled": null}
            })),
        )
        .expect("valid");
        assert_eq!(
            out,
            json!({"path": "file.txt", "nullable": null, "metadata": {}})
        );
    }

    /// "preserves optional nulls whose referenced schema is nullable"
    /// (validation.test.ts:126-143): `$ref` properties are exempt from the
    /// null cleanup.
    #[test]
    fn test_normalize_optional_nulls_exempts_refs() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"value": {"$ref": "#/$defs/value"}},
            "$defs": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"value": null}))).expect("valid");
        assert_eq!(out, json!({"value": null}));
    }

    /// Array items recurse through both tuple and single `items` schemas.
    #[test]
    fn test_normalize_optional_nulls_recurses_arrays() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"label": {"type": "string"}},
                        "required": []
                    }
                }
            },
            "required": []
        }));
        let out = validate_tool_arguments(
            &tool,
            &call(json!({"tags": [{"label": null}, {"label": "keep"}]})),
        )
        .expect("valid");
        assert_eq!(out, json!({"tags": [{}, {"label": "keep"}]}));
    }

    #[test]
    fn relative_root_id_applies_once_for_local_references() {
        let schema = json!({
            "$id": "schemas/tool.json",
            "type": "object",
            "properties": { "count": { "$ref": "#/$defs/count" } },
            "required": ["count"],
            "$defs": { "count": { "type": "integer" } }
        });
        let validator = compile_schema_validator(&schema).expect("validator");
        assert!(validator.is_valid(&json!({"count": 2})));
        let invalid = json!({"count": "a"});
        let error = validator
            .iter_errors(&invalid)
            .next()
            .expect("string is not an integer");
        assert_eq!(error.instance_path().to_string(), "/count");
    }

    #[test]
    fn test_validate_tool_call_not_found() {
        let err = validate_tool_call(&[], &call(json!({}))).unwrap_err();
        assert_eq!(err, "Tool \"bash\" not found");
    }

    #[test]
    fn test_coercion_null_to_number() {
        // `required` mirrors the upstream coercion harness
        // (createToolCallWithPlainSchema): required properties skip the
        // optional-null cleanup and reach coercion (7915cdac6 changed
        // optional nulls to deletion — see the normalize tests).
        let tool = tool(json!({
            "type": "object",
            "properties": {"n": {"type": "number"}, "i": {"type": "integer"}},
            "required": ["n", "i"]
        }));
        let out =
            validate_tool_arguments(&tool, &call(json!({"n": null, "i": null}))).expect("valid");
        assert_eq!(out, json!({"n": 0, "i": 0}));
    }

    #[test]
    fn test_coercion_string_to_number_and_integer() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"n": {"type": "number"}, "i": {"type": "integer"}}
        }));
        let out =
            validate_tool_arguments(&tool, &call(json!({"n": "123", "i": "42"}))).expect("valid");
        assert_eq!(out, json!({"n": 123, "i": 42}));
        let out = validate_tool_arguments(&tool, &call(json!({"n": 1, "i": "4.5"})))
            .expect("integer conversion truncates toward zero");
        assert_eq!(out, json!({"n": 1, "i": 4}));
        let out = validate_tool_arguments(&tool, &call(json!({"n": 1, "i": -4.5})))
            .expect("numeric input follows the same integer conversion");
        assert_eq!(out, json!({"n": 1, "i": -4}));
        let out = validate_tool_arguments(&tool, &call(json!({"n": "", "i": 1})))
            .expect("pass 2 follows JS Number for an empty string");
        assert_eq!(out, json!({"n": 0, "i": 1}));
    }

    #[test]
    fn coercion_preserves_a_valid_union_member_without_array_wrapping() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "payload": {"type": ["array", "string"], "items": {"type": "integer"}}
            },
            "required": ["payload"]
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"payload": "x"})))
            .expect("the string union arm already accepts this value");
        assert_eq!(out, json!({"payload": "x"}));
        let out = validate_tool_arguments(&tool, &call(json!({"payload": ["1", 2]})))
            .expect("the array union arm accepts converted items");
        assert_eq!(out, json!({"payload": [1, 2]}));
    }

    #[test]
    fn test_coercion_bool_to_number_and_back() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"n": {"type": "number"}, "b": {"type": "boolean"}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"n": true, "b": 1}))).expect("valid");
        assert_eq!(out, json!({"n": 1, "b": true}));
        let out = validate_tool_arguments(&tool, &call(json!({"n": false, "b": "false"})))
            .expect("valid");
        assert_eq!(out, json!({"n": 0, "b": false}));
    }

    #[test]
    fn test_coercion_to_string_and_null() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "s": {"type": "string"},
                "x": {"type": "null"}
            },
            "required": ["s", "x"]
        }));
        let out =
            validate_tool_arguments(&tool, &call(json!({"s": 12, "x": false}))).expect("valid");
        assert_eq!(out, json!({"s": "12", "x": null}));
        let out = validate_tool_arguments(&tool, &call(json!({"s": null, "x": 0}))).expect("valid");
        assert_eq!(out, json!({"s": "null", "x": null}));
    }

    #[test]
    fn test_coercion_nested_objects_and_arrays() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "opts": {
                    "type": "object",
                    "properties": {"timeout": {"type": "integer"}},
                    "additionalProperties": {"type": "string"}
                },
                "list": {"type": "array", "items": {"type": "integer"}},
                "tuple": {"type": "array", "items": [{"type": "integer"}, {"type": "boolean"}]}
            }
        }));
        let out = validate_tool_arguments(
            &tool,
            &call(json!({
                "opts": {"timeout": "30", "extra": 5},
                "list": ["1", 2, "3"],
                "tuple": ["7", "true"]
            })),
        )
        .expect("valid");
        assert_eq!(
            out,
            json!({
                "opts": {"timeout": 30, "extra": "5"},
                "list": [1, 2, 3],
                "tuple": [7, true]
            })
        );
    }

    #[test]
    fn test_coercion_any_of_first_valid_branch() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "v": {"anyOf": [{"type": "integer"}, {"type": "string"}]}
            }
        }));
        // 2e95584da: "42" already matches the `string` arm, so it is preserved
        // as-is (pre-fix this was coerced to the integer 42).
        let out = validate_tool_arguments(&tool, &call(json!({"v": "42"}))).expect("valid");
        assert_eq!(out, json!({"v": "42"}));
        let out = validate_tool_arguments(&tool, &call(json!({"v": "abc"}))).expect("valid");
        assert_eq!(out, json!({"v": "abc"}));
    }

    #[test]
    fn test_coercion_type_array_first_convertible() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"v": {"type": ["integer", "string"]}},
            "required": ["v"]
        }));
        // Already matches a union member: left unchanged.
        let out = validate_tool_arguments(&tool, &call(json!({"v": "keep"}))).expect("valid");
        assert_eq!(out, json!({"v": "keep"}));
        // Matches none: first convertible type wins.
        let out = validate_tool_arguments(&tool, &call(json!({"v": null}))).expect("valid");
        assert_eq!(out, json!({"v": 0}));
    }

    // -- union arm preservation (2e95584da / f9476a61e @ 4181f66) -------------

    /// validation.test.ts: "preserves a value that already matches a nullable
    /// union arm".
    #[test]
    fn preserves_a_value_that_already_matches_a_nullable_union_arm() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"value": null}))).expect("valid");
        assert_eq!(out, json!({"value": null}));
    }

    /// validation.test.ts: "preserves a value that already matches a oneOf
    /// nullable union arm".
    #[test]
    fn preserves_a_value_that_already_matches_a_one_of_nullable_union_arm() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"value": {"oneOf": [{"type": "number"}, {"type": "null"}]}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"value": null}))).expect("valid");
        assert_eq!(out, json!({"value": null}));
    }

    /// validation.test.ts: "still coerces nullable unions when the original
    /// value does not match any arm".
    #[test]
    fn still_coerces_nullable_unions_when_the_original_value_does_not_match_any_arm() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"value": "42"}))).expect("valid");
        assert_eq!(out, json!({"value": 42}));
    }

    /// validation.test.ts: "accepts null for nullable array schemas with
    /// items" (f9476a61e; the TypeBox `Compile` assertion is TypeBox-specific —
    /// rpi's jsonschema draft-7 validator is the only engine, so the intent
    /// port asserts `validateToolArguments` accepts `null` unchanged).
    #[test]
    fn accepts_null_for_nullable_array_schemas_with_items() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"value": {"type": ["array", "null"], "items": {"type": "string"}}}
        }));
        let out = validate_tool_arguments(&tool, &call(json!({"value": null}))).expect("valid");
        assert_eq!(out, json!({"value": null}));
    }

    #[test]
    fn test_extended_bigint_undefined_and_void_rules() {
        let tool = tool(json!({
            "type": "object",
            "properties": {
                "big": {"type": "bigint"},
                "maybe": {"type": "undefined"},
                "never": {"type": "void"},
                "numbers": {"type": "array", "items": {"type": "integer"}}
            }
        }));
        let input = json!({"big": "12n", "maybe": null, "numbers": "7"});
        let out = validate_tool_arguments(&tool, &call(input.clone())).expect("valid");
        assert_eq!(out["big"], "12n");
        assert_eq!(out["maybe"], Value::Null);
        assert_eq!(out["numbers"], json!([7]));
        assert!(out.get("never").is_none());
        let error = validate_tool_arguments(
            &tool,
            &call(json!({"big": 12, "maybe": null, "never": false, "numbers": [1]})),
        )
        .expect_err("Void always rejects");
        assert!(error.contains("never"), "{error}");
        let validator = compile_schema_validator(&json!({"type": "void"})).expect("Void compiles");
        assert!(!validator.is_valid(&Value::Null));
    }

    #[test]
    fn test_unresolved_refs_fail_only_at_their_instance_location() {
        let tool = tool(json!({
            "$id": "https://example.com/root.json",
            "type": "object",
            "properties": {
                "good": {"$ref": "#/$defs/string"},
                "recursive": {"$ref": "#"},
                "missing": {"$ref": "#/$defs/missing"},
                "external": {"$ref": "https://example.com/missing.json"},
                "named": {"$ref": "A"}
            },
            "$defs": {
                "string": {"type": "string"},
                "A": {"$id": "A", "type": "integer"}
            }
        }));
        let valid = json!({"good": "ok", "recursive": {"good": "nested"}, "named": 3});
        validate_tool_arguments(&tool, &call(valid)).expect("valid and cyclic refs survive");
        let missing =
            json!({"good": "ok", "recursive": {"good": "nested"}, "missing": 1, "named": 3});
        let error = validate_tool_arguments(&tool, &call(missing)).expect_err("missing ref");
        assert!(error.contains("missing"), "{error}");
        let external =
            json!({"good": "ok", "recursive": {"good": "nested"}, "external": 1, "named": 3});
        let error = validate_tool_arguments(&tool, &call(external)).expect_err("external ref");
        assert!(error.contains("external"), "{error}");
    }

    #[test]
    fn test_validation_failure_message_format() {
        let tool = tool(json!({
            "type": "object",
            "properties": {"cmd": {"type": "string"}},
            "required": ["cmd"]
        }));
        let err = validate_tool_arguments(&tool, &call(json!({}))).unwrap_err();
        assert!(
            err.starts_with("Validation failed for tool \"bash\":\n"),
            "{err}"
        );
        assert!(err.contains("  - cmd: "), "{err}");
        assert!(err.contains("\n\nReceived arguments:\n{}"), "{err}");
    }

    #[test]
    fn upstream_primitives_const_enum_and_arrays() {
        for (schema, input, expected) in [
            (json!({"type": "number"}), json!(""), json!(0)),
            (json!({"type": "number"}), json!("true"), json!(1)),
            (json!({"type": "integer"}), json!("3.5"), json!(3)),
            (json!({"type": "boolean"}), json!("1"), json!(true)),
            (json!({"type": "boolean"}), json!("FALSE"), json!(false)),
            (json!({"type": "null"}), json!("null"), Value::Null),
            (json!({"type": "string"}), Value::Null, json!("null")),
            (json!({"const": 1}), json!("1"), json!(1)),
            (json!({"enum": [1, 2, "hello"]}), json!("2"), json!(2)),
            (
                json!({"type": "array", "items": {"type": "string"}}),
                Value::Null,
                json!(["null"]),
            ),
        ] {
            assert_coerces(schema, input, expected);
        }
        assert_eq!(coerced(json!({"const": 1}), json!("2")), json!("2"));
        assert_eq!(
            coerced(json!({"enum": [1, 2, "hello"]}), json!("world")),
            json!("world")
        );
        assert_eq!(
            coerced(json!({"type": ["integer", "null"]}), json!("1.5")),
            json!("1.5")
        );
        for input in [json!("true"), json!("12n")] {
            assert_eq!(
                coerced(json!({"type": ["number", "null"]}), input.clone()),
                input,
                "pass 3 must not apply TypeBox-only number conversions"
            );
        }
        assert_coerces(json!({"type": "number"}), json!("12n"), json!(12));
    }

    #[test]
    fn bigint_string_number_fallback_matches_upstream_grammar() {
        let schema = json!({"type": "number"});
        assert_coerces(schema.clone(), json!("-12n"), json!(-12));
        assert_coerces(schema.clone(), json!("01"), json!(1));
        assert_coerces(schema.clone(), json!("1e3"), json!(1000));
        assert_coerces(schema.clone(), json!("0x10"), json!(16));
        assert_coerces(schema.clone(), json!("12.9"), json!(12.9));
        for input in ["01n", "1e3n", "12.9n", "12N", "9007199254740992n"] {
            assert_eq!(coerced(schema.clone(), json!(input)), json!(input));
        }
    }

    #[test]
    fn follows_refs_and_converts_pattern_and_implicit_containers() {
        assert_coerces(
            json!({
                "type": "object",
                "properties": {"item": {"$ref": "#/$defs/item"}},
                "$defs": {"item": {"type": "object", "properties": {"id": {"type": "integer"}}}}
            }),
            json!({"item": {"id": "7"}}),
            json!({"item": {"id": 7}}),
        );
        assert_coerces(
            json!({
                "type": "object",
                "properties": {"a": {"type": "number"}},
                "patternProperties": {"^n_": {"type": "number"}},
                "additionalProperties": {"type": "boolean"}
            }),
            json!({"a": "1", "n_x": "2", "other": "true"}),
            json!({"a": 1, "n_x": 2, "other": true}),
        );
        assert_coerces(
            json!({"properties": {"n": {"type": "integer"}}}),
            json!({"n": "5"}),
            json!({"n": 5}),
        );
        assert_coerces(
            json!({"items": {"type": "integer"}}),
            json!(["5"]),
            json!([5]),
        );
        assert_coerces(
            json!({"properties": {"a~1b": {"type": "integer"}}}),
            json!({"a~1b": "3"}),
            json!({"a~1b": 3}),
        );
    }

    #[test]
    fn converts_prefix_items_without_wrapping_tuples() {
        let tuple = json!({
            "type": "array",
            "prefixItems": [{"type": "number"}, {"type": "boolean"}],
            "items": false
        });
        assert_coerces(tuple.clone(), json!(["1", "true"]), json!([1, true]));
        assert_eq!(coerced(tuple, json!("1")), json!("1"));
    }

    #[test]
    fn upstream_all_of_one_of_and_union_member_fixtures() {
        assert_coerces(
            json!({"allOf": [
                {"type": "object", "properties": {"a": {"type": "number"}}},
                {"type": "object", "properties": {"b": {"type": "boolean"}}}
            ]}),
            json!({"a": "1", "b": "true"}),
            json!({"a": 1, "b": true}),
        );
        assert_coerces(
            json!({"oneOf": [{"type": "boolean"}, {"type": "number"}]}),
            json!("1"),
            json!(true),
        );
        for keyword in ["anyOf", "oneOf"] {
            assert_coerces(
                json!({
                    "type": "object",
                    keyword: [
                        {"type": "object", "properties": {"a": {"type": "array", "items": {"type": "integer"}}}, "required": ["a"]},
                        {"type": "object", "properties": {"b": {"type": "boolean"}}, "required": ["b"]}
                    ],
                    "properties": {"c": {"type": "integer"}},
                    "required": ["c"]
                }),
                json!({"a": "3", "c": "2"}),
                json!({"a": [3], "c": 2}),
            );
        }
    }

    #[test]
    fn recursive_json_union_converts_nested_nulls_and_arrays() {
        let schema = json!({
            "type": "object",
            "properties": {"data": {"$ref": "#/$defs/Json"}},
            "required": ["data"],
            "$defs": {
                "Json": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "number"},
                        {"type": "boolean"},
                        {"type": "array", "items": {"$ref": "#/$defs/Json"}},
                        {"type": "object", "additionalProperties": {"$ref": "#/$defs/Json"}}
                    ]
                }
            }
        });
        assert_coerces(
            schema,
            json!({"data": {"a": [1, null]}}),
            json!({"data": {"a": [1, "null"]}}),
        );
    }

    #[test]
    fn recursive_unions_convert_once_and_reject_without_expanding_forever() {
        let tree = json!({
            "type": "object",
            "properties": {"t": {"$ref": "#/$defs/Tree"}},
            "$defs": {"Tree": {"anyOf": [
                {"type": "string"},
                {"type": "array", "items": {"$ref": "#/$defs/Tree"}}
            ]}}
        });
        assert_coerces(tree.clone(), json!({"t": [5]}), json!({"t": ["5"]}));
        assert_coerces(tree, json!({"t": ["a", ["b"]]}), json!({"t": ["a", ["b"]]}));

        let schema = json!({
            "type": "object",
            "properties": {"value": {"anyOf": [
                {"type": "number"},
                {"type": "array", "items": {"$ref": "#/properties/value"}}
            ]}},
            "required": ["value"]
        });
        assert_coerces(
            schema,
            json!({"value": ["1", ["2"]]}),
            json!({"value": [1, [2]]}),
        );

        let deep_schema = json!({
            "type": "object",
            "properties": {"s": {"$ref": "#/$defs/S"}},
            "$defs": {"S": {"anyOf": [
                {"type": "object", "additionalProperties": {"$ref": "#/$defs/S"}, "required": ["a"]},
                {"type": "object", "additionalProperties": {"$ref": "#/$defs/S"}, "required": ["b"]}
            ]}}
        });
        let mut nested = json!({"x": 1});
        for _ in 0..40 {
            nested = json!({"c": nested});
        }
        let input = json!({"s": nested});
        let compiled = get_compiled_schema(&deep_schema).expect("recursive schema compiles");
        let output = coercion::coerce(&compiled, input.clone());
        assert_eq!(output, input);
        assert!(!compiled.root().is_valid(&output));
    }

    #[test]
    fn union_members_and_optional_nulls_follow_all_property_keywords() {
        assert_coerces(
            json!({
                "type": "object",
                "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                "anyOf": [{"required": ["a"]}, {"required": ["b"]}]
            }),
            json!({"a": "3.5"}),
            json!({"a": 3}),
        );
        let item =
            json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}});
        for schema in [
            json!({"type": "object", "additionalProperties": item.clone()}),
            json!({"type": "object", "patternProperties": {"^entry$": item.clone()}, "additionalProperties": false}),
        ] {
            assert_coerces(schema, json!({"entry": {"n": null}}), json!({"entry": {}}));
        }
    }

    #[test]
    fn validation_paths_unescape_json_pointer_segments() {
        let schema = json!({
            "type": "object",
            "properties": {"a/b": {"type": "integer"}, "c~d": {"type": "integer"}}
        });
        let error = validate_tool_arguments(
            &tool(schema.clone()),
            &call(json!({"a/b": "bad", "c~d": 1})),
        )
        .expect_err("invalid property");
        assert!(error.contains("- a/b:"), "{error}");
        let error = validate_tool_arguments(&tool(schema), &call(json!({"a/b": 1, "c~d": "bad"})))
            .expect_err("invalid property");
        assert!(error.contains("- c~d:"), "{error}");
    }

    #[test]
    fn compiled_schemas_are_cached_by_content() {
        let schema = json!({"$id": "schema-cache-regression", "type": "object", "properties": {"n": {"type": "integer"}}});
        let first = get_compiled_schema(&schema).expect("first compile");
        let second = get_compiled_schema(&schema).expect("cache hit");
        assert!(Arc::ptr_eq(&first, &second));
        let changed = json!({"$id": "schema-cache-regression", "type": "object", "properties": {"n": {"type": "string"}}});
        let third = get_compiled_schema(&changed).expect("changed schema compile");
        assert!(!Arc::ptr_eq(&first, &third));
    }
}
