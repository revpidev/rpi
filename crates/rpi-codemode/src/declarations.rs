//! TypeScript declaration rendering for the script-visible API (port of
//! `packages/codemode/src/declarations.ts` @ 4a080252f).
//!
//! Tools become members of `declare const tools`, globals become
//! `declare function` statements, and `ns.member` globals members of
//! `declare const ns`. Descriptions become doc comments; schemas become
//! types.

use serde_json::{Map, Value};

use crate::identifier::to_codemode_identifier;
use crate::types::CodemodeToolInfo;

const INDENT: &str = "  ";
/// Largest rendered input type, in characters, before it becomes `unknown`
/// (declarations.ts:10).
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema (declarations.ts:12).
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords with a number or string value shown as
/// `@keyword value` property tags (declarations.ts:14-34).
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
/// Tag keywords whose values all hold at once, so only the largest (lower) or
/// smallest (upper) matters (declarations.ts:28-30).
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest property tag; longer ones (large defaults, long patterns) are left
/// out (declarations.ts:31-32).
const MAX_TAG_CHARS: usize = 80;
/// Characters that end a `//` comment (declarations.ts:33-34).
const LINE_TERMINATORS: [char; 4] = ['\r', '\n', '\u{2028}', '\u{2029}'];

/// `RenderDeclarationsOptions` (declarations.ts:35-44) collapsed to the two
/// slices.
#[derive(Default)]
pub struct RenderDeclarationsOptions<'a> {
    pub tools: &'a [CodemodeToolInfo],
    pub globals: &'a [CodemodeToolInfo],
}

/// Render TypeScript declarations for the script-visible API.
pub fn render_declarations(options: RenderDeclarationsOptions<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !options.tools.is_empty() {
        let members = options
            .tools
            .iter()
            .map(|tool| {
                format!(
                    "{}{INDENT}{}",
                    doc_comment(tool.description.as_deref(), INDENT),
                    render_tool_signature(tool, None)
                )
            })
            .collect::<Vec<_>>();
        sections.push(format!(
            "declare const tools: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    // Insertion-ordered namespaces (`Map` upstream).
    let mut namespace_names: Vec<String> = Vec::new();
    let mut namespaces: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for global in options.globals {
        match global.name.find('.') {
            None => {
                sections.push(render_global(
                    &format!("declare function {}", global.name),
                    global,
                    "",
                ));
            }
            Some(dot) => {
                let namespace = global.name[..dot].to_owned();
                let member = render_global(&global.name[dot + 1..], global, INDENT);
                let members = namespaces.entry(namespace.clone()).or_insert_with(|| {
                    namespace_names.push(namespace.clone());
                    Vec::new()
                });
                members.push(member);
            }
        }
    }
    for namespace in namespace_names {
        let members = namespaces.remove(&namespace).unwrap_or_default();
        sections.push(format!(
            "declare const {namespace}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// One tool as a member of the `tools` object:
/// `name(args: T): Promise<R>;` with the name as the identifier scripts use.
/// Input types longer than `input_max_chars` render as `unknown`.
pub fn render_tool_signature(tool: &CodemodeToolInfo, input_max_chars: Option<usize>) -> String {
    let input = match &tool.input_schema {
        None => "unknown".to_owned(),
        Some(schema) => schema_to_type(
            schema,
            Some(input_max_chars.unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS)),
        ),
    };
    format!(
        "{}(args: {}): Promise<{}>;",
        to_codemode_identifier(&tool.name),
        input,
        render_tool_output_type(tool.output_schema.as_ref())
    )
}

/// A tool's sample: the description followed by the tool's declaration
/// (declarations.ts:91-99).
pub fn render_tool_sample(tool: &CodemodeToolInfo) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_tool_signature(tool, None)
    );
    format!(
        "{}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```",
        tool.description.as_deref().unwrap_or("").trim()
    )
}

/// The type a tool call resolves to: the output schema's type, and `unknown`
/// without a schema (declarations.ts:99-102).
pub fn render_tool_output_type(schema: Option<&Value>) -> String {
    match schema {
        None => "unknown".to_owned(),
        Some(schema) => schema_to_type(schema, None),
    }
}

fn render_global(head: &str, global: &CodemodeToolInfo, indent: &str) -> String {
    if let Some(signature) = &global.signature {
        return format!(
            "{}{indent}{head}{signature};",
            doc_comment(global.description.as_deref(), indent)
        );
    }
    let input = match &global.input_schema {
        None => "unknown".to_owned(),
        Some(schema) => schema_to_type(schema, None),
    };
    let output = match &global.output_schema {
        None => "unknown".to_owned(),
        Some(schema) => schema_to_type(schema, None),
    };
    format!(
        "{}{indent}{head}(args: {input}): Promise<{output}>;",
        doc_comment(global.description.as_deref(), indent)
    )
}

fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let Some(text) = description.map(str::trim).filter(|text| !text.is_empty()) else {
        return String::new();
    };
    let text = text.replace("*/", "*\\/");
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body = lines
        .iter()
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}/**\n{body}\n{indent} */\n")
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for item in types {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    if unique.iter().any(|item| item == "unknown") {
        return "unknown".to_owned();
    }
    if unique.is_empty() {
        return "never".to_owned();
    }
    unique.join(" | ")
}

/// Convert a JSON Schema to a TypeScript type expression
/// (declarations.ts:133-153): a result longer than `max_chars` is rendered
/// again without constraint tags, and renders as `unknown` if it is still too
/// long.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        let mut context = SchemaContext {
            root: schema,
            resolving: Vec::new(),
            expansions: 0,
            tags,
        };
        to_type(schema, &mut context)
    };
    let rendered = render(true);
    let Some(max) = max_chars else {
        return rendered;
    };
    // JS `String.length` counts UTF-16 code units, not Unicode scalars.
    if rendered.encode_utf16().count() <= max {
        return rendered;
    }
    let untagged = render(false);
    if untagged.encode_utf16().count() <= max {
        untagged
    } else {
        "unknown".to_owned()
    }
}

struct SchemaContext<'a> {
    root: &'a Value,
    resolving: Vec<String>,
    expansions: usize,
    /// Whether property comments include constraint tags.
    tags: bool,
}

fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// `decodeURIComponent` for one JSON pointer segment: percent-escapes are
/// decoded as UTF-8; malformed escapes stay literal instead of throwing.
fn decode_uri_component(segment: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| segment.to_owned())
}

fn resolve_ref(reference: &str, root: &Value) -> Option<Value> {
    if reference == "#" {
        // Upstream's `ref.slice(2)` over `"#"` is empty, so the loop is
        // skipped and the root schema is returned.
        return Some(root.clone());
    }
    if !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference[2..].split('/').filter(|part| !part.is_empty()) {
        let key = decode_uri_component(segment)
            .replace("~1", "/")
            .replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    if current.is_boolean() || current.is_object() {
        Some(current.clone())
    } else {
        None
    }
}

fn to_type(schema: &Value, context: &mut SchemaContext<'_>) -> String {
    if schema.is_boolean() {
        return if schema.as_bool() == Some(true) {
            "unknown".to_owned()
        } else {
            "never".to_owned()
        };
    }
    let Some(object) = schema.as_object() else {
        return "unknown".to_owned();
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if context.resolving.iter().any(|item| item == reference)
            || context.expansions >= MAX_REF_EXPANSIONS
        {
            return "unknown".to_owned();
        }
        let Some(target) = resolve_ref(reference, context.root) else {
            return "unknown".to_owned();
        };
        context.expansions += 1;
        context.resolving.push(reference.to_owned());
        let rendered = to_type(&target, context);
        context.resolving.pop();
        return rendered;
    }

    if object.contains_key("const") {
        return serde_json::to_string(&object["const"]).unwrap_or_else(|_| "unknown".to_owned());
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return union(
            values
                .iter()
                .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
                .collect(),
        );
    }

    let variants = object
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| object.get("oneOf").and_then(Value::as_array));
    if let Some(variants) = variants {
        return union(
            variants
                .iter()
                .map(|variant| to_type(variant, context))
                .collect(),
        );
    }
    if let Some(parts) = object.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = parts
            .iter()
            .map(|part| to_type(part, context))
            .filter(|part| part != "unknown")
            .collect();
        if parts.is_empty() {
            return "unknown".to_owned();
        }
        return parts
            .iter()
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }

    let schema_type = object.get("type");
    if let Some(types) = schema_type.and_then(Value::as_array) {
        return union(
            types
                .iter()
                .map(|entry| {
                    let mut clone = object.clone();
                    clone.insert("type".to_owned(), entry.clone());
                    to_type(&Value::Object(clone), context)
                })
                .collect(),
        );
    }
    match schema_type.and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("number") | Some("integer") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => array_type(object, context),
        Some("object") => object_type(object, context),
        None => {
            if object.contains_key("properties")
                || object.contains_key("additionalProperties")
                || object.contains_key("required")
            {
                return object_type(object, context);
            }
            if object.contains_key("items") || object.contains_key("prefixItems") {
                return array_type(object, context);
            }
            "unknown".to_owned()
        }
        Some(_) => "unknown".to_owned(),
    }
}

fn array_type(object: &serde_json::Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    if let Some(items) = object.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, context));
    }
    let tuple: Vec<Value> = object
        .get("prefixItems")
        .and_then(Value::as_array)
        .or_else(|| object.get("items").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    if !tuple.is_empty() {
        return format!(
            "[{}]",
            tuple
                .iter()
                .map(|item| to_type(item, context))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    "unknown[]".to_owned()
}

fn description_of(property: &Value) -> String {
    property
        .as_object()
        .and_then(|object| object.get("description"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned()
}

/// JS `String(number)`: integral floats render without a trailing `.0`
/// (also above 1e15), and exponential notation uses an explicit exponent
/// sign like JS (`1e+21`, `1e-7`).
fn js_number_text(number: f64) -> String {
    if number == 0.0 {
        return "0".to_owned();
    }
    let magnitude = number.abs();
    // Rust's `Display` is the shortest representation that round-trips,
    // like JS, but JS switches to exponential notation outside
    // [1e-6, 1e21). Inside that window `Display` already matches JS digits
    // (including no trailing `.0`).
    if (1e-6..1e21).contains(&magnitude) && magnitude != 0.0 {
        return format!("{number}");
    }
    let text = serde_json::Number::from_f64(number)
        .map(|number| number.to_string())
        .unwrap_or_else(|| number.to_string());
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return text;
    };
    match exponent.parse::<i32>() {
        Ok(exponent) if exponent >= 0 => format!("{mantissa}e+{exponent}"),
        Ok(exponent) => format!("{mantissa}e{exponent}"),
        Err(_) => text,
    }
}

/// JS `String(value)` for the number and string values a tag can carry.
fn tag_value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(js_number_text(number.as_f64()?)),
        _ => None,
    }
}

/// Identity of a tag value for `Set` semantics (`new Set(values)`):
/// numbers compare by value (`1` and `1.0` are the same), and `-0` equals
/// `0` like JS `SameValueZero`.
fn tag_value_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(format!("s:{text}")),
        Value::Number(number) => {
            let value = number.as_f64()?;
            let value = if value == 0.0 { 0.0 } else { value };
            Some(format!("n:{:016x}", value.to_bits()))
        }
        _ => None,
    }
}

/// Replace the characters that would end a `//` comment with `\uXXXX`
/// escapes (declarations.ts:402-404).
fn escape_line_terminators(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    for ch in tag.chars() {
        if LINE_TERMINATORS.contains(&ch) {
            out.push_str(&format!("\\u{:04x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

/// Constraints of a property that its TypeScript type cannot express, as
/// JSDoc-style tags: `@integer @minimum 1 @maximum 100 @default 20`
/// (declarations.ts:253-301).
fn constraint_tags(property: &Value) -> String {
    let Some(property) = property.as_object() else {
        return String::new();
    };
    // Optional fields such as Pydantic's `int | None` keep their constraints
    // in the non-null branch. The branch's constraints apply together with
    // the property's, so both are read.
    let variants = property
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| property.get("oneOf").and_then(Value::as_array));
    let non_null: Vec<&Value> = variants
        .map(|variants| {
            variants
                .iter()
                .filter(|variant| {
                    !variant.is_object()
                        || variant.get("type").and_then(Value::as_str) != Some("null")
                })
                .collect()
        })
        .unwrap_or_default();
    let branch = non_null.first().copied().filter(|_| non_null.len() == 1);
    let mut schemas: Vec<Map<String, Value>> = vec![property.clone()];
    if let Some(branch) = branch.and_then(Value::as_object) {
        schemas.push(branch.clone());
    }
    // Draft-04 and OpenAPI 3.0 make `minimum`/`maximum` exclusive with a
    // boolean `exclusiveMinimum`/`exclusiveMaximum`.
    for schema in &mut schemas {
        if schema.get("exclusiveMinimum") == Some(&Value::Bool(true))
            && let Some(minimum) = schema.remove("minimum")
        {
            schema.insert("exclusiveMinimum".to_owned(), minimum);
        }
        if schema.get("exclusiveMaximum") == Some(&Value::Bool(true))
            && let Some(maximum) = schema.remove("maximum")
        {
            schema.insert("exclusiveMaximum".to_owned(), maximum);
        }
    }

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |schema: &Map<String, Value>| match schema.get("type") {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("integer")),
        _ => false,
    };
    if schemas.iter().any(is_integer) {
        tags.push("@integer".to_owned());
    }
    for keyword in TAG_KEYWORDS {
        let values: Vec<&Value> = schemas
            .iter()
            .map(|schema| schema.get(keyword).unwrap_or(&Value::Null))
            .collect();
        if LOWER_BOUNDS.contains(&keyword) || UPPER_BOUNDS.contains(&keyword) {
            let numbers: Vec<f64> = values.iter().filter_map(|value| value.as_f64()).collect();
            let Some(number) = (if LOWER_BOUNDS.contains(&keyword) {
                numbers.iter().copied().reduce(f64::max)
            } else {
                numbers.iter().copied().reduce(f64::min)
            }) else {
                continue;
            };
            tags.push(format!("@{keyword} {}", js_number_text(number)));
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        for value in values {
            let Some(key) = tag_value_key(value) else {
                continue;
            };
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if let Some(text) = tag_value_text(value) {
                tags.push(format!("@{keyword} {text}"));
            }
        }
    }
    if schemas
        .iter()
        .any(|schema| schema.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".to_owned());
    }
    // Annotations, unlike constraints, come from the property itself.
    if let Some(default) = property.get("default")
        && !default.is_null()
        && let Ok(serialized) = serde_json::to_string(default)
    {
        tags.push(format!("@default {serialized}"));
    }
    if property.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".to_owned());
    }
    tags.into_iter()
        .filter(|tag| tag.encode_utf16().count() <= MAX_TAG_CHARS)
        .map(|tag| escape_line_terminators(&tag))
        .collect::<Vec<_>>()
        .join(" ")
}

fn object_type(object: &serde_json::Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut names: Vec<String> = properties.keys().cloned().collect();
    names.sort();
    let mut members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(name) { "" } else { "?" };
            format!(
                "{}{optional}: {};",
                property_key(name),
                to_type(&properties[name], context)
            )
        })
        .collect();
    let additional = object.get("additionalProperties");
    match additional {
        Some(value) if value != &Value::Bool(false) => {
            let rendered = if value == &Value::Bool(true) {
                "unknown".to_owned()
            } else {
                to_type(value, context)
            };
            members.push(format!("[key: string]: {rendered};"));
        }
        None if names.is_empty() => {
            members.push("[key: string]: unknown;".to_owned());
        }
        _ => {}
    }
    if members.is_empty() {
        return "{}".to_owned();
    }
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let mut lines: Vec<String> = description_of(&properties[name])
                .split(LINE_TERMINATORS.as_slice())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            let tags = if context.tags {
                constraint_tags(&properties[name])
            } else {
                String::new()
            };
            if !tags.is_empty() {
                lines.push(tags);
            }
            lines
        })
        .collect();
    if !comments.iter().any(|comment| !comment.is_empty()) {
        return format!("{{ {} }}", members.join(" "));
    }

    let mut lines: Vec<String> = vec!["{".to_owned()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        lines.push(format!(
            "{INDENT}{}",
            members[index].replace('\n', &format!("\n{INDENT}"))
        ));
    }
    for member in members.iter().skip(names.len()) {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".to_owned());
    lines.join("\n")
}

/// Whether a value is a plain object (the upstream `isObject` helper).
pub fn is_plain_object(value: &Value) -> bool {
    is_object(value)
}

#[cfg(test)]
mod tests {
    //! Ports `packages/codemode/test/declarations.test.ts` @ 4a080252f.

    use super::*;
    use serde_json::json;

    #[test]
    fn renders_primitives_literals_and_unions() {
        assert_eq!(schema_to_type(&json!({ "type": "string" }), None), "string");
        assert_eq!(
            schema_to_type(&json!({ "type": "integer" }), None),
            "number"
        );
        assert_eq!(
            schema_to_type(&json!({ "type": ["string", "null"] }), None),
            "string | null"
        );
        assert_eq!(schema_to_type(&json!({ "const": "a" }), None), "\"a\"");
        assert_eq!(
            schema_to_type(&json!({ "enum": ["a", 1, null] }), None),
            "\"a\" | 1 | null"
        );
        assert_eq!(
            schema_to_type(
                &json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] }),
                None
            ),
            "string | number"
        );
        assert_eq!(
            schema_to_type(&json!({ "anyOf": [{ "type": "string" }, {}] }), None),
            "unknown"
        );
        assert_eq!(
            schema_to_type(
                &json!({ "allOf": [{ "anyOf": [{ "type": "string" }, { "type": "number" }] }, { "const": 1 }] }),
                None
            ),
            "(string | number) & 1"
        );
        assert_eq!(
            schema_to_type(&json!({ "$ref": "#/defs/x" }), None),
            "unknown"
        );
        assert_eq!(schema_to_type(&json!(true), None), "unknown");
        assert_eq!(schema_to_type(&json!(false), None), "never");
    }

    #[test]
    fn renders_objects_on_one_line_with_sorted_properties() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" }, "max-lines": { "type": "number" } },
                    "required": ["city"],
                    "additionalProperties": false,
                }),
                None
            ),
            "{ city: string; \"max-lines\"?: number; }"
        );
        assert_eq!(
            schema_to_type(
                &json!({ "type": "object", "additionalProperties": { "type": "number" } }),
                None
            ),
            "{ [key: string]: number; }"
        );
        assert_eq!(
            schema_to_type(&json!({ "type": "object" }), None),
            "{ [key: string]: unknown; }"
        );
        assert_eq!(
            schema_to_type(
                &json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                None
            ),
            "{}"
        );
    }

    #[test]
    fn puts_property_descriptions_on_comment_lines() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "weather": {
                            "type": "array",
                            "description": "look up weather for a given list of locations",
                            "items": { "type": "object", "properties": { "location": { "type": "string" } }, "required": ["location"] }
                        }
                    },
                    "required": ["weather"]
                }),
                None
            ),
            "{\n  // look up weather for a given list of locations\n  weather: Array<{ location: string; }>;\n}"
        );
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "outer": {
                            "type": "object",
                            "description": "Outer",
                            "properties": { "inner": { "type": "string", "description": "Inner" } }
                        }
                    }
                }),
                None
            ),
            "{\n  // Outer\n  outer?: {\n    // Inner\n    inner?: string;\n  };\n}"
        );
    }

    // https://github.com/earendil-works/pi/issues/10707
    #[test]
    fn puts_constraints_on_property_comment_lines() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "timeoutMs": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 30000,
                            "default": 0,
                            "description": "Wait in milliseconds."
                        },
                        "id": { "type": "string", "pattern": "^[a-z]+$", "maxLength": 8 },
                        "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 5, "uniqueItems": true, "deprecated": true },
                        "name": { "type": "string" }
                    },
                    "required": ["id"]
                }),
                None
            ),
            [
                "{",
                "  // @maxLength 8 @pattern ^[a-z]+$",
                "  id: string;",
                "  name?: string;",
                "  // @maxItems 5 @uniqueItems @deprecated",
                "  tags?: Array<string>;",
                "  // Wait in milliseconds.",
                "  // @integer @minimum 0 @maximum 30000 @default 0",
                "  timeoutMs?: number;",
                "}",
            ]
            .join("\n")
        );
    }

    #[test]
    fn reads_constraints_of_optional_fields_from_the_non_null_branch() {
        // Pydantic's `limit: int | None = Field(None, ge=1, le=100)`.
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "anyOf": [{ "type": "integer", "minimum": 1, "maximum": 100 }, { "type": "null" }],
                            "default": null,
                            "title": "Limit"
                        }
                    }
                }),
                None
            ),
            "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}"
        );
        // Constraints of the property and the branch both apply.
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "n": {
                            "anyOf": [{ "type": "integer", "minimum": 10, "maximum": 50, "format": "int32" }, { "type": "null" }],
                            "minimum": 0,
                            "maximum": 20,
                            "format": "port"
                        }
                    }
                }),
                None
            ),
            "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}"
        );
    }

    #[test]
    fn reads_draft_04_boolean_exclusive_bounds() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "n": { "type": "integer", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": false },
                        "m": { "anyOf": [{ "type": "number", "maximum": 5, "exclusiveMaximum": true }, { "type": "null" }], "maximum": 8 }
                    }
                }),
                None
            ),
            [
                "{",
                "  // @maximum 8 @exclusiveMaximum 5",
                "  m?: number | null;",
                "  // @integer @exclusiveMinimum 0 @maximum 10",
                "  n?: number;",
                "}",
            ]
            .join("\n")
        );
    }

    #[test]
    fn leaves_out_long_tags_and_escapes_line_terminators() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": { "config": { "type": "object", "default": { "text": "x".repeat(100) } } }
                }),
                None
            ),
            "{ config?: { [key: string]: unknown; }; }"
        );
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "sep": { "type": "string", "pattern": "^[\r\n\u{2028}\u{2029}]$", "default": "\u{2028}", "description": "a\u{2028}b" }
                    }
                }),
                None
            ),
            [
                "{",
                "  // a",
                "  // b",
                "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
                "  sep?: string;",
                "}",
            ]
            .join("\n")
        );
    }

    #[test]
    fn drops_constraint_tags_before_rendering_a_type_over_the_budget_as_unknown() {
        let schema =
            json!({ "type": "object", "properties": { "n": { "type": "integer", "minimum": 1 } } });
        assert_eq!(
            schema_to_type(&schema, None),
            "{\n  // @integer @minimum 1\n  n?: number;\n}"
        );
        assert_eq!(schema_to_type(&schema, Some(20)), "{ n?: number; }");
        assert_eq!(schema_to_type(&schema, Some(10)), "unknown");
    }

    #[test]
    fn resolves_local_references_and_stops_at_recursive_ones() {
        let schema = json!({
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/Item" },
                "legacy": { "$ref": "#/definitions/Legacy" },
                "remote": { "$ref": "https://example.com/schema.json" }
            },
            "required": ["item"],
            "$defs": {
                "Item": {
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "parent": { "$ref": "#/$defs/Item" } },
                    "required": ["id"]
                }
            },
            "definitions": { "Legacy": { "enum": ["a", "b"] } }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{ item: { id: string; parent?: unknown; }; legacy?: \"a\" | \"b\"; remote?: unknown; }"
        );
    }

    #[test]
    fn resolves_the_root_reference_and_percent_decoded_segments() {
        // `{"$ref":"#"}` resolves to the root and stops at the recursion
        // instead of panicking (upstream `ref.slice(2)` is empty).
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$ref": "#" } }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{ child?: { child?: unknown; }; }"
        );
        assert_eq!(schema_to_type(&json!({ "$ref": "#" }), None), "unknown");

        let schema = json!({
            "type": "object",
            "properties": { "item": { "$ref": "#/$defs/a%20b" } },
            "$defs": { "a b": { "type": "string" } }
        });
        assert_eq!(schema_to_type(&schema, None), "{ item?: string; }");
    }

    #[test]
    fn numeric_tags_dedupe_by_value_and_format_like_js() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": {
                    "type": "number",
                    "multipleOf": 1,
                    "anyOf": [{ "type": "number", "multipleOf": 1.0 }, { "type": "null" }],
                    "default": 1000000000000000000000.0
                }
            }
        });
        let rendered = schema_to_type(&schema, None);
        assert!(
            rendered.contains("@multipleOf 1 @default 1e+21"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("@multipleOf 1").count(), 1, "{rendered}");
        // `-0` equals `0` for tag dedupe.
        let schema = json!({
            "type": "object",
            "properties": {
                "n": { "multipleOf": 0, "anyOf": [{ "multipleOf": -0.0 }, { "type": "null" }] }
            }
        });
        let rendered = schema_to_type(&schema, None);
        assert_eq!(rendered.matches("@multipleOf").count(), 1, "{rendered}");

        // JS `String(number)` uses the shortest round-trip digits inside
        // [1e-6, 1e21) and exponential notation outside it.
        assert_eq!(js_number_text(1.0), "1");
        assert_eq!(js_number_text(1.5), "1.5");
        assert_eq!(
            js_number_text(1.2345678901234567e19),
            "12345678901234567000"
        );
        assert_eq!(js_number_text(1e21), "1e+21");
        assert_eq!(js_number_text(1e-7), "1e-7");
        assert_eq!(js_number_text(-0.0), "0");
    }

    #[test]
    fn renders_arrays_and_tuples() {
        assert_eq!(
            schema_to_type(
                &json!({ "type": "array", "items": { "type": "string" } }),
                None
            ),
            "Array<string>"
        );
        assert_eq!(
            schema_to_type(
                &json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "number" }] }),
                None
            ),
            "[string, number]"
        );
        assert_eq!(
            schema_to_type(&json!({ "type": "array" }), None),
            "unknown[]"
        );
    }

    #[test]
    fn renders_types_over_the_budget_as_unknown() {
        let properties: serde_json::Map<String, Value> = (0..50)
            .map(|index| (format!("field{index}"), json!({ "type": "string" })))
            .collect();
        let schema = json!({ "type": "object", "properties": properties });
        assert_eq!(schema_to_type(&schema, Some(100)), "unknown");
        assert!(schema_to_type(&schema, None).contains("field49?: string;"));
    }

    #[test]
    fn renders_signatures_with_normalized_identifiers() {
        assert_eq!(
            render_tool_signature(
                &CodemodeToolInfo {
                    name: "hidden-dynamic-tool".to_owned(),
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"],
                        "additionalProperties": false
                    })),
                    output_schema: Some(
                        json!({ "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"] })
                    ),
                    ..Default::default()
                },
                None
            ),
            "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
        );
        assert_eq!(
            render_tool_signature(
                &CodemodeToolInfo {
                    name: "free".to_owned(),
                    ..Default::default()
                },
                None
            ),
            "free(args: unknown): Promise<unknown>;"
        );
    }

    #[test]
    fn renders_output_schemas_shaped_like_an_mcp_call_tool_result_as_plain_types() {
        let output_schema = json!({
            "type": "object",
            "properties": {
                "content": { "type": "array", "items": { "type": "object" } },
                "isError": { "type": "boolean" },
                "_meta": { "type": "object" }
            },
            "required": ["content"]
        });
        assert_eq!(
            render_tool_signature(
                &CodemodeToolInfo {
                    name: "plain".to_owned(),
                    input_schema: Some(json!({ "type": "object", "properties": {} })),
                    output_schema: Some(output_schema),
                    ..Default::default()
                },
                None
            ),
            "plain(args: { [key: string]: unknown; }): Promise<{ _meta?: { [key: string]: unknown; }; content: Array<{ [key: string]: unknown; }>; isError?: boolean; }>;"
        );
    }

    #[test]
    fn renders_the_per_tool_sample() {
        assert_eq!(
            render_tool_sample(&CodemodeToolInfo {
                name: "foo".to_owned(),
                description: Some("bar".to_owned()),
                input_schema: Some(json!({ "type": "string" })),
                ..Default::default()
            }),
            "bar\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { foo(args: string): Promise<unknown>; };\n```"
        );
    }

    #[test]
    fn renders_tools_and_globals() {
        let tools = vec![
            CodemodeToolInfo {
                name: "read".to_owned(),
                description: Some("Read a file.\nSecond line.".to_owned()),
                input_schema: Some(
                    json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
                ),
                output_schema: Some(json!({ "type": "string" })),
                signature: None,
            },
            CodemodeToolInfo {
                name: "remote-api".to_owned(),
                ..Default::default()
            },
        ];
        let globals = vec![CodemodeToolInfo {
            name: "attach".to_owned(),
            description: Some("Attach it.".to_owned()),
            input_schema: Some(json!({ "type": "string" })),
            ..Default::default()
        }];
        let text = render_declarations(RenderDeclarationsOptions {
            tools: &tools,
            globals: &globals,
        });
        assert_eq!(
            text,
            [
                "declare const tools: {",
                "  /**",
                "   * Read a file.",
                "   * Second line.",
                "   */",
                "  read(args: { path: string; }): Promise<string>;",
                "  remote_api(args: unknown): Promise<unknown>;",
                "};",
                "",
                "/** Attach it. */",
                "declare function attach(args: string): Promise<unknown>;",
            ]
            .join("\n")
        );
    }

    #[test]
    fn renders_namespaced_globals_and_explicit_signatures() {
        let globals = vec![
            CodemodeToolInfo {
                name: "models.list".to_owned(),
                description: Some("List models.".to_owned()),
                signature: Some("(type: string): Promise<string[]>".to_owned()),
                ..Default::default()
            },
            CodemodeToolInfo {
                name: "models.get".to_owned(),
                input_schema: Some(json!({ "type": "string" })),
                ..Default::default()
            },
            CodemodeToolInfo {
                name: "plain".to_owned(),
                signature: Some("(): void".to_owned()),
                ..Default::default()
            },
        ];
        let text = render_declarations(RenderDeclarationsOptions {
            tools: &[],
            globals: &globals,
        });
        assert_eq!(
            text,
            [
                "declare function plain(): void;",
                "",
                "declare const models: {",
                "  /** List models. */",
                "  list(type: string): Promise<string[]>;",
                "  get(args: string): Promise<unknown>;",
                "};",
            ]
            .join("\n")
        );
    }

    #[test]
    fn escapes_comment_terminators_in_descriptions() {
        let tools = vec![CodemodeToolInfo {
            name: "x".to_owned(),
            description: Some("a */ b".to_owned()),
            ..Default::default()
        }];
        let text = render_declarations(RenderDeclarationsOptions {
            tools: &tools,
            globals: &[],
        });
        assert!(text.contains("/** a *\\/ b */"));
    }
}
