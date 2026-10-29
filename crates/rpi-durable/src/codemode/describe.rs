//! What the model reads about code mode (port of
//! `packages/durable/src/codemode/describe.ts` @ `4a080252f`).
//!
//! The module carries the fixed tool description, the deterministic
//! TypeScript-style declaration rendering used by the `codemode` prompt
//! section, namespace grouping, and the tool catalog a sandbox's
//! `searchTools()`/`describeTool()`/`describeNamespace()` answer from.
//!
//! The upstream renderer lives in `@earendil-works/pi-codemode/declarations`;
//! rpi-durable cannot depend on the QuickJS runtime, so [`schema_to_type`]
//! renders a compact, deterministic declaration instead. It covers the JSON
//! Schema shapes the harness tools use; unknown constructs become `unknown`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness::types::ToolRegistration;
use crate::search::{SearchDocument, rank_documents};

use super::models::MODELS_DESCRIPTION;

/// Default token budget of the tool declarations in the `codemode` section.
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;
/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: usize = 4;
/// Default result count of `searchTools()`.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;

const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.\n- The script survives a restart: it resumes where it was, without running finished calls again.\n- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// `toCodemodeIdentifier`: characters invalid in a JavaScript identifier become
/// `_`; the first character must be a letter, `_`, or `$`.
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for character in name.chars() {
        let valid = if identifier.is_empty() {
            character.is_ascii_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        };
        identifier.push(if valid { character } else { '_' });
    }
    if identifier.is_empty() {
        identifier.push('_');
    }
    identifier
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A group of tools named by a common prefix, such as an MCP server's
/// `mcp__github__`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNamespace {
    /// Namespace name shown to scripts.
    pub name: String,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Tool names starting with it belong to the namespace; the longest
    /// matching prefix wins.
    pub prefix: String,
}

/// One tool as scripts see it: `tools.<jsName>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolInfo {
    /// Raw tool name.
    pub name: String,
    /// Identifier scripts use.
    pub js_name: String,
    /// The tool's description and declaration, as `sampleOf` renders it.
    pub description: String,
    /// The tool declares a structured-output schema.
    #[serde(default, skip_serializing_if = "is_false")]
    pub structured: bool,
    /// An internal tool of this extension; never listed to scripts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

/// One namespace in start data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNamespaceInfo {
    /// Namespace name.
    pub name: String,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Raw names of the tools in this namespace.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// One global in start data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeGlobalInfo {
    /// Global name, such as `searchTools` or `models.getModelsOfType`.
    pub name: String,
    /// Whether the sandbox exposes it as a spread namespace object.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spread: bool,
}

/// What a script sees of a tool: its declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeDeclaration {
    /// Raw tool name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Input JSON Schema.
    pub input_schema: Value,
    /// Output JSON Schema; `{ "type": "string" }` without one.
    pub output_schema: Value,
}

impl CodemodeToolInfo {
    /// The tool catalog entry for one registration.
    pub fn of(tool: &ToolRegistration) -> Self {
        Self {
            name: tool.name().to_string(),
            js_name: to_codemode_identifier(tool.name()),
            description: sample_of(tool),
            structured: tool.structured_output_schema.is_some(),
            hidden: tool
                .name()
                .starts_with(super::models::CODEMODE_INTERNAL_PREFIX),
        }
    }
}

/// The code mode tool's fixed description: the script environment and its
/// globals.
pub fn codemode_description(models: bool) -> String {
    let globals = [
        "Globals:",
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.",
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.",
        "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find tools the `codemode` section does not list.",
    ];
    let mut sections = vec![DESCRIPTION_INTRO.to_string(), globals.join("\n")];
    if models {
        sections.push(MODELS_DESCRIPTION.to_string());
    }
    sections.push(
        "The `codemode` section of the system prompt lists the tools scripts can call, by namespace, as far as space allows."
            .to_string(),
    );
    sections.join("\n\n")
}

/// `declarationOf`: a tool's name, description, and schemas.
pub fn declaration_of(tool: &ToolRegistration) -> CodemodeDeclaration {
    CodemodeDeclaration {
        name: tool.name().to_string(),
        description: tool.tool.description.clone(),
        input_schema: tool.tool.parameters.clone(),
        output_schema: tool
            .structured_output_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "string" })),
    }
}

/// `sampleOf`: a tool's description and a TypeScript declaration.
pub fn sample_of(tool: &ToolRegistration) -> String {
    let declaration = declaration_of(tool);
    let signature = render_tool_signature(&declaration);
    format!(
        "{}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {signature} }};\n```",
        tool.tool.description.trim()
    )
}

fn render_tool_signature(declaration: &CodemodeDeclaration) -> String {
    format!(
        "{}(args: {}): Promise<{}>;",
        to_codemode_identifier(&declaration.name),
        schema_to_type(&declaration.input_schema, 0),
        schema_to_type(&declaration.output_schema, 0)
    )
}

/// A compact deterministic TypeScript type for a JSON Schema.
pub fn schema_to_type(schema: &Value, depth: usize) -> String {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return "unknown".to_string();
    }
    if let Some(union) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        let mut parts: Vec<String> = union
            .iter()
            .map(|variant| schema_to_type(variant, depth + 1))
            .collect();
        parts.sort();
        parts.dedup();
        return parts.join(" | ");
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.is_empty()
    {
        return values
            .iter()
            .map(|value| match value {
                Value::String(text) => format!("{:?}", text),
                other => compact(other),
            })
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        // Local references are not expanded: the declaration stays compact and
        // deterministic, and the raw schema remains available on the tool.
        return format!("unknown /* {reference} */");
    }
    let kinds: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if kinds.is_empty() {
        return if schema.get("properties").is_some() {
            object_type(schema, depth)
        } else {
            "unknown".to_string()
        };
    }
    let mut parts: Vec<String> = kinds
        .into_iter()
        .map(|kind| match kind {
            "string" => "string".to_string(),
            "integer" | "number" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => format!(
                "Array<{}>",
                schema
                    .get("items")
                    .map(|items| schema_to_type(items, depth + 1))
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            "object" => object_type(schema, depth),
            _ => "unknown".to_string(),
        })
        .collect();
    parts.sort();
    parts.dedup();
    parts.join(" | ")
}

fn object_type(schema: &Value, depth: usize) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return "Record<string, unknown>".to_string();
    };
    if properties.is_empty() {
        return "Record<string, unknown>".to_string();
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut fields: Vec<String> = properties
        .iter()
        .map(|(name, property)| {
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!(
                "{}{optional}: {};",
                safe_property(name),
                schema_to_type(property, depth + 1)
            )
        })
        .collect();
    fields.sort();
    format!("{{ {} }}", fields.join(" "))
}

fn safe_property(name: &str) -> String {
    let identifier = to_codemode_identifier(name);
    if identifier == name {
        identifier
    } else {
        format!("{identifier:?}")
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_string())
}

/// `namespaceOf`: the namespace with the longest prefix the tool name starts
/// with.
pub fn namespace_of<'a>(
    name: &str,
    namespaces: &'a [CodemodeNamespace],
) -> Option<&'a CodemodeNamespace> {
    let mut found: Option<&CodemodeNamespace> = None;
    for namespace in namespaces {
        if name.starts_with(&namespace.prefix)
            && found.is_none_or(|current| namespace.prefix.len() > current.prefix.len())
        {
            found = Some(namespace);
        }
    }
    found
}

fn render_tool_section(tool: &ToolRegistration) -> String {
    let id = to_codemode_identifier(tool.name());
    let heading = if id == tool.name() {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", tool.name())
    };
    format!("{heading}\n{}", sample_of(tool).trim())
}

struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

struct CatalogGroup {
    namespace: Option<CodemodeNamespace>,
    entries: Vec<CatalogEntry>,
}

/// Pick the tool sections that fit the budget: in each round every group (tools
/// without a namespace first, then namespaces by name) places its cheapest
/// remaining tool; a group whose next tool does not fit drops out while the
/// others continue. Every namespace is represented before any namespace is
/// complete.
fn select_catalog(groups: &[CatalogGroup], budget: usize) -> std::collections::HashSet<String> {
    let mut queues: Vec<Vec<CatalogEntry>> = groups
        .iter()
        .map(|group| {
            let mut entries: Vec<CatalogEntry> = group
                .entries
                .iter()
                .map(|entry| CatalogEntry {
                    name: entry.name.clone(),
                    section: entry.section.clone(),
                    cost: entry.cost,
                })
                .collect();
            entries.sort_by_key(|entry| entry.cost);
            entries
        })
        .collect();
    let mut shown = std::collections::HashSet::new();
    let mut remaining = budget;
    let mut active: Vec<usize> = (0..queues.len())
        .filter(|index| !queues[*index].is_empty())
        .collect();
    while !active.is_empty() {
        let mut next_active = Vec::new();
        for index in active {
            let Some(next) = queues[index].first() else {
                continue;
            };
            if next.cost > remaining {
                continue;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queues[index].remove(0);
            if !queues[index].is_empty() {
                next_active.push(index);
            }
        }
        active = next_active;
    }
    shown
}

/// The `codemode` section: the tools scripts can call, grouped by namespace,
/// with their declarations as far as the budget allows. `None` without tools.
pub fn codemode_section(
    tools: &[ToolRegistration],
    namespaces: &[CodemodeNamespace],
    budget: usize,
) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut groups: Vec<CatalogGroup> = vec![CatalogGroup {
        namespace: None,
        entries: Vec::new(),
    }];
    for tool in tools {
        let namespace = namespace_of(tool.name(), namespaces);
        let key = namespace.map(|namespace| namespace.name.clone());
        let index = groups.iter().position(|group| {
            group.namespace.as_ref().map(|candidate| &candidate.name) == key.as_ref()
        });
        let index = match index {
            Some(index) => index,
            None => {
                groups.push(CatalogGroup {
                    namespace: namespace.cloned(),
                    entries: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let section = render_tool_section(tool);
        groups[index].entries.push(CatalogEntry {
            name: tool.name().to_string(),
            cost: section.len().div_ceil(CHARS_PER_TOKEN),
            section,
        });
    }
    groups.sort_by(|left, right| match (&left.namespace, &right.namespace) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.name.cmp(&right.name),
    });
    let shown = select_catalog(&groups, budget);

    let mut tool_sections: Vec<String> =
        vec!["Tools scripts can call (`await tools.<name>(args)`):".to_string()];
    for group in &groups {
        if group.entries.is_empty() {
            continue;
        }
        let visible: Vec<&CatalogEntry> = group
            .entries
            .iter()
            .filter(|entry| shown.contains(&entry.name))
            .collect();
        if let Some(namespace) = &group.namespace {
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace.description.as_deref().unwrap_or("").trim();
            tool_sections.push(format!(
                "## {}{listing}{}",
                namespace.name,
                if description.is_empty() {
                    String::new()
                } else {
                    format!("\n{description}")
                }
            ));
        } else if visible.len() < group.entries.len() {
            tool_sections
                .push("Some tools are not listed: find them with `searchTools()`.".to_string());
        }
        for entry in visible {
            tool_sections.push(entry.section.clone());
        }
    }
    Some(tool_sections.join("\n\n"))
}

/// The catalog a sandbox answers `ALL_TOOLS`, `searchTools()`, `describeTool()`
/// and `describeNamespace()` from. Built from start data, so a replay sees what
/// the first run saw.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeCatalog {
    /// Visible and hidden tools.
    #[serde(default)]
    pub tools: Vec<CodemodeToolInfo>,
    /// Namespaces.
    #[serde(default)]
    pub namespaces: Vec<CodemodeNamespaceInfo>,
}

impl CodemodeCatalog {
    /// Build a catalog.
    pub fn new(tools: Vec<CodemodeToolInfo>, namespaces: Vec<CodemodeNamespaceInfo>) -> Self {
        Self { tools, namespaces }
    }

    /// Visible tools, in start order.
    pub fn visible_tools(&self) -> impl Iterator<Item = &CodemodeToolInfo> {
        self.tools.iter().filter(|tool| !tool.hidden)
    }

    fn namespace_for(&self, tool_name: &str) -> Option<&CodemodeNamespaceInfo> {
        self.namespaces
            .iter()
            .find(|namespace| namespace.tools.iter().any(|name| name == tool_name))
    }

    fn matches_namespace(&self, namespace: &CodemodeNamespaceInfo, query: &str) -> bool {
        namespace.name == query
            || to_codemode_identifier(&namespace.name) == to_codemode_identifier(query)
    }

    fn resolve_namespace(&self, query: &str) -> Option<&CodemodeNamespaceInfo> {
        self.namespaces
            .iter()
            .find(|namespace| self.matches_namespace(namespace, query))
    }

    /// `searchTools()`: BM25 over visible tools, optionally filtered to one
    /// namespace. Each match is `{ name, description }` with the script name.
    pub fn search(&self, query: &str, limit: usize, namespace: Option<&str>) -> Vec<Value> {
        let filter = namespace.and_then(|query| self.resolve_namespace(query));
        if namespace.is_some() && filter.is_none() {
            return Vec::new();
        }
        let mut documents: Vec<SearchDocument> = Vec::new();
        for tool in self.visible_tools() {
            let own = self.namespace_for(&tool.name);
            if let Some(filter) = filter
                && own.map(|namespace| namespace.name.as_str()) != Some(filter.name.as_str())
            {
                continue;
            }
            let text = [
                tool.name.clone(),
                tool.name.replace('_', " "),
                tool.description.clone(),
                own.map(|namespace| namespace.name.clone())
                    .unwrap_or_default(),
                own.and_then(|namespace| namespace.description.clone())
                    .unwrap_or_default(),
            ]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            documents.push(SearchDocument {
                name: tool.name.clone(),
                text,
            });
        }
        rank_documents(query, &documents, limit)
            .into_iter()
            .filter_map(|name| {
                self.tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .map(|tool| json!({ "name": tool.js_name, "description": tool.description }))
            })
            .collect()
    }

    /// `describeTool()`: the tool's description, or `None`.
    pub fn describe_tool(&self, name: &str) -> Option<Value> {
        self.visible_tools()
            .find(|tool| tool.name == name || tool.js_name == name)
            .map(|tool| Value::String(tool.description.clone()))
    }

    /// `describeNamespace()`: `{ name, description?, instructions?, tools }`.
    pub fn describe_namespace(&self, name: &str) -> Option<Value> {
        let namespace = self.resolve_namespace(name)?;
        let mut object = serde_json::Map::new();
        object.insert("name".to_string(), Value::String(namespace.name.clone()));
        if let Some(description) = &namespace.description {
            object.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        if let Some(instructions) = &namespace.instructions {
            object.insert(
                "instructions".to_string(),
                Value::String(instructions.clone()),
            );
        }
        let tools: Vec<Value> = namespace
            .tools
            .iter()
            .map(|name| Value::String(to_codemode_identifier(name)))
            .collect();
        object.insert("tools".to_string(), Value::Array(tools));
        Some(Value::Object(object))
    }

    /// `ALL_TOOLS`: every visible tool, as `{ name, jsName, description }`.
    pub fn all_tools(&self) -> Vec<Value> {
        self.visible_tools()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "jsName": tool.js_name,
                    "description": tool.description,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_matches_the_upstream_mapping() {
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("9lives"), "_lives");
        assert_eq!(to_codemode_identifier(""), "_");
    }

    #[test]
    fn namespace_of_picks_the_longest_prefix() {
        let namespaces = vec![
            CodemodeNamespace {
                name: "mcp".to_string(),
                prefix: "mcp__".to_string(),
                ..Default::default()
            },
            CodemodeNamespace {
                name: "github".to_string(),
                prefix: "mcp__github__".to_string(),
                ..Default::default()
            },
        ];
        assert_eq!(
            namespace_of("mcp__github__search", &namespaces)
                .map(|namespace| namespace.name.as_str()),
            Some("github")
        );
    }
}
