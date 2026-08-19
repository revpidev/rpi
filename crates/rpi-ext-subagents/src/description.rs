//! Tool description resolution (toolDescriptionMode full/compact/custom).
//!
//! Port of pi-subagents `src/extension/tool-description.ts` @ v0.48.0
//! (56f97234): mode resolution with warn-fallback, custom template loading
//! (project config dir then agent dir, 50 KiB cap, `{{placeholder}}`
//! substitution, unknown placeholders kept), mandatory deduplicated safety
//! section. The full/compact EXECUTION texts are rewritten for the structured
//! entry points per ADR-0016 (single run) and ADR-0018 (tasks/steps
//! composition with interpolation) — the byte-parity exemption is recorded
//! there.

use std::path::{Path, PathBuf};

use crate::config::ExtensionConfig;
use crate::paths;

const CUSTOM_TOOL_DESCRIPTION_FILE: &str = "subagent-tool-description.md";
const CUSTOM_TOOL_DESCRIPTION_MAX_BYTES: u64 = 50 * 1024;

pub const SUBAGENT_SAFETY_GUIDANCE: &str = "SAFETY-CRITICAL SUBAGENT GUIDANCE:
• Use { action: \"list\" } before execution and only run executable/non-disabled agents.
• Keep execution and management separate: omit action for delegation execution; use action only for management/control.
• Compositions (tasks/steps) run async by default; single runs follow asyncByDefault. Pass async:false only for a foreground blocking run, and pass a timeoutMs when a bounded run is needed.
• Ordinary child subagents are not orchestrators. Only explicitly configured fanout children may use the child-safe subagent tool, still bounded by depth/session limits.
• Keep one writer for the same cwd/worktree. Use fresh-context read-only reviewers for independent review, then have the parent synthesize and apply fixes.
• Runs write artifacts (input/output/transcript/meta) next to the session or under .rpi/subagents/artifacts; include output paths and residual risks when reporting results.";

pub const FULL_SUBAGENT_TOOL_DESCRIPTION: &str = "Delegate a task to a focused child subagent session; omit action. Use action only for management/control actions.

EXECUTION:
• Before executing, use { action: \"list\" } and run only executable/non-disabled configured agents.
• SINGLE RUN: { agent: \"scout\", task: \"...\" }. Pass the complete task text; the child has its own system prompt, tool allowlist and fresh session. Returns the final output plus details (runId, agent, exitCode, usage); async runs return a receipt immediately and deliver the result as a session message.
• PARALLEL WAVE: tasks: [{ key: \"correctness\", agent: \"reviewer\", task: \"...\" }, ...]. Children run concurrently bounded by concurrency (default 4); failures are isolated and the result aggregates every child's output in key order with per-child details.
• CHAIN: steps: [{ agent: \"scout\", task: \"...\", as: \"scan\" }, { agent: \"worker\", task: \"Implement from {outputs.scan}\" }]. Children run sequentially; task templates may interpolate {task} (original), {previous} (prior step output), {outputs.<name>} (a step bound with as) and {chain_dir}. A failed step stops the chain and completed steps are returned.
• Top-level async, concurrency, worktree, model, thinking, timeoutMs and budget fields are defaults children inherit unless they override them. async defaults to true for tasks/steps and to asyncByDefault for single runs.
• context is \"fresh\" (default; isolated session) or \"fork\" (branch of the current parent session; requires a persisted parent). timeoutMs (or maxRuntimeMs) bounds the run; foreground runs default to 30 minutes. output names an output file for the child; artifacts:true/false toggles the artifact trail; cwd relocates the child.
• Example: { agent: \"worker\", task: \"Implement X and run the tests\", context: \"fresh\", timeoutMs: 600000 }
• model overrides the agent's model for this run; agentScope (user|project|both) scopes discovery.

MANAGEMENT / CONTROL (use action; omit execution fields):
• list, get, status, interrupt, stop, steer, resume, refine, refine.show, refine.rollback, grant-spawn-budget, doctor. Use { action: \"list\" } to enumerate agents; { action: \"get\", agent: \"...\" } for full definition detail; { action: \"status\" } for active runs; { action: \"doctor\" } for configuration self-diagnostics.

";

pub const COMPACT_SUBAGENT_TOOL_DESCRIPTION: &str = "Delegate a task to a focused child subagent session; omit action. Use action only for management/control actions.

EXECUTE:
• Call { action: \"list\" } first and use only executable/non-disabled agents.
• SINGLE RUN { agent:\"scout\", task:\"...\" }. Complete task text; child has its own prompt/tools/session. Returns the final output with runId/exitCode/usage details.
• PARALLEL tasks:[{key, agent, task, ...}] — concurrent children (concurrency cap, default 4), isolated failures, results aggregated in key order.
• CHAIN steps:[{agent, task, as?}] — sequential children; task templates interpolate {task}, {previous}, {outputs.<name>} (bound with as), {chain_dir}; a failed step stops the chain.
• context fresh|fork; async default true for compositions, asyncByDefault for single runs; timeoutMs bounds the run (foreground default 30 minutes); output names an output file; artifacts toggles the trail; cwd relocates the child; model overrides per run; top-level fields act as child defaults.

MANAGE / CONTROL:
• Use action for list/get/status/interrupt/stop/steer/resume/refine/grant-spawn-budget/doctor. get takes agent; status lists active runs; doctor reports binary/config/discovery/directory diagnostics.

ASYNC / SAFETY:
• Ordinary children are not orchestrators. Keep one writer per cwd/worktree and use fresh read-only reviewers for independent checks.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDescriptionMode {
    Full,
    Compact,
    Custom,
}

/// `resolveToolDescriptionMode` (tool-description.ts:69-75).
pub fn resolve_tool_description_mode(config: &ExtensionConfig) -> ToolDescriptionMode {
    match config.tool_description_mode.as_deref() {
        None => ToolDescriptionMode::Full,
        Some("full") => ToolDescriptionMode::Full,
        Some("compact") => ToolDescriptionMode::Compact,
        Some("custom") => ToolDescriptionMode::Custom,
        Some(other) => {
            tracing::warn!(
                mode = other,
                "Ignoring invalid toolDescriptionMode; expected \"full\", \"compact\", or \"custom\"."
            );
            ToolDescriptionMode::Full
        }
    }
}

fn custom_description_paths(cwd: &Path) -> Vec<PathBuf> {
    vec![
        paths::get_project_config_dir(cwd).join(CUSTOM_TOOL_DESCRIPTION_FILE),
        paths::get_agent_dir().join(CUSTOM_TOOL_DESCRIPTION_FILE),
    ]
}

/// `renderCustomTemplate` (tool-description.ts:86-106).
fn render_custom_template(template: &str, cwd: &Path) -> String {
    let agent_dir = paths::get_agent_dir().to_string_lossy().to_string();
    let project_config_dir = paths::get_project_config_dir(cwd)
        .to_string_lossy()
        .to_string();
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = &after[..end];
                let replacement = match name {
                    "fullDescription" | "full" => Some(FULL_SUBAGENT_TOOL_DESCRIPTION.to_string()),
                    "compactDescription" | "compact" => {
                        Some(COMPACT_SUBAGENT_TOOL_DESCRIPTION.to_string())
                    }
                    "safetyGuidance" | "safety" => Some(SUBAGENT_SAFETY_GUIDANCE.to_string()),
                    "agentDir" => Some(agent_dir.clone()),
                    "projectConfigDir" => Some(project_config_dir.clone()),
                    _ => {
                        tracing::warn!("subagent-tool-description.md: unknown placeholder {{{{{name}}}}} left unchanged.");
                        None
                    }
                };
                match replacement {
                    Some(text) => out.push_str(&text),
                    None => out.push_str(&format!("{{{{{name}}}}}")),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `loadCustomToolDescription` (tool-description.ts:108-143).
fn load_custom_tool_description(cwd: &Path) -> Option<String> {
    for path in custom_description_paths(cwd) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            tracing::warn!(
                "Ignoring custom tool description '{}' because it is not a file.",
                path.to_string_lossy()
            );
            continue;
        }
        if meta.len() > CUSTOM_TOOL_DESCRIPTION_MAX_BYTES {
            tracing::warn!(
                "Ignoring custom tool description '{}' because it is larger than {} bytes.",
                path.to_string_lossy(),
                CUSTOM_TOOL_DESCRIPTION_MAX_BYTES
            );
            continue;
        }
        let Ok(template) = std::fs::read_to_string(&path) else {
            continue;
        };
        let template = template.trim();
        if template.is_empty() {
            tracing::warn!(
                "Ignoring empty custom tool description '{}'.",
                path.to_string_lossy()
            );
            continue;
        }
        let rendered = render_custom_template(template, cwd).trim().to_string();
        if rendered.is_empty() {
            tracing::warn!(
                "Ignoring custom tool description '{}' because it rendered empty.",
                path.to_string_lossy()
            );
            continue;
        }
        return Some(rendered);
    }
    None
}

/// `withMandatorySafetyGuidance` (tool-description.ts:145-154): split out any
/// embedded copies of the safety section, then append it exactly once.
fn with_mandatory_safety_guidance(description: &str) -> String {
    let parts: Vec<&str> = description
        .split(SUBAGENT_SAFETY_GUIDANCE)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        SUBAGENT_SAFETY_GUIDANCE.to_string()
    } else {
        format!("{}\n\n{}", parts.join("\n\n"), SUBAGENT_SAFETY_GUIDANCE)
    }
}

/// `buildSubagentToolDescription` (tool-description.ts:169-182). The
/// legacy-chain-control line stripping is moot for the rewritten texts (they
/// contain no legacy chain guidance).
pub fn build_subagent_tool_description(config: &ExtensionConfig, cwd: &Path) -> String {
    match resolve_tool_description_mode(config) {
        ToolDescriptionMode::Compact => {
            format!("{COMPACT_SUBAGENT_TOOL_DESCRIPTION}\n\n{SUBAGENT_SAFETY_GUIDANCE}")
        }
        ToolDescriptionMode::Custom => match load_custom_tool_description(cwd) {
            Some(custom) => with_mandatory_safety_guidance(&custom),
            None => {
                tracing::warn!("subagent-tool-description.md was not found or valid for toolDescriptionMode \"custom\"; using full description.");
                format!("{FULL_SUBAGENT_TOOL_DESCRIPTION}{SUBAGENT_SAFETY_GUIDANCE}")
            }
        },
        ToolDescriptionMode::Full => {
            format!("{FULL_SUBAGENT_TOOL_DESCRIPTION}{SUBAGENT_SAFETY_GUIDANCE}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_contains_safety_once_and_structure() {
        let config = ExtensionConfig::new();
        let description = build_subagent_tool_description(&config, Path::new("/repo"));
        assert!(description.contains("SINGLE RUN"));
        assert!(description.contains("PARALLEL WAVE"));
        assert!(description.contains("CHAIN"));
        assert!(description.contains("{outputs.<name>}"));
        assert!(description.contains("action: \"list\""));
        // ADR-0018 decision 5: the composition entry points are documented;
        // the stale "async arrives with P1" wording is gone.
        assert!(!description.contains("arrive with P1"));
        assert_eq!(
            description
                .matches("SAFETY-CRITICAL SUBAGENT GUIDANCE")
                .count(),
            1
        );
    }

    #[test]
    fn invalid_mode_warns_to_full() {
        let mut config = ExtensionConfig::new();
        config.tool_description_mode = Some("bogus".into());
        assert_eq!(
            resolve_tool_description_mode(&config),
            ToolDescriptionMode::Full
        );
        config.tool_description_mode = Some("compact".into());
        assert_eq!(
            resolve_tool_description_mode(&config),
            ToolDescriptionMode::Compact
        );
    }

    #[test]
    fn custom_template_dedupes_safety_and_keeps_unknown_placeholders() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-desc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_dir = dir.join(".rpi");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join(CUSTOM_TOOL_DESCRIPTION_FILE),
            "My custom text {{compact}} {{unknownVar}}\n\n{safety}",
        )
        .unwrap();
        let mut config = ExtensionConfig::new();
        config.tool_description_mode = Some("custom".into());
        let description = build_subagent_tool_description(&config, &dir);
        assert!(description.starts_with("My custom text"));
        assert!(
            description.contains("{{unknownVar}}"),
            "unknown placeholder kept"
        );
        assert!(description.contains("Delegate a task to a focused child"));
        assert_eq!(
            description
                .matches("SAFETY-CRITICAL SUBAGENT GUIDANCE")
                .count(),
            1,
            "embedded safety copy deduped, one appended"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
