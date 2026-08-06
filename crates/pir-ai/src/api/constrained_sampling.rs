//! Port of `packages/ai/src/api/constrained-sampling.ts` @ pi 0.82.1
//! (2efa728).
//!
//! JSON-schema strict sampling (Anthropic) and OpenAI grammar constrained
//! sampling helpers. Fallible operations return `Err(String)` carrying the
//! upstream `Error.message`; adapters surface them through the stream error
//! path.

use std::collections::HashMap;

use crate::types::{
    ConstrainedSampling, ConstrainedSamplingConfig, ConstrainedSamplingStrict, Tool,
};

/// `GrammarConstrainedSampling`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    pub format: GrammarOutFormat,
    pub definition: String,
    pub input_property: String,
}

/// Output-side grammar format (`"lark" | "regex"` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarOutFormat {
    Lark,
    Regex,
}

impl GrammarOutFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lark => "lark",
            Self::Regex => "regex",
        }
    }
}

/// `GrammarToolInputJsonBuffer`.
#[derive(Debug, Clone, Default)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

/// `getGrammarToolInput`.
pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
    input_property: &str,
) -> Result<String, String> {
    match arguments.get(input_property) {
        Some(serde_json::Value::String(input)) => Ok(input.clone()),
        _ => Err(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
        )),
    }
}

/// `appendGrammarToolInputJsonDelta`.
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, String> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        ));
    }
    if !next_input.starts_with(&buffer.input) {
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        ));
    }

    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        delta.push('{');
        delta.push_str(&serde_json::to_string(input_property).unwrap_or_default());
        delta.push_str(":\"");
        buffer.started = true;
    }
    // JSON.stringify(inputDelta).slice(1, -1): escape without the quotes.
    let quoted = serde_json::to_string(input_delta).unwrap_or_default();
    if quoted.len() >= 2 {
        delta.push_str(&quoted[1..quoted.len() - 1]);
    }
    buffer.input = next_input.to_owned();

    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type") != Some(&serde_json::Value::String("object".to_owned())) {
        return Err("grammar constrained sampling requires an object parameter schema".to_owned());
    }
    let required = schema.get("required").and_then(|r| r.as_array());
    let input_property =
        match required {
            Some(entries) if entries.len() == 1 => match entries[0].as_str() {
                Some(name) => name.to_owned(),
                None => return Err(
                    "grammar constrained sampling requires exactly one required string property"
                        .to_owned(),
                ),
            },
            _ => {
                return Err(
                    "grammar constrained sampling requires exactly one required string property"
                        .to_owned(),
                );
            }
        };

    let property = schema
        .get("properties")
        .and_then(|properties| properties.get(&input_property));
    match property {
        None => Err(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        )),
        Some(property)
            if property.get("type") == Some(&serde_json::Value::String("string".to_owned())) =>
        {
            Ok(input_property)
        }
        Some(_) => Err(format!(
            "grammar constrained sampling property {input_property} must have type string"
        )),
    }
}

/// `resolveJsonSchemaStrictSampling`.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let strict = match &tool.constrained_sampling {
        Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })) => {
            *strict
        }
        _ => return Ok(None),
    };

    if supports_strict_mode {
        return Ok(Some(true));
    }
    if strict == ConstrainedSamplingStrict::Require {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// `resolveGrammarConstrainedSampling`.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_open_ai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    let variants = match &tool.constrained_sampling {
        Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::Grammar { variants })) => {
            variants
        }
        _ => return Ok(None),
    };

    if !supports_open_ai_grammar_tools {
        return Ok(None);
    }

    let lark_definition = variants.openai_lark.as_deref();
    let regex_definition = variants.openai_regex.as_deref();
    let has_lark_definition = lark_definition.is_some_and(|d| !d.trim().is_empty());
    let has_regex_definition = regex_definition.is_some_and(|d| !d.trim().is_empty());
    if !has_lark_definition && !has_regex_definition {
        return Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        ));
    }

    let (format, definition) = if has_lark_definition {
        // invariant: has_lark_definition implies openai_lark is Some
        (
            GrammarOutFormat::Lark,
            lark_definition.unwrap_or_default().to_owned(),
        )
    } else {
        // invariant: !has_lark && has_regex implies openai_regex is Some
        (
            GrammarOutFormat::Regex,
            regex_definition.unwrap_or_default().to_owned(),
        )
    };

    match infer_grammar_input_property(tool) {
        Ok(input_property) => Ok(Some(GrammarConstrainedSampling {
            format,
            definition,
            input_property,
        })),
        Err(message) => Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {message}.",
            tool.name
        )),
    }
}

/// `createGrammarToolInputProperties`: tool name → grammar input property.
pub fn create_grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_open_ai_grammar_tools: bool,
) -> Result<HashMap<String, String>, String> {
    let mut properties = HashMap::new();
    for tool in tools.unwrap_or(&[]) {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_open_ai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(constrained_sampling: serde_json::Value) -> Tool {
        serde_json::from_value(json!({
            "name": "t", "description": "d",
            "parameters": {"type": "object", "properties": {}, "required": []},
            "constrainedSampling": constrained_sampling,
        }))
        .expect("tool")
    }

    #[test]
    fn test_resolve_json_schema_strict_sampling() {
        let prefer = tool(json!({"type": "json_schema", "strict": "prefer"}));
        let require = tool(json!({"type": "json_schema", "strict": "require"}));
        let none = Tool {
            constrained_sampling: None,
            ..prefer.clone()
        };

        assert_eq!(resolve_json_schema_strict_sampling(&none, true), Ok(None));
        assert_eq!(
            resolve_json_schema_strict_sampling(&prefer, true),
            Ok(Some(true))
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&prefer, false),
            Ok(None)
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&require, true),
            Ok(Some(true))
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&require, false),
            Err("Tool \"t\" requires JSON-schema constrained sampling, but strict tools are unsupported.".to_owned())
        );
    }

    #[test]
    fn test_append_grammar_tool_input_json_delta() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        // No-op delta before close yields None.
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "input", "", false),
            Ok(None)
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "input", "hel", false),
            Ok(Some("{\"input\":\"hel".to_owned()))
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "input", "hello", true),
            Ok(Some("lo\"}".to_owned()))
        );
        assert!(buffer.closed);
        // Idempotent close after closed is a no-op.
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "input", "hello", true),
            Ok(None)
        );
    }

    #[test]
    fn test_append_grammar_tool_input_json_delta_non_monotonic() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        append_grammar_tool_input_json_delta(&mut buffer, "input", "abc", false).expect("delta");
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "input", "abx", false),
            Err("grammar tool input for property \"input\" changed non-monotonically".to_owned())
        );
    }

    #[test]
    fn test_resolve_grammar_constrained_sampling() {
        let grammar_tool: Tool = serde_json::from_value(json!({
            "name": "g", "description": "d",
            "parameters": {
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"]
            },
            "constrainedSampling": {"type": "grammar", "variants": {"openai_lark": "start: x"}}
        }))
        .expect("tool");

        // Unsupported provider → None.
        assert_eq!(
            resolve_grammar_constrained_sampling(&grammar_tool, false),
            Ok(None)
        );
        let grammar = resolve_grammar_constrained_sampling(&grammar_tool, true)
            .expect("ok")
            .expect("grammar");
        assert_eq!(grammar.format, GrammarOutFormat::Lark);
        assert_eq!(grammar.definition, "start: x");
        assert_eq!(grammar.input_property, "input");
    }

    #[test]
    fn test_get_grammar_tool_input() {
        let mut arguments = serde_json::Map::new();
        arguments.insert("input".to_owned(), json!("value"));
        assert_eq!(
            get_grammar_tool_input("t", &arguments, "input"),
            Ok("value".to_owned())
        );
        arguments.insert("input".to_owned(), json!(42));
        assert_eq!(
            get_grammar_tool_input("t", &arguments, "input"),
            Err("Grammar tool call \"t\" requires argument \"input\" to be a string.".to_owned())
        );
    }
}
