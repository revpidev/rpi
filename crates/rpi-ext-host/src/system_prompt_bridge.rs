//! Self-contained system-prompt renderer for the `before_agent_start`
//! chaining bridge (#9548, runner.ts:1180 `renderCurrentSystemPrompt`).
//!
//! `rpi` owns the authoritative builder (`crates/rpi/src/core/system_prompt.rs`,
//! port of coding-agent `system-prompt.ts`); this crate cannot depend on it,
//! so the runner carries a compact mirror of `buildSystemPrompt` against the
//! options **JSON** shape (`BuildSystemPromptOptions` camelCase). The render
//! feeds only `ctx.getSystemPrompt()` / `event.systemPrompt` visibility for
//! chained handlers — the authoritative prompt still comes from the host's
//! own builder over the returned options.
//!
//! The algorithm mirrors upstream `buildSystemPrompt` +
//! `buildSystemPromptSections` section-for-section (preamble + tagged
//! sections joined by blank lines, custom sections replaced by name); one
//! documented gap remains versus the host builder: the `docs` section (rpi
//! bundled-doc paths live only in the host crate). Per-tool guideline
//! merging is NOT a gap — `build_rules` below consumes `toolGuidelines`
//! per selected tool, mirroring the host builder (#9548).
//! The gap only affects the chained visibility render, never the request.
//!
//! Merge note (runner boundary): handler-returned options merge key-level;
//! the three map fields (`sections`/`toolSnippets`/`toolGuidelines`) merge
//! per-key, but ARRAY fields (`selectedTools`/`promptGuidelines`/
//! `contextFiles`) only replace wholesale — upstream in-place mutations like
//! `promptGuidelines.push(...)` must be expressed as the full new array on
//! the rpi ABI (by-value JSON cannot carry a partial array mutation).

use serde_json::Value;

/// Render the current prompt text from the options JSON (camelCase
/// `BuildSystemPromptOptions`). Falls back to an empty string on a non-object
/// input (callers substitute the host-provided base render).
pub fn build_system_prompt(options: &Value) -> String {
    let Some(map) = options.as_object() else {
        return String::new();
    };
    if let Some(forced) = map.get("forceSystemPrompt").and_then(Value::as_str) {
        return forced.to_owned();
    }

    // Named sections in declaration order, mirroring the host builder's
    // `build_system_prompt_sections` (system-prompt.ts:121-183); `preamble`
    // stays untagged, every other section wraps in its tag below.
    let mut prompt_sections: Vec<(String, String)> = Vec::new();
    let custom_prompt = map
        .get("customPrompt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    match custom_prompt {
        Some(custom) => prompt_sections.push(("preamble".to_owned(), custom.to_owned())),
        None => {
            prompt_sections.push((
                "preamble".to_owned(),
                "You are an expert coding assistant operating inside rpi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files."
                    .to_owned(),
            ));
            prompt_sections.push((
                "tools".to_owned(),
                format!(
                    "{}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.",
                    tools_list(map)
                ),
            ));
            prompt_sections.push(("rules".to_owned(), build_rules(map)));
            // `docs` is rpi-specific (no bundled docs in the host crate's
            // view); the host-side builder renders it — omitting it here
            // only affects the chained visibility render.
        }
    }

    if let Some(append) = map
        .get("appendSystemPrompt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        prompt_sections.push(("addendum".to_owned(), append.to_owned()));
    }
    if let Some(files) = map.get("contextFiles").and_then(Value::as_array) {
        if !files.is_empty() {
            let mut blocks = vec!["Project-specific instructions and guidelines:".to_owned()];
            for file in files {
                let path = file.get("path").and_then(Value::as_str).unwrap_or("");
                let content = file.get("content").and_then(Value::as_str).unwrap_or("");
                blocks.push(format!(
                    "<project_instructions path=\"{path}\">\n{content}\n</project_instructions>"
                ));
            }
            prompt_sections.push(("project_context".to_owned(), blocks.join("\n\n")));
        }
    }
    // Skills: the host pre-formats to `skillsXml` (there is no `skills`
    // array on the rpi options JSON). Upstream gates the section on a
    // read/bash skill-file-read tool being selected (system-prompt.ts:160-167).
    let tools = selected_tools(map);
    let has_skill_read_tool = tools.iter().any(|tool| tool == "read" || tool == "bash");
    if has_skill_read_tool {
        if let Some(skills_xml) = map
            .get("skillsXml")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            prompt_sections.push(("skills".to_owned(), skills_xml.to_owned()));
        }
    }
    if let Some(cwd) = map.get("cwd").and_then(Value::as_str) {
        prompt_sections.push(("cwd".to_owned(), cwd.replace('\\', "/")));
    }
    // Custom sections: replace a built-in by name in place, otherwise append
    // (upstream object-literal semantics, system-prompt.ts:174-176). Invalid
    // names are skipped, matching the host builder's defensive filter.
    if let Some(sections) = map.get("sections").and_then(Value::as_object) {
        for (name, content) in sections {
            let Some(content) = content.as_str() else {
                continue;
            };
            if content.is_empty() || !is_valid_section_name(name) {
                continue;
            }
            match prompt_sections.iter_mut().find(|(n, _)| n == name) {
                Some(existing) => existing.1 = content.to_owned(),
                None => prompt_sections.push((name.clone(), content.to_owned())),
            }
        }
    }

    prompt_sections
        .into_iter()
        .map(|(name, content)| {
            if name == "preamble" {
                content
            } else {
                format!("<{name}>\n{content}\n</{name}>")
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `SYSTEM_PROMPT_SECTION_NAME` + the `preamble` rejection
/// (system-prompt.ts:52, mirrored from the host builder).
fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && name != "preamble"
}

fn selected_tools(map: &serde_json::Map<String, Value>) -> Vec<String> {
    // Missing or null falls back to the default four (`selectedTools ??
    // [...]`); an explicit array — even empty — is honored (upstream
    // normalize keeps `[]`, system-prompt.ts:58).
    match map.get("selectedTools") {
        Some(Value::Array(tools)) => tools
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => vec![
            "read".to_owned(),
            "bash".to_owned(),
            "edit".to_owned(),
            "write".to_owned(),
        ],
    }
}

fn tools_list(map: &serde_json::Map<String, Value>) -> String {
    let tools = selected_tools(map);
    let snippets = map.get("toolSnippets").and_then(Value::as_object);
    let visible: Vec<(String, String)> = tools
        .iter()
        .filter_map(|name| {
            let snippet = snippets?.get(name)?.as_str()?.to_owned();
            if snippet.is_empty() {
                return None;
            }
            Some((name.clone(), snippet))
        })
        .collect();
    if visible.is_empty() {
        "(none)".to_owned()
    } else {
        visible
            .iter()
            .map(|(name, snippet)| format!("- {name}: {snippet}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn build_rules(map: &serde_json::Map<String, Value>) -> String {
    let tools = selected_tools(map);
    let mut rules: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add_rule = |rule: &str| {
        let normalized = rule.trim();
        if !normalized.is_empty() && seen.insert(normalized.to_owned()) {
            rules.push(normalized.to_owned());
        }
    };

    let has = |name: &str| tools.iter().any(|tool| tool == name);
    let (has_bash, has_powershell) = (has("bash"), has("powershell"));
    if (has_bash || has_powershell) && !has("grep") && !has("find") && !has("ls") {
        if has_bash && has_powershell {
            add_rule("Use bash or PowerShell for file operations like listing, searching, and finding files");
        } else if has_powershell {
            add_rule(
                "Use PowerShell for file operations like listing, searching, and finding files",
            );
        } else {
            add_rule("Use bash for file operations like ls, rg, find");
        }
    }
    // Per-tool guidelines in selected-tool order (`toolGuidelines[name]`),
    // then the flat `promptGuidelines` extras (system-prompt.ts:111-114).
    let tool_guidelines = map.get("toolGuidelines").and_then(Value::as_object);
    for name in &tools {
        if let Some(lines) = tool_guidelines.and_then(|guidelines| guidelines.get(name)) {
            if let Some(lines) = lines.as_array() {
                for line in lines {
                    if let Some(line) = line.as_str() {
                        add_rule(line);
                    }
                }
            }
        }
    }
    if let Some(guidelines) = map.get("promptGuidelines").and_then(Value::as_array) {
        for guideline in guidelines {
            if let Some(guideline) = guideline.as_str() {
                add_rule(guideline);
            }
        }
    }
    add_rule("Be concise in your responses");
    add_rule("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_default_sections_tagged_with_preamble_raw() {
        let prompt = build_system_prompt(&json!({"cwd": "/w"}));
        assert!(prompt.starts_with("You are an expert coding assistant operating inside rpi,"));
        assert!(prompt.contains("<tools>\n(none)\n\nIn addition to the tools above"));
        assert!(prompt.contains("<rules>\n- Use bash for file operations like ls, rg, find"));
        assert!(prompt.ends_with("<cwd>\n/w\n</cwd>"));
        assert!(!prompt.contains("<preamble>"));
    }

    #[test]
    fn forced_prompt_is_opaque() {
        assert_eq!(
            build_system_prompt(&json!({"forceSystemPrompt": "forced", "cwd": "/w"})),
            "forced"
        );
    }

    #[test]
    fn custom_prompt_replaces_default_block() {
        let prompt = build_system_prompt(&json!({"customPrompt": "custom", "cwd": "/w"}));
        assert!(prompt.starts_with("custom\n\n<cwd>"));
        assert!(!prompt.contains("<tools>"));
    }

    #[test]
    fn skills_render_with_read_tool_and_trimmed_xml() {
        let prompt = build_system_prompt(&json!({
            "selectedTools": ["read"],
            "skillsXml": "  <skill>x</skill>  ",
            "cwd": "/w"
        }));
        assert!(prompt.contains("<skills>\n<skill>x</skill>\n</skills>"));
        // No read/bash tool selected → no skills section.
        let without = build_system_prompt(&json!({
            "selectedTools": ["edit"],
            "skillsXml": "<skill>x</skill>",
            "cwd": "/w"
        }));
        assert!(!without.contains("<skills>"));
    }

    #[test]
    fn custom_sections_append_replace_and_validate() {
        let prompt = build_system_prompt(&json!({
            "cwd": "/w",
            "sections": {
                "extra": "extra body",
                "rules": "custom rules",
                "preamble": "must be rejected",
                "Bad Name": "must be skipped"
            }
        }));
        // Appended after cwd, wrapped in its own tag.
        assert!(prompt.contains("<cwd>\n/w\n</cwd>\n\n<extra>\nextra body\n</extra>"));
        // Replaced in place (before addendum position, i.e. rules slot).
        assert!(prompt.contains("<rules>\ncustom rules\n</rules>"));
        assert!(!prompt.contains("Be concise"));
        // `preamble` and invalid names never land.
        assert!(!prompt.contains("must be rejected"));
        assert!(!prompt.contains("must be skipped"));
    }

    #[test]
    fn explicit_empty_selected_tools_stays_empty() {
        // `selectedTools: []` is honored (no default fallback): no bash
        // exploration rule and no skills gate.
        let prompt = build_system_prompt(&json!({
            "selectedTools": [],
            "skillsXml": "<skill>x</skill>",
            "cwd": "/w"
        }));
        assert!(prompt.contains("(none)"));
        assert!(!prompt.contains("Use bash for file operations"));
        assert!(!prompt.contains("<skills>"));
    }

    #[test]
    fn context_files_render_project_context() {
        let prompt = build_system_prompt(&json!({
            "cwd": "/w",
            "contextFiles": [{"path": "AGENTS.md", "content": "be nice"}]
        }));
        assert!(prompt.contains(
            "<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"AGENTS.md\">\nbe nice\n</project_instructions>\n</project_context>"
        ));
    }
}
