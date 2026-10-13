//! Tool selection names, patterns, and `+name`/`-name` modifiers.
//!
//! Port of `packages/coding-agent/src/core/settings-manager.ts`
//! (`isToolModifier` / `getToolListError` / `applyToolModifiers`) and
//! `packages/coding-agent/src/core/mcp-servers.ts`
//! (`toolPatternRegExp` / `createToolNameMatcher` / `isMcpToolName`) @
//! `abe508e1b` (v1.1.0, `04b97ef00`).

use std::collections::HashSet;

use serde_json::Value;

use crate::extensions::mcp::resources::{
    LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, READ_MCP_RESOURCE_TOOL,
};

/// `toolPatternRegExp` (mcp-servers.ts:11-17): `*` matches any characters,
/// everything else is literal. Matches the whole name (anchored).
///
/// The same algorithm backs the MCP `toolExposure` patterns; the previous
/// local copy in `extensions/mcp/config.rs` now delegates here.
pub fn tool_pattern_matches(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return pattern == value;
    };
    if !value.starts_with(first) {
        return false;
    }
    let mut rest = &value[first.len()..];
    let mut last = first;
    for part in parts {
        last = part;
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    value.ends_with(last)
}

/// `createToolNameMatcher` (mcp-servers.ts:20-24): whether a tool name
/// matches any of the entries, each an exact name or a `*` pattern.
#[derive(Debug, Clone, Default)]
pub struct ToolNameMatcher {
    names: HashSet<String>,
    patterns: Vec<String>,
}

impl ToolNameMatcher {
    pub fn new(entries: &[String]) -> Self {
        let mut names = HashSet::new();
        let mut patterns = Vec::new();
        for entry in entries {
            if entry.contains('*') {
                patterns.push(entry.clone());
            } else {
                names.insert(entry.clone());
            }
        }
        Self { names, patterns }
    }

    pub fn matches(&self, name: &str) -> bool {
        self.names.contains(name)
            || self
                .patterns
                .iter()
                .any(|pattern| tool_pattern_matches(pattern, name))
    }
}

/// MCP resource tools, which reach every server with resources
/// (`MCP_RESOURCE_TOOLS`, mcp-servers.ts:26-35).
const MCP_RESOURCE_TOOLS: [&str; 3] = [
    LIST_MCP_RESOURCES_TOOL,
    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
    READ_MCP_RESOURCE_TOOL,
];

/// `isMcpToolName` (mcp-servers.ts:38-40): a server tool
/// (`mcp__<server>__<tool>`) or a resource tool.
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__") || MCP_RESOURCE_TOOLS.contains(&name)
}

/// `isToolModifier` (settings-manager.ts:217-219): a `+name`/`-name` entry
/// that modifies a selection instead of replacing it.
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// `isToolModifier` for unvalidated settings JSON values.
pub fn is_tool_modifier_value(entry: &Value) -> bool {
    entry.as_str().is_some_and(is_tool_modifier)
}

/// `getToolListError` (settings-manager.ts:224-232): validate a tool list
/// from `--tools` or the SDK `tools` option. A list is either an allowlist
/// of plain names and patterns or a list of only `+name`/`-name` entries
/// with exact names. Returns the problem, or `None` when the list is valid.
pub fn get_tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers: Vec<&String> = entries
        .iter()
        .filter(|entry| is_tool_modifier(entry))
        .collect();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_owned());
    }
    let pattern = modifiers.iter().find(|entry| entry.contains('*'));
    pattern.map(|pattern| {
        format!("+name and -name entries take exact tool names, not patterns: {pattern}")
    })
}

/// `applyToolModifiers` (settings-manager.ts:237-248): apply the `+name`
/// and `-name` entries of `entries` to `base` in order; other entries are
/// ignored. `+name` adds a tool when missing, `-name` removes one when
/// present.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries {
        if !is_tool_modifier(entry) {
            continue;
        }
        let name = &entry[1..];
        let index = tools.iter().position(|tool| tool == name);
        if entry.starts_with('+') {
            if index.is_none() && !name.is_empty() {
                tools.push(name.to_owned());
            }
        } else if entry.starts_with('-')
            && let Some(index) = index
        {
            tools.remove(index);
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_owned()).collect()
    }

    #[test]
    fn pattern_matches_literal_and_star_forms() {
        assert!(tool_pattern_matches("read", "read"));
        assert!(!tool_pattern_matches("read", "readx"));
        assert!(tool_pattern_matches(
            "mcp__radius__*",
            "mcp__radius__search"
        ));
        assert!(tool_pattern_matches("*_tool", "dynamic_tool"));
        assert!(tool_pattern_matches("ask_*", "ask_question"));
        assert!(tool_pattern_matches("*", "anything"));
        assert!(!tool_pattern_matches("mcp__a__*", "mcp__b__x"));
    }

    #[test]
    fn matcher_handles_names_and_patterns() {
        let matcher = ToolNameMatcher::new(&owned(&["read", "*_tool", "mcp__radius__*"]));
        assert!(matcher.matches("read"));
        assert!(matcher.matches("dynamic_tool"));
        assert!(matcher.matches("mcp__radius__search"));
        assert!(!matcher.matches("write"));
    }

    #[test]
    fn mcp_tool_names_cover_server_and_resource_tools() {
        assert!(is_mcp_tool_name("mcp__radius__search"));
        assert!(is_mcp_tool_name("list_mcp_resources"));
        assert!(is_mcp_tool_name("list_mcp_resource_templates"));
        assert!(is_mcp_tool_name("read_mcp_resource"));
        assert!(!is_mcp_tool_name("read"));
    }

    #[test]
    fn tool_list_error_rejects_mixed_and_pattern_modifiers() {
        assert_eq!(get_tool_list_error(&owned(&["read", "bash"])), None);
        assert_eq!(
            get_tool_list_error(&owned(&["read", "+codemode"])),
            Some("tool names cannot be mixed with +name or -name entries".to_owned())
        );
        assert_eq!(
            get_tool_list_error(&owned(&["+mcp__radius__*"])),
            Some(
                "+name and -name entries take exact tool names, not patterns: +mcp__radius__*"
                    .to_owned()
            )
        );
        // Only modifiers is valid.
        assert_eq!(get_tool_list_error(&owned(&["+codemode", "-write"])), None);
    }

    #[test]
    fn modifiers_apply_in_order() {
        assert_eq!(
            apply_tool_modifiers(&owned(&["read", "bash"]), &owned(&["+codemode", "-write"])),
            owned(&["read", "bash", "codemode"])
        );
        assert_eq!(
            apply_tool_modifiers(&owned(&["read", "bash", "edit"]), &owned(&["-bash"])),
            owned(&["read", "edit"])
        );
        // +name only adds when missing; -name only removes when present.
        assert_eq!(
            apply_tool_modifiers(&owned(&["read"]), &owned(&["+read", "-missing"])),
            owned(&["read"])
        );
        // A bare `+` is a modifier with an empty name: no-op.
        assert_eq!(
            apply_tool_modifiers(&owned(&["read"]), &owned(&["+"])),
            owned(&["read"])
        );
    }
}
