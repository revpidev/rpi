//! Management actions: list / get / status / doctor (FR-P0-10).

use std::path::Path;

use serde_json::Value;

use crate::agents::discover::{self, AgentConfig};
use crate::config::SettingsPair;
use crate::paths;
use crate::tool::{ToolOutcome, FOREGROUND_RUN_MEMORY};

/// `handleList` text (agent-management.ts:753-788), P0 subset: no chains, no
/// restricted section (capability ceiling is P1), sorted by name.
pub fn format_agent_list(agents: &[AgentConfig]) -> String {
    let mut sorted: Vec<&AgentConfig> = agents.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut lines = vec!["Executable agents:".to_string()];
    if sorted.is_empty() {
        lines.push("- (none)".to_string());
    } else {
        for agent in sorted {
            let mut meta = agent.source_str().to_string();
            if let Some(context) = &agent.default_context {
                meta.push_str(&format!(", context: {}", context.as_str()));
            }
            if let Some(aliases) = &agent.aliases {
                if !aliases.is_empty() {
                    meta.push_str(&format!(", aliases: {}", aliases.join(", ")));
                }
            }
            lines.push(format!(
                "- {} ({}): {}",
                agent.name, meta, agent.description
            ));
        }
    }
    lines.push(String::new());
    lines.push("Chains:".to_string());
    lines.push("- (none)".to_string());
    lines.join("\n")
}

/// `formatAgentDetail` (agent-management.ts:665-701) for the P0 field set.
pub fn format_agent_detail(agent: &AgentConfig) -> String {
    let mut tools: Vec<String> = agent.tools.clone().unwrap_or_default();
    tools.extend(agent.mcp_direct_tools.iter().map(|t| format!("mcp:{t}")));
    let mut lines = vec![
        format!("Agent: {} ({})", agent.name, agent.source_str()),
        format!("Path: {}", agent.file_path.to_string_lossy()),
        format!("Description: {}", agent.description),
    ];
    if agent.package_name.is_some() {
        lines.push(format!("Local name: {}", agent.local_name));
        lines.push(format!(
            "Package: {}",
            agent.package_name.clone().unwrap_or_default()
        ));
    }
    if let Some(aliases) = &agent.aliases {
        if !aliases.is_empty() {
            lines.push(format!("Aliases: {}", aliases.join(", ")));
        }
    }
    if let Some(model) = &agent.model {
        lines.push(format!("Model: {model}"));
    }
    if !agent.fallback_models.is_empty() {
        lines.push(format!(
            "Fallback models: {}",
            agent.fallback_models.join(", ")
        ));
    }
    if !tools.is_empty() {
        lines.push(format!("Tools: {}", tools.join(", ")));
    }
    if !agent.skills.is_empty() {
        lines.push(format!("Skills: {}", agent.skills.join(", ")));
    }
    lines.push(format!("System prompt mode: {}", agent.system_prompt_mode));
    lines.push(format!(
        "Inherit project context: {}",
        if agent.inherit_project_context {
            "true"
        } else {
            "false"
        }
    ));
    lines.push(format!(
        "Inherit skills: {}",
        if agent.inherit_skills {
            "true"
        } else {
            "false"
        }
    ));
    if let Some(context) = &agent.default_context {
        lines.push(format!("Default context: {}", context.as_str()));
    }
    if let Some(async_default) = agent.default_async {
        lines.push(format!(
            "Async: {}",
            if async_default { "true" } else { "false" }
        ));
    }
    if let Some(timeout) = agent.default_timeout_ms {
        lines.push(format!("Timeout: {timeout}ms"));
    }
    if agent.source == discover::AgentSource::Builtin {
        lines.push(format!(
            "Disabled: {}",
            if agent.disabled.unwrap_or(false) {
                "true"
            } else {
                "false"
            }
        ));
    }
    if let Some(extensions) = &agent.extensions {
        lines.push(format!(
            "Extensions: {}",
            if extensions.is_empty() {
                "(none)".to_string()
            } else {
                extensions.join(", ")
            }
        ));
    }
    if let Some(subagent_only) = &agent.subagent_only_extensions {
        lines.push(format!(
            "Subagent-only extensions: {}",
            if subagent_only.is_empty() {
                "(none)".to_string()
            } else {
                subagent_only.join(", ")
            }
        ));
    }
    if let crate::agents::discover::ThinkingSpec::Level(level) = &agent.thinking {
        lines.push(format!("Thinking: {level}"));
    }
    if let Some(output) = &agent.output {
        lines.push(format!("Output: {output}"));
    }
    if !agent.default_reads.is_empty() {
        lines.push(format!("Reads: {}", agent.default_reads.join(", ")));
    }
    if agent.default_progress {
        lines.push("Progress: true".to_string());
    }
    if let Some(max_depth) = agent.max_subagent_depth {
        lines.push(format!("Max subagent depth: {max_depth}"));
    }
    if !agent.system_prompt.trim().is_empty() {
        lines.push(String::new());
        lines.push("System Prompt:".to_string());
        lines.push(agent.system_prompt.clone());
    }
    lines.join("\n")
}

/// P0 management dispatch. Returns the tool text plus optional details
/// additions (upstream `result()` carries details.mode).
pub fn handle_management_action(
    action: &str,
    agent_name: Option<&str>,
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
) -> ToolOutcome {
    match action {
        "list" => {
            let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
            ToolOutcome::text(format_agent_list(&agents))
        }
        "get" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"get\" requires an agent name.".to_string());
            };
            let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
            match discover::resolve_agent_name(&agents, name) {
                Ok(Some(agent)) => ToolOutcome::text(format_agent_detail(agent)),
                Ok(None) => ToolOutcome::error(format!("Unknown agent: {name}")),
                Err(message) => ToolOutcome::error(message),
            }
        }
        "status" => {
            // P0 has no async runs; foreground memory carries the last runs.
            let memory = FOREGROUND_RUN_MEMORY
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut lines = vec!["Foreground runs (this session):".to_string()];
            if memory.runs.is_empty() {
                lines.push("- (none)".to_string());
            }
            for run in memory.runs.iter().rev().take(10) {
                lines.push(format!(
                    "- {} agent={} exitCode={} durationMs={}{}",
                    run.run_id,
                    run.agent,
                    run.exit_code,
                    run.duration_ms,
                    run.error
                        .as_ref()
                        .map(|error| format!(" error={error}"))
                        .unwrap_or_default()
                ));
            }
            ToolOutcome::text(lines.join("\n"))
        }
        "doctor" => ToolOutcome::text(doctor_report(cwd, settings, config)),
        other => ToolOutcome::error(format!(
            "Unknown subagent action \"{other}\". Supported P0 actions: list, get, status, doctor."
        )),
    }
}

/// Doctor sections (extension/doctor.ts:229-270, P0 subset): Runtime,
/// Filesystem, Discovery, Depth/budget, Permission-system env parity.
fn doctor_report(
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
) -> String {
    let mut sections = Vec::new();

    let runtime_cwd = cwd.to_string_lossy();
    let configured_session_dir = config
        .default_session_dir
        .clone()
        .unwrap_or_else(|| "(derived from parent session)".to_string());
    sections.push(format!(
        "Runtime:\n- cwd: {runtime_cwd}\n- foreground execution: supported\n- async execution: P1\n- configured session dir: {configured_session_dir}"
    ));

    let temp_root = paths::temp_root_dir();
    let artifacts_temp = paths::temp_artifacts_dir();
    let mut filesystem = String::from("Filesystem:");
    for (label, dir) in [
        ("temp root", &temp_root),
        ("temp artifacts", &artifacts_temp),
    ] {
        let state = if dir.is_dir() {
            "ok"
        } else if dir.exists() {
            "failed"
        } else {
            "missing"
        };
        filesystem.push_str(&format!("\n- {label}: {state} ({})", dir.to_string_lossy()));
    }
    sections.push(filesystem);

    match discover::discover_agents(cwd, "both", settings, None) {
        Ok(agents) => {
            let by_source = |source: discover::AgentSource| {
                agents.iter().filter(|agent| agent.source == source).count()
            };
            sections.push(format!(
                "Discovery:\n- agents: {} (builtin {}, user {}, project {})\n- chains: 0",
                agents.len(),
                by_source(discover::AgentSource::Builtin),
                by_source(discover::AgentSource::User),
                by_source(discover::AgentSource::Project),
            ));
        }
        Err(error) => {
            sections.push(format!("Discovery:\n- failed: {error}"));
        }
    }

    let max_depth = crate::runner::budget::resolve_current_max_depth(
        config.max_subagent_depth.as_ref().and_then(Value::as_u64),
    );
    let max_spawns = crate::runner::budget::resolve_max_spawns_per_run(
        config
            .max_subagent_spawns_per_run
            .as_ref()
            .and_then(Value::as_u64),
    );
    let depth = crate::runner::budget::check_depth(None);
    sections.push(format!(
        "Depth / budget:\n- maxSubagentDepth: {max_depth} (env {} config {})\n- current depth: {}\n- maxSubagentSpawnsPerRun: {max_spawns}",
        if std::env::var_os(crate::runner::budget::SUBAGENT_MAX_DEPTH_ENV).is_some() { "set" } else { "unset" },
        if config.max_subagent_depth.is_some() { "set" } else { "unset" },
        depth.depth,
    ));

    let child_env = std::env::var(crate::launch::args::SUBAGENT_CHILD_ENV).ok();
    let parent_session = std::env::var(crate::launch::args::SUBAGENT_PARENT_SESSION_ENV).ok();
    sections.push(format!(
        "Subagent env:\n- RPI_SUBAGENT_CHILD: {}\n- RPI_SUBAGENT_PARENT_SESSION: {}",
        child_env.as_deref().unwrap_or("(unset)"),
        parent_session.as_deref().unwrap_or("(unset)"),
    ));

    sections.join("\n\n")
}

/// `/subagents` browse output (P0: static list; the upstream interactive
/// admin TUI is a P1/P2 surface, requirements §2.4 VARIANT).
pub fn format_subagents_browser(agents: &[AgentConfig]) -> String {
    let mut sorted: Vec<&AgentConfig> = agents.iter().collect();
    sorted.sort_by_key(|agent| std::cmp::Reverse(agent.source));
    let mut lines = vec!["Subagents (project > user > builtin):".to_string()];
    for agent in sorted {
        lines.push(format!(
            "{} [{}] · {} — {}",
            agent.name,
            agent.source_str(),
            agent
                .model
                .clone()
                .unwrap_or_else(|| "inherits session model".into()),
            agent.description
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_and_detail_format() {
        let agents = crate::agents::builtin::load_builtin_agents(None);
        let listing = format_agent_list(&agents);
        assert!(listing.starts_with("Executable agents:"));
        assert!(
            listing.contains("- oracle (builtin, context: fork, aliases: advisor):"),
            "listing was:\n{listing}"
        );
        assert!(listing.contains("Chains:"));
        let oracle = agents.iter().find(|a| a.name == "oracle").unwrap();
        let detail = format_agent_detail(oracle);
        assert!(detail.starts_with("Agent: oracle (builtin)"));
        assert!(detail.contains("Aliases: advisor"));
        assert!(detail.contains("Thinking: high"));
        assert!(detail.contains("Default context: fork"));
        assert!(detail.contains("System Prompt:"));
    }
}
