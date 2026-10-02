//! Model-facing codemode description (port of the description helpers in
//! `packages/coding-agent/src/extensions/codemode/tool.ts` @ a13d35a74,
//! v1.0.0 slimmed form). The rendered text is a byte-parity surface.

use std::collections::{HashMap, HashSet};

use rpi_codemode::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, mcp_structured_content_schema, render_tool_output_type,
    render_tool_sample, to_codemode_identifier,
};
use rpi_ext_host::types::ToolNamespace;

use crate::config::get_docs_path;
use crate::extensions::codemode::tool::{ToolInfo, to_codemode_declaration};

/// Characters per token when estimating the cost of a tool section
/// (tool.ts:156).
const CHARS_PER_TOKEN: usize = 4;

const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. Calls still running when the script ends are cancelled.\n- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// `CODEMODE_DOCS_PATH` (tool.ts:133): the reference model-facing doc.
pub fn codemode_docs_path() -> String {
    get_docs_path()
        .join("codemode.md")
        .to_string_lossy()
        .into_owned()
}

/// One line per global; the details live in the docs path (tool.ts:140-151).
fn describe_globals(models: bool) -> String {
    let mut lines = vec![
        "Globals:".to_owned(),
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script.".to_owned(),
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.".to_owned(),
        "- `ALL_TOOLS`, `searchTools(query, { limit?, namespace? })`, `describeTool(name)`, `describeNamespace(name)`: find unlisted tools, such as MCP tools.".to_owned(),
    ];
    if models {
        lines.push(format!(
            "- `models`: classifiers and image generation. Read {} first.",
            codemode_docs_path()
        ));
    }
    lines.join("\n")
}

/// `CodemodeDescriptionOptions` (tool.ts:173-185).
#[derive(Default)]
pub struct CodemodeDescriptionOptions {
    pub models: bool,
    /// Namespace of each tool, by tool name.
    pub namespaces: HashMap<String, ToolNamespace>,
    /// Tools that are callable but never listed with their declaration
    /// (`deferred` exposure).
    pub deferred: HashSet<String>,
    /// Estimated tokens (characters / 4) the tool sections may use.
    pub inline_budget: Option<usize>,
}

/// `### \`id\` (\`raw name\`)` followed by the tool's description and
/// declaration (tool.ts:188-192).
fn render_tool_section(declaration: &rpi_codemode::CodemodeToolInfo) -> String {
    let id = to_codemode_identifier(&declaration.name);
    let heading = if id == declaration.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", declaration.name)
    };
    format!("{heading}\n{}", render_tool_sample(declaration).trim())
}

struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

struct CatalogGroup {
    namespace: Option<ToolNamespace>,
    entries: Vec<CatalogEntry>,
}

/// Pick the tool sections that fit the budget (tool.ts:211-235).
fn select_catalog(groups: &[CatalogGroup], budget: Option<usize>) -> HashSet<String> {
    let Some(budget) = budget else {
        return groups
            .iter()
            .flat_map(|group| group.entries.iter().map(|entry| entry.name.clone()))
            .collect();
    };
    // Insertion order preserved; each group's queue is cheapest-first.
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
    let mut shown = HashSet::new();
    let mut remaining = budget;
    let mut active: Vec<usize> = (0..queues.len())
        .filter(|index| !queues[*index].is_empty())
        .collect();
    while !active.is_empty() {
        let mut next_active = Vec::new();
        for index in active {
            let next = queues[index]
                .first()
                .map(|entry| entry.cost)
                .unwrap_or(usize::MAX);
            if next > remaining {
                continue;
            }
            remaining -= next;
            let entry = queues[index].remove(0);
            shown.insert(entry.name);
            if !queues[index].is_empty() {
                next_active.push(index);
            }
        }
        active = next_active;
    }
    shown
}

/// `createCodemodeDescription` (tool.ts:237-291).
pub fn create_codemode_description(
    tools: &[ToolInfo],
    options: &CodemodeDescriptionOptions,
) -> String {
    let declarations: Vec<rpi_codemode::CodemodeToolInfo> = tools
        .iter()
        .filter(|tool| !options.deferred.contains(&tool.name))
        .map(to_codemode_declaration)
        .collect();
    // Insertion-ordered groups: unnamed first, then namespaces by name.
    let mut groups: Vec<CatalogGroup> = vec![CatalogGroup {
        namespace: None,
        entries: Vec::new(),
    }];
    let mut group_index: HashMap<String, usize> = HashMap::new();
    for declaration in &declarations {
        let (key, namespace) = match options.namespaces.get(&declaration.name) {
            Some(namespace) => (format!("ns:{}", namespace.name), Some(namespace.clone())),
            None => (String::new(), None),
        };
        let index = match group_index.get(&key) {
            Some(index) => *index,
            None => {
                let index = groups.len();
                groups.push(CatalogGroup {
                    namespace,
                    entries: Vec::new(),
                });
                group_index.insert(key, index);
                index
            }
        };
        let section = render_tool_section(declaration);
        groups[index].entries.push(CatalogEntry {
            name: declaration.name.clone(),
            // Upstream measures `String.length` (UTF-16 code units).
            cost: section.encode_utf16().count().div_ceil(CHARS_PER_TOKEN),
            section,
        });
    }
    let mut ordered: Vec<CatalogGroup> = std::mem::take(&mut groups);
    ordered.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => a.name.cmp(&b.name),
    });
    let shown = select_catalog(&ordered, options.inline_budget);

    let mut sections = vec![
        DESCRIPTION_INTRO.to_owned(),
        describe_globals(options.models),
    ];
    if declarations.iter().any(|declaration| {
        shown.contains(&declaration.name)
            && mcp_structured_content_schema(declaration.output_schema.as_ref()).is_some()
    }) {
        sections.push(format!(
            "Shared MCP Types:\n```ts\n{}\n```",
            rpi_codemode::MCP_TYPESCRIPT_PREAMBLE
        ));
    }
    if declarations.is_empty() {
        return sections.join("\n\n");
    }

    let mut tool_sections = vec!["Nested tools:".to_owned()];
    for group in &ordered {
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
            let description = namespace
                .description
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty());
            tool_sections.push(match description {
                Some(description) => format!("## {}{listing}\n{description}", namespace.name),
                None => format!("## {}{listing}", namespace.name),
            });
        }
        for entry in visible {
            tool_sections.push(entry.section.clone());
        }
    }
    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// What a script call resolves to, in one line (tool.ts:315-327).
pub fn describe_output(schema: Option<&serde_json::Value>) -> String {
    let rendered = render_tool_output_type(schema);
    if rendered == "string" {
        return "a string".to_owned();
    }
    let object = schema.and_then(serde_json::Value::as_object);
    if let Some(object) = object
        && object.get("type").and_then(serde_json::Value::as_str) == Some("object")
        && mcp_structured_content_schema(schema).is_none()
        && let Some(properties) = object
            .get("properties")
            .and_then(serde_json::Value::as_object)
    {
        let required: HashSet<&str> = object
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect()
            })
            .unwrap_or_default();
        let fields: Vec<String> = properties
            .keys()
            .map(|name| {
                if required.contains(name.as_str()) {
                    name.clone()
                } else {
                    format!("{name}?")
                }
            })
            .collect();
        return format!("`{{ {} }}`", fields.join(", "));
    }
    let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("`{normalized}`")
}

/// A declared tool's description followed by how scripts call it and what
/// the call resolves to (tool.ts:293-313).
pub fn describe_script_call(tool: &ToolInfo) -> String {
    let declaration = to_codemode_declaration(tool);
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        tool.description.trim(),
        to_codemode_identifier(&tool.name),
        describe_output(declaration.output_schema.as_ref())
    )
}

/// Re-exported so `tool.rs` can reference the shared cap.
pub const _DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = DEFAULT_INPUT_SCHEMA_MAX_CHARS;
