//! Management actions: list / get / status / doctor (FR-P0-10).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agents::discover::{self, AgentConfig};
use crate::config::SettingsPair;
use crate::paths;
use crate::tool::{FOREGROUND_RUN_MEMORY, ToolOutcome};

/// `handleList` text (agent-management.ts:753-788), P0 subset: no chains, no
/// restricted section (capability ceiling is P1), sorted by name.
/// List with a descendant-agent ceiling (#2338): non-allowlisted agents move
/// to the "Restricted agents" section (upstream list output marks them the
/// same way; `capability ceiling: <sources>` names the restricting source).
pub fn format_agent_list_with_ceiling(
    agents: &[AgentConfig],
    ceiling: Option<&crate::launch::args::DescendantAllowlist>,
) -> String {
    let mut sorted: Vec<&AgentConfig> = agents.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let (executable, restricted): (Vec<&AgentConfig>, Vec<&AgentConfig>) = sorted
        .into_iter()
        .partition(|agent| ceiling.is_none_or(|c| c.allows(&agent.name)));
    let mut lines = vec!["Executable agents:".to_string()];
    if executable.is_empty() {
        lines.push("- (none)".to_string());
    } else {
        for agent in executable {
            let mut meta = agent.source_str().to_string();
            if let Some(context) = &agent.default_context {
                meta.push_str(&format!(", context: {}", context.as_str()));
            }
            if let Some(aliases) = &agent.aliases
                && !aliases.is_empty()
            {
                meta.push_str(&format!(", aliases: {}", aliases.join(", ")));
            }
            lines.push(format!(
                "- {} ({}): {}",
                agent.name, meta, agent.description
            ));
        }
    }
    append_restricted_agent_lines(&mut lines, &restricted, ceiling);
    lines.push(String::new());
    lines.push("Chains:".to_string());
    lines.push("- (none)".to_string());
    lines.join(
        "
",
    )
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
    if let Some(aliases) = &agent.aliases
        && !aliases.is_empty()
    {
        lines.push(format!("Aliases: {}", aliases.join(", ")));
    }
    if let Some(model) = &agent.model {
        lines.push(format!("Model: {model}"));
    }
    if let Some(allowed) = &agent.allowed_agents {
        lines.push(format!(
            "Allowed agents: {}",
            if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            }
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
    if !agent.mutation_tools.is_empty() {
        lines.push(format!(
            "Mutation tools: {}",
            agent.mutation_tools.join(", ")
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

/// `appendRestrictedAgentLines` (agent-management.ts:857-864): the
/// restricted section shared by BOTH list modes (default lines and
/// capability lines) — the `; capability ceiling: <sources>` clause only
/// appears when the ceiling names its sources (upstream omits the clause
/// for an empty source list).
fn append_restricted_agent_lines(
    lines: &mut Vec<String>,
    restricted: &[&AgentConfig],
    ceiling: Option<&crate::launch::args::DescendantAllowlist>,
) {
    if restricted.is_empty() {
        return;
    }
    let sources = ceiling.map(|c| c.sources.join(", ")).unwrap_or_default();
    let clause = if sources.is_empty() {
        String::new()
    } else {
        format!("; capability ceiling: {sources}")
    };
    lines.push(String::new());
    lines.push(format!(
        "Restricted agents (not executable in this session{clause}):"
    ));
    for agent in restricted {
        lines.push(format!(
            "- {} ({}): {}",
            agent.name,
            agent.source_str(),
            agent.description
        ));
    }
}

/// `formatAgentCapabilitiesLine` (#1717, agent-management.ts:751-771 @
/// 0fc0eebb): one line per agent carrying the selection-relevant facts —
/// description preview, tool surface, model, thinking. The header reads
/// `Executable agents (capabilities):`. #2338: the session's capability
/// ceiling partitions non-allowlisted agents into their own restricted
/// section (both list modes share `appendRestrictedAgentLines` upstream).
pub fn format_agent_capabilities_list(
    agents: &[AgentConfig],
    ceiling: Option<&crate::launch::args::DescendantAllowlist>,
) -> String {
    let mut sorted: Vec<&AgentConfig> = agents.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let (sorted, restricted): (Vec<&AgentConfig>, Vec<&AgentConfig>) = sorted
        .into_iter()
        .partition(|agent| ceiling.is_none_or(|c| c.allows(&agent.name)));
    let mut lines = vec!["Executable agents (capabilities):".to_string()];
    if sorted.is_empty() {
        lines.push("- (none)".to_string());
    } else {
        for agent in sorted {
            let mut tools = "none".to_string();
            let mut declared = agent.tools.clone().unwrap_or_default();
            declared.extend(agent.mcp_direct_tools.iter().map(|t| format!("mcp:{t}")));
            if agent.tools.is_none() && agent.mcp_direct_tools.is_empty() {
                tools = "default/ambient".to_string();
            } else if !declared.is_empty() {
                tools = declared.join(", ");
            }
            if !agent.exclude_tools.is_empty() {
                tools = format!("{tools}; excludes: {}", agent.exclude_tools.join(", "));
            }
            let model = match &agent.model {
                Some(model) => model.clone(),
                None => "inherits current session".to_string(),
            };
            let thinking = match &agent.thinking {
                crate::agents::discover::ThinkingSpec::Level(level) => level.clone(),
                crate::agents::discover::ThinkingSpec::Disabled => "off".to_string(),
                crate::agents::discover::ThinkingSpec::Unset => "default".to_string(),
            };
            // #2213: report the acceptance policy in capability rows
            // (upstream `formatAcceptanceSummary`: an acceptance ROLE
            // reads `Acceptance role: <role>`).
            let acceptance = agent
                .acceptance_role
                .as_deref()
                .map(|role| format!("; Acceptance role: {role}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {} ({}): Description: {}; Tools: {}; Model: {}; Thinking: {}{acceptance}",
                agent.name,
                agent.source_str(),
                truncate_chars(&agent.description, 240),
                tools,
                model,
                thinking
            ));
        }
    }
    append_restricted_agent_lines(&mut lines, &restricted, ceiling);
    lines.join("\n")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let normalized: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed: String = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut cut: String = collapsed.chars().take(max_chars - 1).collect();
        cut.push('…');
        cut
    }
}

/// `agentCapabilitiesSnapshot` (#1720, agent-management.ts:842-851 +
/// `agentCapabilityRow` @ 0fc0eebb): structured per-agent records so callers
/// can select agents without parsing prose rows. #2338: the session's
/// capability ceiling partitions restricted agents into the tail of the
/// array with `executable: false` + `restrictionSources`, carries the real
/// `restrictedCount`, and names the ceiling's sources in
/// `capabilityCeilingSources` when present.
pub fn agent_capabilities_snapshot(
    agents: &[AgentConfig],
    ceiling: Option<&crate::launch::args::DescendantAllowlist>,
) -> Value {
    let mut sorted: Vec<&AgentConfig> = agents.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let (executable, restricted): (Vec<&AgentConfig>, Vec<&AgentConfig>) = sorted
        .into_iter()
        .partition(|agent| ceiling.is_none_or(|c| c.allows(&agent.name)));
    let capability_row = |agent: &AgentConfig, is_executable: bool| {
        let mut record = json!({
            "name": agent.name,
            "description": truncate_chars(&agent.description, 1000),
            "source": agent.source_str(),
            "executable": is_executable,
            "tools": {
                "tools": agent.tools,
                "mcpDirectTools": agent.mcp_direct_tools,
                "excludes": agent.exclude_tools,
            },
            // Upstream row shape: `model` carries value/thinking ONLY
            // (the #2338 `allowedAgents` field is not part of the
            // capability row — upstream destructures it away).
            "model": {
                "value": agent.model,
                "thinking": match &agent.thinking {
                    crate::agents::discover::ThinkingSpec::Level(level) => json!(level),
                    crate::agents::discover::ThinkingSpec::Disabled => json!("off"),
                    crate::agents::discover::ThinkingSpec::Unset => Value::Null,
                },
            },
            // `acceptance` mirrors the prose row's role fact (upstream
            // `presentDetails({ policy, role })`; rpi carries the role).
            "acceptance": {
                "role": agent.acceptance_role,
            },
            "execution": {
                "defaultAsync": agent.default_async,
                "timeoutMs": agent.default_timeout_ms,
            },
            "output": {
                "path": agent.output,
                "mode": agent.output_mode,
            },
            "extensions": {
                "names": agent.extensions,
                "subagentOnly": agent.subagent_only_extensions,
                "skills": agent.skills,
            },
        });
        // `restrictionSources` rides RESTRICTED rows ONLY (upstream
        // `agentCapabilityRow`: `executable ? undefined : sources ?? []` —
        // the key is OMITTED on executable rows, not null).
        if !is_executable {
            record["restrictionSources"] =
                json!(ceiling.map(|c| c.sources.clone()).unwrap_or_default());
        }
        if let Some(aliases) = &agent.aliases {
            record["aliases"] = json!(aliases);
        }
        record
    };
    let mut rows: Vec<Value> = executable
        .iter()
        .map(|agent| capability_row(agent, true))
        .collect();
    rows.extend(restricted.iter().map(|agent| capability_row(agent, false)));
    let mut snapshot = json!({
        "agents": rows,
        "restrictedCount": restricted.len(),
    });
    if let Some(sources) = ceiling.map(|c| c.sources.clone())
        && !sources.is_empty()
    {
        snapshot["capabilityCeilingSources"] = json!(sources);
    }
    snapshot
}

/// Extra dependencies the P1 control actions need (async registry access).
pub struct ActionDeps<'a> {
    pub host: Option<&'a dyn crate::HostContext>,
    pub runtime: Option<&'a crate::PluginRuntime>,
    /// Raw call params (for control actions needing ids/messages).
    pub params: Option<Value>,
}

/// `handleManagementAction` (agent-management.ts:1242 dispatch, P1 subset).
/// Mutating actions (create/update/delete/disable/enable/eject/reset) also
/// refresh the advertised-agent catalog: the `subagent` tool is
/// re-registered with a rebuilt description, and the host's `refreshTools`
/// fires from the registration (R7.1.11.3 rpi shape — no parent system
/// prompt injection).
pub fn handle_management_action_with(
    action: &str,
    agent_name: Option<&str>,
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    deps: &ActionDeps<'_>,
) -> ToolOutcome {
    let outcome = handle_management_action_inner(action, agent_name, cwd, settings, config, deps);
    if matches!(
        action,
        "create" | "update" | "delete" | "disable" | "enable" | "eject" | "reset"
    ) {
        refresh_advertised_catalog(deps.host, cwd, config);
    }
    outcome
}

/// Re-register the `subagent` tool so the advertised catalog (if any)
/// rebuilds; registration failure is logged and non-fatal (the action
/// itself already succeeded).
pub(crate) fn refresh_advertised_catalog(
    host: Option<&dyn crate::HostContext>,
    cwd: &Path,
    config: &crate::config::ExtensionConfig,
) {
    let Some(host) = host else {
        return;
    };
    let Some(calls) = host.async_calls() else {
        return;
    };
    let project_trusted = host.project_trusted() != Some(false);
    let provider = host.parent_model();
    let description = match crate::description::build_subagent_tool_description(
        config,
        cwd,
        project_trusted,
        provider
            .as_deref()
            .and_then(|model| model.split('/').next()),
    ) {
        Ok(description) => description,
        Err(error) => {
            tracing::warn!(%error, "advertised-catalog refresh: invalid settings");
            return;
        }
    };
    let response = crate::host_call_static(
        &calls,
        "registerTool",
        json!({
            "name": "subagent",
            "label": "Subagent",
            "description": description,
            "promptSnippet": "Delegate focused subtasks to child agent sessions (scout/researcher/worker/reviewer/oracle/delegate) or manage them via action.",
            "parameters": crate::tool::tool_parameters_schema(),
            "renderCall": true,
            "renderResult": true,
        }),
    );
    if response.get("error").is_some() {
        tracing::warn!("advertised-catalog refresh: registerTool rejected");
    }
}

const PROJECT_SCOPE_REQUIRES_TRUST: &str = "Project scope requires a trusted project; retry in a trusted session or use agentScope \"user\" or \"both\".";

/// `findBlockingAgentDiagnostic` subset (agent-management.ts): when a
/// named agent was skipped because its definition is invalid, `get` must
/// surface that parse/validation error instead of "Unknown agent".
fn blocking_agent_diagnostic<'a>(
    name: &str,
    diagnostics: &'a [discover::DiscoverDiagnostic],
) -> Option<&'a discover::DiscoverDiagnostic> {
    let requested = name.trim();
    let dashed = requested.replace(' ', "-");
    diagnostics.iter().find(|diagnostic| {
        diagnostic
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem == requested || stem == dashed)
    })
}

/// `normalizeListScope` (agent-management.ts): list/get accept
/// `user|project|both`; an explicit invalid value is a loud error, not a
/// silent fallback to `both`.
fn management_scope(params: &Value, action: &str) -> Result<&'static str, String> {
    match params.get("agentScope") {
        None => Ok("both"),
        Some(Value::String(scope)) => match scope.as_str() {
            "both" => Ok("both"),
            "user" => Ok("user"),
            "project" => Ok("project"),
            other => Err(format!(
                "agentScope must be 'user', 'project', or 'both' for {action}; got '{other}'."
            )),
        },
        Some(other) => Err(format!(
            "agentScope must be 'user', 'project', or 'both' for {action}; got {other}."
        )),
    }
}

/// Revive a remembered foreground run (#3025): the child session file is
/// the authoritative continuation point; the new attempt runs async so the
/// resume call returns immediately (upstream `executeAsyncSingle`).
fn resume_from_foreground_record(
    record: &Value,
    index: Option<usize>,
    task: Option<String>,
    host: &dyn crate::HostContext,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
) -> ToolOutcome {
    let run_id = record["runId"].as_str().unwrap_or("?");
    if record["children"]
        .as_array()
        .is_some_and(|children| children.iter().any(|child| child["status"] == "running"))
    {
        return ToolOutcome::error(format!(
            "Foreground run {run_id} is still running; resume applies to interrupted or completed runs."
        ));
    }
    let child = match crate::runner::foreground_history::child_for_resume(record, index) {
        Ok(child) => child,
        Err(message) => return ToolOutcome::error(message),
    };
    let agent = child["agent"].as_str().unwrap_or("worker").to_string();
    let session_file = child["sessionFile"]
        .as_str()
        .expect("child_for_resume requires sessionFile");
    let task =
        task.unwrap_or_else(|| "Continue the interrupted task from this session.".to_string());
    let mut params = serde_json::json!({
        "agent": agent,
        "task": task,
        "sessionFile": session_file,
        "async": true,
    });
    if let Some(cwd) = record["cwd"].as_str().filter(|cwd| !cwd.is_empty()) {
        params["cwd"] = json!(cwd);
    }
    if let Some(model) = child["model"].as_str().filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    if let Some(thinking) = child["thinking"]
        .as_str()
        .filter(|thinking| !thinking.is_empty())
    {
        params["thinking"] = json!(thinking);
    }
    let result = crate::tool::execute_subagent_tool(&params, host, settings, config, runtime, None);
    ToolOutcome {
        text: result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        details: result.get("details").cloned().unwrap_or(Value::Null),
        is_error: result["isError"].as_bool().unwrap_or(false),
    }
}

fn handle_management_action_inner(
    action: &str,
    agent_name: Option<&str>,
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    deps: &ActionDeps<'_>,
) -> ToolOutcome {
    let raw_params = deps.params.clone().unwrap_or(Value::Null);
    // #2805: a declined project trust hides project agents/settings and fails
    // project-scope management closed.
    let trusted = deps.host.and_then(|host| host.project_trusted()) != Some(false);
    // Model-controlled names reach filesystem joins (agent files, settings
    // keys) — reject path-shaped input up front (C1, upstream sanitizeName).
    if let Some(name) = agent_name
        && let Err(message) = crate::paths::ensure_safe_component(name, "Agent name")
    {
        return ToolOutcome::error(message);
    }
    match action {
        "list" => {
            let scope = match management_scope(&raw_params, action) {
                Ok(scope) => scope,
                Err(error) => return ToolOutcome::error(error),
            };
            if scope == "project" && !trusted {
                return ToolOutcome::error(PROJECT_SCOPE_REQUIRES_TRUST.to_string());
            }
            let (agents, diagnostics) = discover::discover_agents_with_diagnostics_trusted(
                cwd, scope, settings, None, trusted,
            );
            // #1717/#1720: `capabilities: true` switches to the compact
            // capability lines and attaches structured `details.agentCapabilities`
            // records (agent-management.ts:972-995 + agentCapabilityRow @
            // 0fc0eebb); the default listing shape is unchanged.
            let capability_mode = raw_params.get("capabilities") == Some(&Value::Bool(true));
            let session_ceiling = crate::launch::args::DescendantAllowlist::from_env();
            let mut text = if capability_mode {
                format_agent_capabilities_list(&agents, session_ceiling.as_ref())
            } else {
                format_agent_list_with_ceiling(&agents, session_ceiling.as_ref())
            };
            let diagnostic_lines = format_discovery_diagnostics(&diagnostics);
            if !diagnostic_lines.is_empty() {
                text.push('\n');
                text.push_str(&diagnostic_lines.join("\n"));
            }
            let mut details = Value::Null;
            if capability_mode {
                details = json!({
                    "mode": "management",
                    "results": [],
                    "agentCapabilities": agent_capabilities_snapshot(&agents, session_ceiling.as_ref()),
                });
            }
            ToolOutcome {
                text,
                details,
                is_error: false,
            }
        }
        "get" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"get\" requires an agent name.".to_string());
            };
            let scope = match management_scope(&raw_params, action) {
                Ok(scope) => scope,
                Err(error) => return ToolOutcome::error(error),
            };
            if scope == "project" && !trusted {
                return ToolOutcome::error(PROJECT_SCOPE_REQUIRES_TRUST.to_string());
            }
            let (agents, diagnostics) = discover::discover_agents_with_diagnostics_trusted(
                cwd, scope, settings, None, trusted,
            );
            match discover::resolve_agent_name(&agents, name) {
                Ok(Some(agent)) => ToolOutcome::text(format_agent_detail(agent)),
                Ok(None) => {
                    if let Some(diagnostic) = blocking_agent_diagnostic(name, &diagnostics) {
                        return ToolOutcome::error(format!(
                            "Agent '{name}' has invalid configuration: {}",
                            diagnostic.error
                        ));
                    }
                    ToolOutcome::error(format!("Unknown agent: {name}"))
                }
                Err(message) => ToolOutcome::error(message),
            }
        }
        "status" => {
            // Optional view selectors (#1963): `view:"transcript"` requires
            // an id and reads that run's live child transcript on demand;
            // the fleet view is [DEFER] (03 §2.13) so transcript is the only
            // valid view in rpi.
            let view = raw_params.get("view").and_then(Value::as_str);
            if let Some(view) = view {
                if view != "transcript" {
                    return ToolOutcome::error(format!(
                        "Unknown status view: {view}. Valid: transcript."
                    ));
                }
                let Some(id) = id_param(&raw_params) else {
                    return ToolOutcome::error(
                        "status view \"transcript\" requires the run id (use { id }).".to_string(),
                    );
                };
                let lines = raw_params
                    .get("lines")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(1, 500))
                    .unwrap_or(80) as usize;
                let index = raw_params
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize);
                return match format_status_transcript(&id, index, lines, cwd, settings, config) {
                    Ok(text) => ToolOutcome::text(text),
                    Err(message) => ToolOutcome::error(message),
                };
            }
            // Foreground memory + live async registry (async-status.ts shape).
            let current_session = deps
                .host
                .and_then(|host| host.parent_session(settings))
                .map(|session| session.id);
            let memory = FOREGROUND_RUN_MEMORY
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut lines = vec!["Foreground runs (this session):".to_string()];
            if memory.runs.is_empty() {
                lines.push("- (none)".to_string());
            }
            // #3025: foreground runs remembered from an earlier host process
            // (the in-memory list dies with the process). Read the persisted
            // records up front: an in-memory run must still show its resume
            // hint when its final record carries a session file.
            let session_id = deps
                .host
                .and_then(|host| host.parent_session(settings))
                .map(|session| session.id);
            let persisted =
                crate::runner::foreground_history::list_for_session(session_id.as_deref(), 50);
            let resumable_runs: Vec<&str> = persisted
                .iter()
                .filter(|record| {
                    record["children"].as_array().is_some_and(|children| {
                        children.iter().any(|child| {
                            child["sessionFile"].as_str().is_some_and(|path| {
                                path.ends_with(".jsonl") && Path::new(path).is_file()
                            })
                        })
                    })
                })
                .filter_map(|record| record["runId"].as_str())
                .collect();
            for run in memory.runs.iter().rev().take(10) {
                if let Some(session_id) = current_session.as_deref()
                    && run.session_id.as_deref() != Some(session_id)
                {
                    continue;
                }
                let resume = if resumable_runs.contains(&run.run_id.as_str()) {
                    format!(" · resume {}", run.run_id)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "- {} agent={} exitCode={} durationMs={}{}{resume}",
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
            for record in persisted.iter().take(10) {
                let run_id = record["runId"].as_str().unwrap_or("?");
                if memory.runs.iter().any(|run| run.run_id == run_id) {
                    continue;
                }
                let child = &record["children"][0];
                let state = child["status"].as_str().unwrap_or("?");
                let resume = if resumable_runs.contains(&run_id) {
                    format!(" · resume {run_id}")
                } else {
                    String::new()
                };
                lines.push(format!(
                    "- {} agent={} state={}{}{}",
                    run_id,
                    child["agent"].as_str().unwrap_or("?"),
                    state,
                    child["error"]
                        .as_str()
                        .map(|error| format!(" error={error}"))
                        .unwrap_or_default(),
                    resume
                ));
            }
            lines.push(String::new());
            lines.push("Background runs:".to_string());
            let async_runs = crate::runner::background::list_runs();
            if async_runs.is_empty() {
                lines.push("- (none)".to_string());
            }
            for run in &async_runs {
                lines.push(format!(
                    "- {} mode={} state={}{}",
                    run["runId"].as_str().unwrap_or("?"),
                    run["mode"].as_str().unwrap_or("?"),
                    run["state"].as_str().unwrap_or("?"),
                    run["error"]
                        .as_str()
                        .map(|error| format!(" error={error}"))
                        .unwrap_or_default()
                ));
            }
            ToolOutcome::text(lines.join("\n"))
        }
        // Lifecycle diagnostics for one async run (#1037): run/dir/state/
        // process-terminal + a bounded events summary — no prompts, secrets
        // or transcripts.
        "debug.run" => {
            let Some(id) = id_param(&raw_params) else {
                return ToolOutcome::error(
                    "action \"debug.run\" requires the run id (use { id }).".to_string(),
                );
            };
            match format_run_lifecycle_debug(&id) {
                Ok(text) => ToolOutcome::text(text),
                Err(message) => ToolOutcome::error(message),
            }
        }
        // Async control actions (async-stop-action.ts / control-channel.ts).
        "interrupt" => match id_param(&raw_params) {
            Some(id) => match crate::runner::background::interrupt_run(&id) {
                Ok(status) => ToolOutcome::text(format_async_status("interrupted", &status)),
                Err(message) => ToolOutcome::error(message),
            },
            None => ToolOutcome::error(
                "action \"interrupt\" requires the background run id (use { id }).".to_string(),
            ),
        },
        "stop" => match id_param(&raw_params) {
            Some(id) => {
                // Child-scoped stop (#1367/#1603): `childId` resolves one
                // child of a multi-child run; malformed ids are rejected —
                // never widened to a run-level stop. Identity candidates
                // per step: `step.runId` (when present) then `step:<index>`
                // (child-identity.ts:26-28 subset).
                let child_id = raw_params
                    .get("childId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match crate::runner::background::find_run(&id) {
                    Some(handle) => {
                        // Terminal runs stay registered (wait/status read
                        // them); stopping one is invalid_state (#1965) —
                        // report instead of queueing or silently succeeding.
                        // Checked BEFORE the runtime dependency: a terminal
                        // run cannot be stopped regardless of context.
                        let snapshot = crate::runner::background::status_snapshot(&handle);
                        let state = snapshot["state"].as_str().unwrap_or_default();
                        if matches!(
                            state,
                            "complete" | "failed" | "stopped" | "paused" | "rejected"
                        ) {
                            return ToolOutcome::error(format!(
                                "invalid_state: Background run {} is {state}; stop only supports running async runs.",
                                handle.run_id
                            ));
                        }
                        // Child-scoped stop resolves + validates BEFORE the
                        // runtime dependency: the rejections are pure status
                        // decisions, and the child stop itself only signals
                        // (no bounded wait).
                        let steps: Vec<Value> =
                            snapshot["steps"].as_array().cloned().unwrap_or_default();
                        if let Some(child_id) = &child_id {
                            let mut matches: Vec<usize> = Vec::new();
                            for (index, step) in steps.iter().enumerate() {
                                let step_marker = format!("step:{index}");
                                let candidates =
                                    [step["runId"].as_str(), Some(step_marker.as_str())];
                                if candidates.contains(&Some(child_id.as_str())) {
                                    matches.push(index);
                                }
                            }
                            let Some(&index) = matches.first() else {
                                return ToolOutcome::error(format!(
                                    "Child '{child_id}' was not found under background run '{}'.",
                                    handle.run_id
                                ));
                            };
                            if matches.len() > 1 {
                                return ToolOutcome::error(format!(
                                    "Child '{child_id}' is ambiguous under background run '{}'.",
                                    handle.run_id
                                ));
                            }
                            let step_state = steps[index]["status"].as_str().unwrap_or("unknown");
                            if !matches!(step_state, "pending" | "running" | "queued") {
                                return ToolOutcome::error(format!(
                                    "invalid_state: Child '{child_id}' in background run '{}' is {step_state}; stop only supports pending or running children.",
                                    handle.run_id
                                ));
                            }
                            crate::runner::foreground::request_stop_for_run_child(
                                &handle.run_id,
                                index as u32,
                            );
                            crate::artifacts::append_jsonl(
                                &handle.run_dir.join("events.jsonl"),
                                &serde_json::json!({
                                    "type": "control.stop",
                                    "runId": handle.run_id,
                                    "targetIndex": index,
                                    "childId": child_id,
                                })
                                .to_string(),
                            );
                            return ToolOutcome::text(format!(
                                "Stop requested for child {child_id} in background run {}.",
                                handle.run_id
                            ));
                        }
                        // Cooperative stop + direct child signalling; the
                        // bounded terminal wait happens on the plugin runtime.
                        let (Some(runtime), Some(_host)) = (deps.runtime, deps.host) else {
                            return ToolOutcome::error(
                                "action \"stop\" is unavailable in this context.".to_string(),
                            );
                        };
                        handle.control.request_stop();
                        crate::runner::foreground::request_stop_for_run(&handle.run_id);
                        let run_dir = handle.run_dir.clone();
                        crate::artifacts::append_jsonl(
                            &run_dir.join("events.jsonl"),
                            &serde_json::json!({
                                "type": "control.stop",
                                "runId": handle.run_id,
                            })
                            .to_string(),
                        );
                        let snapshot = runtime.block_on(async {
                            match crate::runner::background::wait_for_runs(
                                Some(handle.run_id.as_str()),
                                false,
                                15_000,
                                None,
                                None,
                            )
                            .await
                            {
                                Ok(waited) => waited["runs"]
                                    .as_array()
                                    .and_then(|runs| runs.first().cloned()),
                                Err(_) => None,
                            }
                        });
                        match snapshot {
                            Some(status) => {
                                ToolOutcome::text(format_async_status("stopped", &status))
                            }
                            None => ToolOutcome::text(format!(
                                "Stop requested for background run {}.",
                                handle.run_id
                            )),
                        }
                    }
                    None => ToolOutcome::error(format!("No active background run matches '{id}'.")),
                }
            }
            None => ToolOutcome::error(
                "action \"stop\" requires the background run id (use { id }).".to_string(),
            ),
        },
        "steer" => {
            let id = id_param(&raw_params).unwrap_or_default();
            let message = raw_params
                .get("message")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);
            let mode = raw_params
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("steer")
                .to_string();
            let target_index = raw_params
                .get("targetIndex")
                .or_else(|| raw_params.get("index"))
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let Some(message) = message else {
                return ToolOutcome::error(
                    "action \"steer\" requires a non-empty message.".to_string(),
                );
            };
            match crate::runner::background::deliver_steer(&id, &message, &mode, target_index) {
                Ok(request) => ToolOutcome::text(format!(
                    "Steer request {} delivered to child {} of run {}.",
                    request["id"].as_str().unwrap_or("?"),
                    request["targetIndex"].as_u64().unwrap_or(0),
                    id
                )),
                Err(message) => ToolOutcome::error(message),
            }
        }
        "resume" => {
            // Revive from the persisted child session (async-resume.ts):
            // requires a terminal/paused run and a continuation task.
            let (Some(id), Some(host), Some(runtime)) =
                (id_param(&raw_params), deps.host, deps.runtime)
            else {
                return ToolOutcome::error(
                    "action \"resume\" requires the run id and an active session context."
                        .to_string(),
                );
            };
            let task = raw_params
                .get("task")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);
            let current_session = host.parent_session(settings).map(|session| session.id);
            let resume_index = raw_params.get("index");
            match crate::runner::foreground_history::find_prefix_for_session(
                &id,
                current_session.as_deref(),
            ) {
                Ok(Some(record)) => {
                    let resume_index = match resume_index {
                        None => None,
                        Some(value) => match value.as_i64() {
                            Some(value) if value >= 0 => Some(value as usize),
                            Some(value) => {
                                return ToolOutcome::error(format!(
                                    "Foreground run '{}' has {} children. Index {value} is out of range.",
                                    record["runId"].as_str().unwrap_or(&id),
                                    record["children"].as_array().map(Vec::len).unwrap_or(0)
                                ));
                            }
                            None => {
                                return ToolOutcome::error(format!(
                                    "Foreground run '{}' index must be an integer.",
                                    record["runId"].as_str().unwrap_or(&id)
                                ));
                            }
                        },
                    };
                    return resume_from_foreground_record(
                        &record,
                        resume_index,
                        task,
                        host,
                        settings,
                        config,
                        runtime,
                    );
                }
                Ok(None) => {}
                Err(message) => return ToolOutcome::error(message),
            }
            let status = match crate::runner::background::read_run_status(&id) {
                Ok(Some(status)) => status,
                Ok(None) => {
                    return ToolOutcome::error(format!("No background run matches '{id}'."));
                }
                Err(message) => return ToolOutcome::error(message),
            };
            // async-resume.ts:487-489: a resume may only revive a run owned by
            // the active parent session. Without this check any run id could
            // pull another session's child context into this one.
            if let Some(current) = current_session.as_deref()
                && let Some(owner) = status["sessionId"].as_str()
                && owner != current
            {
                return ToolOutcome::error(format!(
                    "Async run '{id}' was not found in the active session."
                ));
            }
            if matches!(status["state"].as_str(), Some("running") | Some("queued")) {
                return ToolOutcome::error(format!(
                    "Background run {id} is still {state}; resume applies to interrupted or stopped runs.",
                    state = status["state"].as_str().unwrap_or("?")
                ));
            }
            let steps = status["steps"].as_array().cloned().unwrap_or_default();
            // TE38 W5 (#2242 @ b72714de): a stopped child has no continuation
            // to revive — resume must not resurrect it (`resolveAsyncResumeTarget`
            // rejects `status == "stopped" || stopped == true` for the selected
            // step; rpi resumes the last step, so the gate lands there).
            if let Some(last) = steps.last()
                && (last["status"].as_str() == Some("stopped")
                    || last["stopped"].as_bool() == Some(true))
            {
                return ToolOutcome::error(format!(
                    "Async run '{id}' child {} was stopped and cannot be resumed. Start a new run instead.",
                    steps.len().saturating_sub(1)
                ));
            }
            let session_file = steps
                .last()
                .and_then(|step| step["sessionFile"].as_str())
                .map(str::to_string);
            let Some(session_file) = session_file else {
                return ToolOutcome::error(format!(
                    "Background run {id} has no persisted child session to resume from."
                ));
            };
            if !session_file.ends_with(".jsonl") {
                return ToolOutcome::error(format!(
                    "Async run '{id}' session file must be a .jsonl file: {session_file}"
                ));
            }
            if !Path::new(&session_file).is_file() {
                return ToolOutcome::error(format!(
                    "Async run '{id}' session file does not exist: {session_file}"
                ));
            }
            let agent_name = steps
                .last()
                .and_then(|step| step["agent"].as_str())
                .unwrap_or("worker")
                .to_string();
            let task = task
                .unwrap_or_else(|| "Continue the interrupted task from this session.".to_string());
            // New async run reviving the old session file.
            let params = serde_json::json!({
                "agent": agent_name,
                "task": task,
                "sessionFile": session_file,
                "async": true,
            });
            let host_cwd = host.cwd();
            let settings = match crate::config::read_settings_pair_trusted(
                &host_cwd,
                host.project_trusted() != Some(false),
            ) {
                Ok(settings) => settings.with_provider(
                    host.parent_model()
                        .as_deref()
                        .and_then(|model| model.split('/').next()),
                ),
                Err(error) => {
                    return ToolOutcome::error(format!("Subagent settings error: {error}"));
                }
            };
            let config = crate::config::load_config();
            let result = crate::tool::execute_subagent_tool(
                &params, host, &settings, &config, runtime, None,
            );
            ToolOutcome {
                text: result["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                details: result.get("details").cloned().unwrap_or(Value::Null),
                is_error: result["isError"].as_bool().unwrap_or(false),
            }
        }
        "grant-spawn-budget" => {
            let additional = raw_params
                .get("additional")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // Root interactive parent only (upstream
            // subagent-executor.ts:5241-5260 + schemas.ts:285: "Root
            // interactive parent with native user confirmation only").
            let Some(host) = deps.host else {
                return ToolOutcome::error(
                    "action \"grant-spawn-budget\" is available only from the root interactive parent session."
                        .to_string(),
                );
            };
            if !host.has_ui() {
                return ToolOutcome::error(
                    "action \"grant-spawn-budget\" is available only from the root interactive parent session (ctx.hasUI is false here)."
                        .to_string(),
                );
            }
            // The session id is derived from the host's authoritative parent
            // session (V13-02, `ctx.sessionFile`; TE-D16-derived id before
            // the accessor) — never trusted from the call parameters, so a
            // grant cannot target another session's ledger.
            let Some(session_id) = host
                .parent_session(settings)
                .map(|session| session.id)
                .filter(|s| !s.is_empty())
            else {
                return ToolOutcome::error(
                    "action \"grant-spawn-budget\" requires an active interactive session."
                        .to_string(),
                );
            };
            let ledger = crate::runner::background::SpawnBudgetLedger::open(&session_id);
            match ledger.grant(additional, config.max_subagent_spawns_per_session()) {
                Ok(granted) => ToolOutcome::text(format!(
                    "Spawn budget granted: cumulative grant is now {granted}."
                )),
                Err(message) => ToolOutcome::error(message),
            }
        }
        // Mutating management set (agent-management.ts:908-1240).
        "create" | "update" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error(format!("action \"{action}\" requires an agent name."));
            };
            let scope = action_scope(&raw_params);
            let config_body = raw_params.get("config").cloned().unwrap_or(Value::Null);
            manage_create_update(action, name, &config_body, cwd, settings, scope, trusted)
        }
        "delete" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"delete\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_delete(name, cwd, settings, scope, trusted)
        }
        "eject" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"eject\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_eject(name, cwd, settings, scope, trusted)
        }
        "disable" | "enable" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error(format!("action \"{action}\" requires an agent name."));
            };
            let scope = action_scope(&raw_params);
            manage_disable_enable(action == "disable", name, cwd, settings, scope, trusted)
        }
        "reset" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"reset\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_reset(name, cwd, settings, scope, trusted)
        }
        // Refinement overlay (agent-refinements.ts, FR-P1-08).
        "refine" | "refine.show" | "refine.rollback" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error(format!("action \"{action}\" requires an agent name."));
            };
            manage_refine(action, name, &raw_params, cwd, settings, trusted)
        }
        "doctor" => ToolOutcome::text(doctor_report(cwd, settings, config, trusted)),
        other => ToolOutcome::error(format!(
            "Unknown subagent action \"{other}\". Supported actions: list, get, status, interrupt, stop, steer, resume, refine, refine.show, refine.rollback, grant-spawn-budget, debug.run, doctor."
        )),
    }
}

/// `formatAsyncRunTranscript` subset (#1963, run-status.ts + fleet-view.ts
/// @ 0fc0eebb): bounded transcript tail for one child of a run. Source order
/// is session file (structured records with `toolCall.arguments`) → live
/// transcript artifact → saved output. The session file is preferred for a
/// completed child because that is the only source that carries the tool-call
/// input preview R18.9.5 requires; a live child without a session file falls
/// back to the transcript artifact stamped at launch. `lines` bounds the tail
/// (default 80, cap 500, fleet-view.ts `transcriptLineLimit`); the rendered
/// body is additionally byte-bounded (#2017 @ b72714de:
/// `MAX_TRANSCRIPT_BODY_BYTES` 32 KiB, whole recent lines preferred, "…
/// [earlier lines omitted]" marker); each rendered line is capped so the body
/// can never explode the tool result.
///
/// Every path comes from the run's status document, so it is validated the
/// same way upstream `readContainedTextTail` / `validateTranscriptPath` do:
/// trusted roots, no symlink, regular file only, realpath still inside the
/// roots. A refused path is a warning; it is never read.
pub fn format_status_transcript(
    id: &str,
    index: Option<usize>,
    line_limit: usize,
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
) -> Result<String, String> {
    let status = crate::runner::background::read_run_status(id)?
        .ok_or_else(|| format!("No background run matches '{id}'."))?;
    let run_id = status["runId"].as_str().unwrap_or("?").to_string();
    let steps: Vec<Value> = status["steps"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() {
        return Err(format!(
            "Background run {run_id} has no children to inspect."
        ));
    }
    let index = index.unwrap_or(0);
    let Some(step) = steps.get(index) else {
        return Err(format!(
            "Transcript index {index} is out of range for {} children.",
            steps.len()
        ));
    };
    let agent = step["agent"].as_str().unwrap_or("subagent");
    let roots = status_transcript_roots(cwd, settings, config);
    let mut lines = vec![
        format!("Run: {run_id}"),
        format!("State: {}", status["state"].as_str().unwrap_or("?")),
        format!("Mode: {}", status["mode"].as_str().unwrap_or("single")),
        format!("Child: {index} ({agent})"),
    ];
    let transcript_path = step["transcriptPath"]
        .as_str()
        .or_else(|| step["artifactPaths"]["transcriptPath"].as_str())
        .map(str::to_string);
    let output_path = step["artifactPaths"]["outputPath"]
        .as_str()
        .or_else(|| step["savedOutputPath"].as_str())
        .map(str::to_string);
    let session_file = step["sessionFile"].as_str().map(str::to_string);
    if let Some(path) = &transcript_path {
        lines.push(format!("Transcript: {path}"));
    }
    if let Some(path) = &output_path {
        lines.push(format!("Output: {path}"));
    }
    if let Some(path) = &session_file {
        lines.push(format!("Session: {path}"));
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut body: Vec<String> = Vec::new();
    let mut source = "Transcript tail".to_string();

    // 1. Session file first: structured records carry `toolCall.arguments`.
    if body.is_empty()
        && let Some(path) = session_file.as_deref()
    {
        match read_session_transcript_tail(path, line_limit, &roots) {
            Ok(Some(tail)) => {
                source = format!("Session transcript tail from {path}");
                body = tail;
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!("Session warning: {error}")),
        }
    }

    // 2. Live transcript artifact (stdout/stderr interleave).
    if body.is_empty()
        && let Some(path) = transcript_path.as_deref()
    {
        match read_transcript_artifact_tail(path, line_limit, &roots) {
            Ok(Some((tail, truncated, malformed))) => {
                source = if truncated {
                    format!("Transcript tail from {path} (tail truncated)")
                } else {
                    format!("Transcript tail from {path}")
                };
                body = tail;
                if malformed > 0 {
                    warnings.push(format!(
                        "Transcript warning: skipped {malformed} malformed record{}",
                        if malformed == 1 { "" } else { "s" }
                    ));
                }
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!("Transcript warning: {error}")),
        }
    }

    // 3. Plain saved output.
    if body.is_empty()
        && let Some(path) = output_path.as_deref()
    {
        match read_trusted_tail(path, &roots, TRANSCRIPT_TAIL_BYTES) {
            Ok(Some(raw)) => {
                let all: Vec<&str> = raw.lines().collect();
                let start = all.len().saturating_sub(line_limit);
                body = all[start..]
                    .iter()
                    .map(|line| bounded_transcript_line(line))
                    .collect();
                source = if start > 0 {
                    format!("Output tail from {path} (tail truncated)")
                } else {
                    format!("Output tail from {path}")
                };
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!("Output warning: {error}")),
        }
    }

    // #2017 body byte budget (TE38 W6): applies to whichever source won.
    bound_transcript_body(&mut body);
    if !warnings.is_empty() {
        lines.extend(warnings);
    }
    if body.is_empty() {
        lines.push(format!(
            "No transcript available for child {index} yet (the child may not have started or produced output)."
        ));
    } else {
        lines.push(String::new());
        lines.push(source);
        lines.extend(body);
    }
    Ok(lines.join("\n"))
}

/// Upper bound on each transcript source read (upstream `TRANSCRIPT_TAIL_BYTES`).
const TRANSCRIPT_TAIL_BYTES: u64 = 1024 * 1024;

/// Trusted roots for a background run's status transcript. They are derived
/// from the current host cwd/settings, never from the paths being validated.
fn status_transcript_roots(
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
) -> Vec<PathBuf> {
    let mut roots = vec![
        paths::get_project_artifacts_dir(cwd),
        paths::temp_artifacts_dir(),
        paths::resolve_parent_session_dir(cwd, settings.session_dir.as_deref()),
        paths::get_agent_dir().join("sessions"),
        paths::temp_root_dir(),
        crate::runner::background::async_runs_dir(),
    ];
    if let Some(dir) = settings
        .session_dir
        .as_deref()
        .filter(|dir| !dir.trim().is_empty())
    {
        push_configured_root(&mut roots, dir);
    }
    if let Some(dir) = config
        .default_session_dir
        .as_deref()
        .filter(|dir| !dir.trim().is_empty())
    {
        push_configured_root(&mut roots, dir);
    }
    // Canonicalize every root before containment is ever checked: a configured
    // `sessionDir` symlink to `/` (or a canonical root equal to `/`) would
    // otherwise pass the lexical check and turn realpath containment into a
    // no-op. Roots that are themselves filesystem roots are dropped.
    roots = roots
        .into_iter()
        .filter_map(|root| {
            let root = std::fs::canonicalize(&root).unwrap_or(root);
            root.parent().is_some().then_some(root)
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Add a configured session root only when it is a real directory scope. A
/// trusted project setting `sessionDir: "/"` would otherwise turn the
/// containment check into a no-op.
fn push_configured_root(roots: &mut Vec<PathBuf>, dir: &str) {
    let root = paths::expand_tilde_and_resolve(dir);
    // Canonicalize so a configured symlink to `/` cannot turn the containment
    // check into a no-op; the final retain also drops a literal `/`.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    if root.parent().is_some() {
        roots.push(root);
    }
}

/// `readContainedTextTail` (fleet-view.ts:138-183): resolve a status-provided
/// path against trusted roots, refuse symlinks/non-files, and require the
/// canonical path to stay inside a canonical root. `Ok(None)` = missing file
/// (silent, matching upstream); `Err` = refused/unreadable (rendered warning).
fn read_trusted_tail(
    path: &str,
    roots: &[PathBuf],
    max_bytes: u64,
) -> Result<Option<String>, String> {
    if roots.is_empty() {
        return Err(format!(
            "refusing to read transcript path without a trusted root: {path}"
        ));
    }
    let candidate = Path::new(path);
    if !candidate.is_absolute() {
        return Err(format!("refusing a non-absolute transcript path: {path}"));
    }
    if !roots.iter().any(|root| candidate.starts_with(root)) {
        return Err(format!(
            "refusing a transcript path outside trusted roots: {path}"
        ));
    }
    let metadata = match std::fs::symlink_metadata(candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "transcript path could not be inspected: {path}: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing a symlink transcript path: {path}"));
    }
    if !metadata.is_file() {
        return Err(format!("refusing a non-file transcript path: {path}"));
    }
    let real = std::fs::canonicalize(candidate)
        .map_err(|error| format!("transcript path could not be resolved: {path}: {error}"))?;
    let real_roots: Vec<PathBuf> = roots
        .iter()
        .filter(|root| root.exists())
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    if !real_roots.iter().any(|root| real.starts_with(root)) {
        return Err(format!(
            "refusing a transcript path that resolves outside trusted roots: {path}"
        ));
    }
    read_file_tail(&real, max_bytes)
        .map(Some)
        .map_err(|error| format!("failed to read {path}: {error}"))
}

/// Bounded tail read (upstream `readTextTail`): read at most `max_bytes` from
/// the end and drop the first partial line when the read started mid-file.
fn read_file_tail(path: &Path, max_bytes: u64) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let size = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    let start = size.saturating_sub(max_bytes);
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .map_err(|error| error.to_string())?;
    }
    let mut buffer = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut buffer)
        .map_err(|error| error.to_string())?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if start > 0 {
        if let Some(newline) = text.find('\n') {
            text = text[newline + 1..].to_string();
        } else {
            text.clear();
        }
    }
    Ok(text)
}

/// Per-rendered-line cap for transcript views (#2015 single-line bound;
/// aligned with `MAX_STREAMED_OUTPUT_LINE_CHARS`, streaming.rs).
const TRANSCRIPT_LINE_MAX_CHARS: usize = 2000;

/// #2017 body bound (fleet-view.ts `appendTranscriptBody` @ b72714de): the
/// rendered body — not just the line count — is capped at 32 KiB; whole
/// recent lines are preferred and earlier lines collapse into one omission
/// marker. Returns whether the marker was inserted.
const TRANSCRIPT_BODY_MAX_BYTES: usize = 32 * 1024;
const TRANSCRIPT_BODY_OMISSION: &str = "  … [earlier lines omitted]";

fn bound_transcript_body(lines: &mut Vec<String>) -> bool {
    let mut bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    if bytes <= TRANSCRIPT_BODY_MAX_BYTES {
        return false;
    }
    // Work newest-first (upstream appends in reverse then flips): drop the
    // oldest lines until the marker fits, then place the marker at the top.
    lines.reverse();
    let marker_bytes = TRANSCRIPT_BODY_OMISSION.len() + 1;
    while bytes + marker_bytes > TRANSCRIPT_BODY_MAX_BYTES {
        match lines.pop() {
            Some(dropped) => bytes -= dropped.len() + 1,
            None => break,
        }
    }
    lines.push(TRANSCRIPT_BODY_OMISSION.to_string());
    lines.reverse();
    true
}

fn bounded_transcript_line(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let count = line.chars().count();
    if count <= TRANSCRIPT_LINE_MAX_CHARS {
        line.to_string()
    } else {
        let mut cut: String = line.chars().take(TRANSCRIPT_LINE_MAX_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

/// Tail of the live transcript artifact. The writer stores one
/// `stdout: <child event>` / `stderr: <text>` line per record; older files and
/// tests may hold raw event JSON. Parse both, render message/tool events, and
/// skip malformed records instead of echoing arbitrary file content.
fn read_transcript_artifact_tail(
    path: &str,
    line_limit: usize,
    roots: &[PathBuf],
) -> Result<Option<(Vec<String>, bool, usize)>, String> {
    let Some(raw) = read_trusted_tail(path, roots, TRANSCRIPT_TAIL_BYTES)? else {
        return Ok(None);
    };
    let all: Vec<&str> = raw.lines().collect();
    let truncated = all.len() > line_limit;
    let start = all.len().saturating_sub(line_limit);
    let mut out = Vec::new();
    let mut malformed = 0usize;
    for line in &all[start..] {
        let trimmed = line.trim();
        let event_line = trimmed.strip_prefix("stdout: ").unwrap_or(trimmed);
        if let Some(stderr) = event_line.strip_prefix("stderr: ") {
            out.push(bounded_transcript_line(&format!("stderr: {stderr}")));
            continue;
        }
        let rendered = match serde_json::from_str::<Value>(event_line) {
            Ok(event) => {
                let event_type = event["type"].as_str().unwrap_or("event");
                match event_type {
                    "message_end" | "tool_result_end" => {
                        let role = event["message"]["role"].as_str().unwrap_or("message");
                        let text = crate::runner::display::extract_text_from_content(
                            &event["message"]["content"],
                        );
                        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        format!("{role}: {first}")
                    }
                    "tool_execution_start" => {
                        format!("tool: {}", event["toolName"].as_str().unwrap_or("?"))
                    }
                    other => other.to_string(),
                }
            }
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        out.push(bounded_transcript_line(&rendered));
    }
    Ok(Some((out, truncated, malformed)))
}

/// JSON preview bound for one tool-call/tool-result part
/// (`stringifyJsonPreview`, fleet-view.ts:169-174 @ 3a63e0ad, #3059).
const TRANSCRIPT_PREVIEW_MAX_CHARS: usize = 240;

/// `stringifyJsonPreview`: a string value is used raw, anything else is
/// compact JSON, truncated to `TRANSCRIPT_PREVIEW_MAX_CHARS` + `…`. Counted in
/// scalar values (upstream slices UTF-16 units; the only intentional
/// difference, astral text never splits a surrogate pair here).
fn stringify_json_preview(value: &Value) -> String {
    let raw = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if raw.chars().count() > TRANSCRIPT_PREVIEW_MAX_CHARS {
        let mut cut: String = raw.chars().take(TRANSCRIPT_PREVIEW_MAX_CHARS).collect();
        cut.push('…');
        cut
    } else {
        raw
    }
}

/// `sessionMessageParts` + `sessionMessageLine` (fleet-view.ts:186-243
/// @ 3a63e0ad, #3059): one session record contributes its text parts and one
/// `[tool: <name> <input>]` part per tool call — the input is the block's
/// `arguments` (Pi's stored field), never the non-existent `args`. Returns
/// `None` for records that carry no renderable part.
fn render_session_message(record: &Value) -> Option<String> {
    let message = record.get("message").filter(|m| m.is_object());
    let target = message.unwrap_or(record);
    let role = target.get("role").and_then(Value::as_str)?;
    let content = target.get("content")?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(text) = content.as_str() {
        if !text.trim().is_empty() {
            parts.push(text.to_string());
        }
    } else if let Some(blocks) = content.as_array() {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.to_string());
                }
                continue;
            }
            let block_type = block.get("type").and_then(Value::as_str);
            if matches!(block_type, Some("toolCall") | Some("tool_call")) {
                let name = block
                    .get("name")
                    .or_else(|| block.get("toolName"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let preview = block
                    .get("arguments")
                    .map(|value| format!(" {}", stringify_json_preview(value)))
                    .unwrap_or_default();
                parts.push(format!("[tool: {name}{preview}]"));
                continue;
            }
            if matches!(block_type, Some("toolResult") | Some("tool_result")) {
                let preview = block
                    .get("result")
                    .map(|value| format!(": {}", stringify_json_preview(value)))
                    .unwrap_or_default();
                parts.push(format!("[tool result{preview}]"));
                continue;
            }
            if let Some(inner) = block.get("content") {
                let preview = stringify_json_preview(inner);
                if !preview.trim().is_empty() {
                    parts.push(preview);
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| format!("{role}: {text}"))
}

/// Tail of a child session JSONL (rpi session format: `{"type":"message",
/// "message":{"role","content"}}` entries): message parts (text + tool-call
/// input previews) split into lines, newest `line_limit` kept, each line
/// bounded. The file is read through the same trusted-root/bounded-tail gate
/// as every other status path.
fn read_session_transcript_tail(
    path: &str,
    line_limit: usize,
    roots: &[PathBuf],
) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = read_trusted_tail(path, roots, TRANSCRIPT_TAIL_BYTES)? else {
        return Ok(None);
    };
    let mut messages: Vec<String> = Vec::new();
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"].as_str() != Some("message") {
            continue;
        }
        if let Some(rendered) = render_session_message(&entry) {
            messages.extend(rendered.split('\n').map(bounded_transcript_line));
        }
    }
    let start = messages.len().saturating_sub(line_limit);
    Ok(Some(messages[start..].to_vec()))
}

/// `formatRunLifecycleDebug` subset (#1037, run-status.ts:86-116): run/dir/
/// state/mode + process terminal + a bounded events summary — deliberately
/// no prompts, secrets, or transcript bodies.
pub fn format_run_lifecycle_debug(id: &str) -> Result<String, String> {
    let status = crate::runner::background::read_run_status(id)?
        .ok_or_else(|| format!("No background run matches '{id}'."))?;
    let run_id = status["runId"].as_str().unwrap_or("?").to_string();
    let run_dir = crate::runner::background::async_runs_dir().join(&run_id);
    let events_path = run_dir.join("events.jsonl");
    let process_terminal = status["processTerminal"]
        .as_object()
        .map(|terminal| {
            format!(
                "{}@{}",
                terminal.get("kind").and_then(Value::as_str).unwrap_or("?"),
                terminal.get("at").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .unwrap_or_else(|| "missing".to_string());
    let mut lines = vec![
        "Run lifecycle debug".to_string(),
        format!("Run: {run_id}"),
        format!("Dir: {}", run_dir.to_string_lossy()),
        format!(
            "Status file: {}",
            run_dir.join("status.json").to_string_lossy()
        ),
        format!("Events log: {}", events_path.to_string_lossy()),
        format!(
            "Session: {}",
            status["sessionId"].as_str().unwrap_or("unknown")
        ),
        format!("State: {}", status["state"].as_str().unwrap_or("?")),
        format!("Mode: {}", status["mode"].as_str().unwrap_or("single")),
        format!("Process terminal: {process_terminal}"),
    ];
    // Bounded events summary: count + last event type only (no payloads —
    // events can carry task text).
    if let Ok(raw) = std::fs::read_to_string(&events_path) {
        let count = raw.lines().filter(|l| !l.trim().is_empty()).count();
        let last = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|entry| entry["type"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .pop();
        lines.push(format!("Events: {count} entries"));
        if let Some(last) = last {
            lines.push(format!("Last event: {last}"));
        }
    }
    Ok(lines.join("\n"))
}

/// `{ id }` or `{ runId }` param for control actions.
fn id_param(params: &Value) -> Option<String> {
    params
        .get("id")
        .or_else(|| params.get("runId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn format_async_status(verb: &str, status: &Value) -> String {
    format!(
        "Background run {} {}: state={}{}",
        status["runId"].as_str().unwrap_or("?"),
        verb,
        status["state"].as_str().unwrap_or("?"),
        status["error"]
            .as_str()
            .map(|error| format!(" error={error}"))
            .unwrap_or_default()
    )
}

/// Doctor sections (extension/doctor.ts:229-270, P0 subset): Runtime,
/// Filesystem, Discovery, Depth/budget, Permission-system env parity.
/// `appendAgentDiagnosticLines` (agent-management.ts:851-857): additive
/// discovery diagnostics block. rpi shape is `path (scope): error` (the
/// upstream block prints `name ?? filePath`; rpi always has the path and the
/// task pins path + scope + error). Returns an empty vector when there is
/// nothing to report so existing output lines stay byte-identical.
pub fn format_discovery_diagnostics(diagnostics: &[discover::DiscoverDiagnostic]) -> Vec<String> {
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["Invalid agent definitions:".to_string()];
    for diagnostic in diagnostics {
        lines.push(format!(
            "- {} ({}): {}",
            diagnostic.path.to_string_lossy(),
            diagnostic.scope.as_str(),
            diagnostic.error
        ));
    }
    lines
}

fn doctor_report(
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    project_trusted: bool,
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

    let (agents, diagnostics) = discover::discover_agents_with_diagnostics_trusted(
        cwd,
        "both",
        settings,
        None,
        project_trusted,
    );
    let by_source = |source: discover::AgentSource| {
        agents.iter().filter(|agent| agent.source == source).count()
    };
    let mut discovery = format!(
        "Discovery:\n- agents: {} (builtin {}, user {}, project {})\n- chains: 0",
        agents.len(),
        by_source(discover::AgentSource::Builtin),
        by_source(discover::AgentSource::User),
        by_source(discover::AgentSource::Project),
    );
    // `doctor.ts:146-147`: invalid definitions are appended to the agents line.
    for diagnostic in &diagnostics {
        discovery.push_str(&format!(
            "\n- invalid agent {} ({}): {}",
            diagnostic.path.to_string_lossy(),
            diagnostic.scope.as_str(),
            diagnostic.error
        ));
    }
    sections.push(discovery);

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

/// `actionScope` (agent-management.ts:84): `user` | `project`, default user.
fn action_scope(params: &Value) -> &'static str {
    match params.get("agentScope").and_then(Value::as_str) {
        Some("project") => "project",
        _ => "user",
    }
}

/// Scope directory for agent definition writes.
fn scope_agent_dir(cwd: &Path, scope: &str) -> PathBuf {
    if scope == "project" {
        crate::paths::get_project_config_dir(cwd).join("agents")
    } else {
        crate::paths::get_agent_dir().join("agents")
    }
}

/// `applyAgentConfig`/serializer subset (agent-serializer.ts): accepted keys
/// map to frontmatter lines; unknown keys are rejected like upstream.
fn write_agent_config(config: &Value, target: &Path) -> Result<(), String> {
    let Some(object) = config.as_object() else {
        return Err("config must be an object.".to_string());
    };
    let mut lines = Vec::new();
    let mut body = String::new();
    for (key, value) in object {
        match key.as_str() {
            "description" => {
                let Some(description) = value.as_str() else {
                    return Err("config.description must be a string.".to_string());
                };
                // #2681 (49a3ec7f): a multiline description is written as a
                // YAML literal block so every line survives rediscovery
                // (upstream agent-serializer.ts:63-72).
                if description.contains('\n') {
                    lines.push("description: |-".to_string());
                    for line in description.split('\n') {
                        lines.push(format!("  {line}"));
                    }
                } else {
                    lines.push(format!("description: {description}"));
                }
            }
            "systemPrompt" => {
                let Some(prompt) = value.as_str() else {
                    return Err("config.systemPrompt must be a string.".to_string());
                };
                body = prompt.to_string();
            }
            "thinking" => {
                // #3074/#3077 + agent-management.ts:521-533: `false` or an
                // empty string clears the field, a known level (trimmed)
                // round-trips, anything else is rejected before writing.
                match value {
                    Value::Bool(false) => {}
                    Value::String(level) => {
                        let level = level.trim();
                        if level.is_empty() {
                            continue;
                        }
                        if crate::launch::model::THINKING_LEVELS.contains(&level) {
                            lines.push(format!("thinking: {level}"));
                        } else {
                            return Err(format!(
                                "config.thinking must be one of {}, or false when provided.",
                                crate::launch::model::THINKING_LEVELS.join(", ")
                            ));
                        }
                    }
                    _ => {
                        return Err(
                            "config.thinking must be a string or false when provided.".to_string()
                        );
                    }
                }
            }
            "mutationTools" => {
                // agent-management.ts:521-525: false deletes, "" clears to
                // an empty list, a comma-separated string/array is parsed.
                match value {
                    Value::Bool(false) => {}
                    Value::String(text) => {
                        let names: Vec<&str> = text
                            .split(',')
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .collect();
                        if !names.is_empty() {
                            lines.push(format!("mutationTools: {}", names.join(", ")));
                        }
                    }
                    Value::Array(items) => {
                        let mut names: Vec<&str> = Vec::with_capacity(items.len());
                        for item in items {
                            let Some(name) = item.as_str() else {
                                return Err(
                                    "config.mutationTools must be a comma-separated string, empty string, string array, or false when provided.".to_string(),
                                );
                            };
                            let name = name.trim();
                            if !name.is_empty() {
                                names.push(name);
                            }
                        }
                        // An explicit empty array clears the list; writing
                        // nothing would fall back to the omitted/inherit state.
                        lines.push(format!("mutationTools: {}", names.join(", ")));
                    }
                    _ => {
                        return Err(
                            "config.mutationTools must be a comma-separated string, empty string, string array, or false when provided.".to_string(),
                        );
                    }
                }
            }
            "model"
            | "systemPromptMode"
            | "defaultContext"
            | "output"
            | "outputMode"
            | "tools"
            | "skills"
            | "aliases"
            | "allowedAgents"
            | "extensions"
            | "excludeTools"
            | "acceptanceRole"
            | "inheritProjectContext"
            | "inheritSkills"
            | "advertise"
            | "allowNestedSubagents"
            | "subagentOnlyExtensions"
            | "defaultAsync"
            | "timeoutMs"
            | "memory" => {
                let rendered = match value {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    Value::Array(items) => {
                        let mut names: Vec<&str> = Vec::with_capacity(items.len());
                        for item in items {
                            let Some(name) = item.as_str() else {
                                return Err(format!(
                                    "config.{key} must be a string or an array of strings when provided."
                                ));
                            };
                            let name = name.trim();
                            if !name.is_empty() {
                                names.push(name);
                            }
                        }
                        // An explicit empty array is a meaningful value for
                        // list fields (e.g. `allowedAgents: []` denies every
                        // descendant, `tools: []` is the empty allowlist).
                        // Dropping the line silently fell back to inherit.
                        names.join(", ")
                    }
                    other => other.to_string(),
                };
                lines.push(format!("{key}: {rendered}"));
            }
            other => {
                return Err(format!(
                    "config.{other} is not a supported agent field (supported: description, systemPrompt, model, thinking, systemPromptMode, defaultContext, output, outputMode, tools, skills, aliases, allowedAgents, extensions, excludeTools, acceptanceRole, inheritProjectContext, inheritSkills, advertise, allowNestedSubagents, subagentOnlyExtensions, defaultAsync, timeoutMs, memory, mutationTools)."
                ));
            }
        }
    }
    if !lines.iter().any(|line| line.starts_with("description:")) {
        return Err("config.description is required.".to_string());
    }
    let content = if lines.is_empty() {
        body
    } else {
        format!("---\n{}\n---\n\n{body}", lines.join("\n"))
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(target, content).map_err(|e| e.to_string())
}

/// Merge an `update` request over the existing agent definition so fields
/// the caller did not mention (model/thinking/tools/mutationTools/body, …)
/// are preserved (upstream `editableAgentConfig` + diff application).
fn merge_update_config(
    existing: &discover::AgentConfig,
    requested: &Value,
) -> Result<Value, String> {
    let mut out = requested
        .as_object()
        .cloned()
        .ok_or_else(|| "config must be an object.".to_string())?;
    let parsed = crate::agents::frontmatter::parse_frontmatter(
        &std::fs::read_to_string(&existing.file_path).map_err(|error| error.to_string())?,
    );
    let mut set = |key: &str, value: Value| {
        out.entry(key.to_string()).or_insert(value);
    };
    set("description", json!(existing.description));
    if let Some(model) = &existing.model {
        set("model", json!(model));
    }
    match &existing.thinking {
        discover::ThinkingSpec::Level(level) => set("thinking", json!(level)),
        discover::ThinkingSpec::Disabled => set("thinking", json!(false)),
        discover::ThinkingSpec::Unset => {}
    }
    if let Some(tools) = &existing.tools {
        set("tools", json!(tools));
    }
    if let Some(mode) = &existing.output_mode {
        set("outputMode", json!(mode));
    }
    if let Some(context) = &existing.default_context {
        set("defaultContext", json!(context.as_str()));
    }
    if let Some(output) = &existing.output {
        set("output", json!(output));
    }
    if let Some(role) = &existing.acceptance_role {
        set("acceptanceRole", json!(role));
    }
    set(
        "inheritProjectContext",
        json!(existing.inherit_project_context),
    );
    set("inheritSkills", json!(existing.inherit_skills));
    if let Some(advertise) = existing.advertise {
        set("advertise", json!(advertise));
    }
    if let Some(allow_nested) = existing.allow_nested_subagents {
        set("allowNestedSubagents", json!(allow_nested));
    }
    if let Some(subagent_only) = &existing.subagent_only_extensions {
        set("subagentOnlyExtensions", json!(subagent_only));
    }
    if let Some(default_async) = existing.default_async {
        set("defaultAsync", json!(default_async));
    }
    if let Some(timeout) = existing.default_timeout_ms {
        set("timeoutMs", json!(timeout));
    }
    if !existing.exclude_tools.is_empty() {
        set("excludeTools", json!(existing.exclude_tools));
    }
    if let Some(memory) = &existing.memory {
        set(
            "memory",
            json!({ "scope": memory.scope, "path": memory.path }),
        );
    }
    if !existing.skills.is_empty() {
        set("skills", json!(existing.skills));
    }
    if let Some(aliases) = &existing.aliases {
        set("aliases", json!(aliases));
    }
    if let Some(allowed) = &existing.allowed_agents {
        set("allowedAgents", json!(allowed));
    }
    if let Some(extensions) = &existing.extensions {
        set("extensions", json!(extensions));
    }
    if !existing.mutation_tools.is_empty() {
        set("mutationTools", json!(existing.mutation_tools));
    }
    set("systemPromptMode", json!(existing.system_prompt_mode));
    set("systemPrompt", json!(parsed.body));
    Ok(Value::Object(out))
}

fn manage_create_update(
    action: &str,
    name: &str,
    config: &Value,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
    trusted: bool,
) -> ToolOutcome {
    if scope == "project" && !trusted {
        return ToolOutcome::error(
            "Project scope requires a trusted project; retry in a trusted session or use scope \"user\"."
                .to_string(),
        );
    }
    let agents =
        discover::discover_agents_trusted(cwd, "both", settings, None, trusted).unwrap_or_default();
    let existing = discover::resolve_agent_name(&agents, name).ok().flatten();
    if action == "create" && existing.is_some() {
        return ToolOutcome::error(format!(
            "Agent '{name}' already exists; use {{ action: \"update\" }} to change it."
        ));
    }
    if action == "update" && existing.is_none() {
        return ToolOutcome::error(format!(
            "Agent '{name}' does not exist; use {{ action: \"create\" }} to add it."
        ));
    }
    let target = scope_agent_dir(cwd, scope).join(format!("{name}.md"));
    let config = if action == "update" {
        let existing = existing.expect("update existence checked");
        if existing.source == discover::AgentSource::Builtin {
            return ToolOutcome::error(format!(
                "Cannot update builtin agent '{name}'; eject it first."
            ));
        }
        match merge_update_config(existing, config) {
            Ok(merged) => merged,
            Err(message) => return ToolOutcome::error(message),
        }
    } else {
        config.clone()
    };
    if let Err(message) = write_agent_config(&config, &target) {
        return ToolOutcome::error(message);
    }
    ToolOutcome::text(format!(
        "Agent '{name}' {} at {}.",
        if action == "create" {
            "created"
        } else {
            "updated"
        },
        target.to_string_lossy()
    ))
}

fn manage_delete(
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
    trusted: bool,
) -> ToolOutcome {
    if scope == "project" && !trusted {
        return ToolOutcome::error(
            "Project scope requires a trusted project; retry in a trusted session or use scope \"user\"."
                .to_string(),
        );
    }
    let agents =
        discover::discover_agents_trusted(cwd, "both", settings, None, trusted).unwrap_or_default();
    let Some(agent) = discover::resolve_agent_name(&agents, name).ok().flatten() else {
        return ToolOutcome::error(format!("Unknown agent: {name}"));
    };
    if agent.source == discover::AgentSource::Builtin {
        return ToolOutcome::error(format!(
            "Agent '{name}' is a built-in agent; use {{ action: \"reset\" }} to remove customizations or \"disable\" to turn it off."
        ));
    }
    let target = scope_agent_dir(cwd, scope).join(format!("{name}.md"));
    if !target.exists() {
        // The definition may live in the other scope; look at the source path.
        let _ = std::fs::remove_file(&agent.file_path);
    } else {
        let _ = std::fs::remove_file(&target);
    }
    ToolOutcome::text(format!("Agent '{name}' deleted."))
}

fn manage_eject(
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
    trusted: bool,
) -> ToolOutcome {
    if scope == "project" && !trusted {
        return ToolOutcome::error(
            "Project scope requires a trusted project; retry in a trusted session or use scope \"user\"."
                .to_string(),
        );
    }
    let agents =
        discover::discover_agents_trusted(cwd, "both", settings, None, trusted).unwrap_or_default();
    let Some(agent) = discover::resolve_agent_name(&agents, name).ok().flatten() else {
        return ToolOutcome::error(format!("Unknown agent: {name}"));
    };
    if agent.source != discover::AgentSource::Builtin {
        return ToolOutcome::error(format!(
            "Agent '{name}' is not a built-in agent; eject applies to built-ins only."
        ));
    }
    let target = scope_agent_dir(cwd, scope).join(format!("{}.md", agent.local_name));
    if target.exists() {
        return ToolOutcome::error(format!(
            "A custom definition for '{name}' already exists at {}.",
            target.to_string_lossy()
        ));
    }
    let Ok(content) = std::fs::read_to_string(&agent.file_path) else {
        return ToolOutcome::error(format!(
            "Failed to read the built-in definition of '{name}'."
        ));
    };
    if let Err(message) = crate::artifacts::write_artifact(&target, &content) {
        return ToolOutcome::error(message.to_string());
    }
    ToolOutcome::text(format!(
        "Ejected built-in agent '{name}' to {} (the copy shadows the built-in).",
        target.to_string_lossy()
    ))
}

/// Settings override write for disable/enable (mergeBuiltinAgentOverride
/// subset: the plugin edits the scope's settings.json `subagents.agentOverrides`).
fn manage_disable_enable(
    disable: bool,
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
    trusted: bool,
) -> ToolOutcome {
    if scope == "project" && !trusted {
        return ToolOutcome::error(
            "Project scope requires a trusted project; retry in a trusted session or use scope \"user\"."
                .to_string(),
        );
    }
    let settings_path = if scope == "project" {
        crate::agents::project_settings_path(cwd)
    } else {
        Some(crate::paths::get_agent_dir().join("settings.json"))
    };
    let Some(settings_path) = settings_path else {
        return ToolOutcome::error("No project settings file location for this cwd.".to_string());
    };
    // #2627/#2680 (TE48 W3/W4): the whole read-modify-write runs under the
    // physical settings file's cross-session lease, and the save replaces the
    // document atomically.
    let result = crate::settings_lease::with_settings_file_lease(
        &settings_path,
        || -> Result<(), String> {
            let mut root: Value = match std::fs::read_to_string(&settings_path) {
                Ok(raw) if !raw.trim().is_empty() => {
                    serde_json::from_str(&raw).map_err(|error| {
                        format!(
                            "settings.json is not valid JSON; refusing to overwrite it: {error}"
                        )
                    })?
                }
                Ok(_) => json!({}),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(error) => {
                    return Err(format!("cannot read settings.json: {error}"));
                }
            };
            let Some(root_object) = root.as_object_mut() else {
                return Err("settings root is not an object.".to_string());
            };
            let subagents = root_object.entry("subagents").or_insert_with(|| json!({}));
            if !subagents.is_object() {
                return Err("settings subagents key is not an object.".to_string());
            }
            let Some(subagents_object) = subagents.as_object_mut() else {
                return Err("settings subagents key is not an object.".to_string());
            };
            let overrides = subagents_object
                .entry("agentOverrides")
                .or_insert_with(|| json!({}));
            let Some(overrides_object) = overrides.as_object_mut() else {
                return Err("settings agentOverrides key is not an object.".to_string());
            };
            let entry = overrides_object
                .entry(name.to_string())
                .or_insert_with(|| json!({}));
            if !entry.is_object() {
                return Err(format!("agentOverrides.{name} is not an object."));
            }
            let Some(entry_object) = entry.as_object_mut() else {
                return Err(format!("agentOverrides.{name} is not an object."));
            };
            if disable {
                entry_object.insert("disabled".to_string(), json!(true));
            } else {
                entry_object.remove("disabled");
            }
            crate::settings_lease::write_settings_json(&settings_path, &root)
                .map_err(|error| error.to_string())
        },
    );
    let _ = settings;
    match result {
        Ok(()) => ToolOutcome::text(format!(
            "Agent '{name}' {}d via settings override ({}).",
            if disable { "disable" } else { "enable" },
            settings_path.to_string_lossy()
        )),
        Err(message) => ToolOutcome::error(message),
    }
}

fn manage_reset(
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
    trusted: bool,
) -> ToolOutcome {
    if scope == "project" && !trusted {
        return ToolOutcome::error(
            "Project scope requires a trusted project; retry in a trusted session or use scope \"user\"."
                .to_string(),
        );
    }
    let target = scope_agent_dir(cwd, scope).join(format!("{name}.md"));
    let mut notes = Vec::new();
    if target.exists() {
        let _ = std::fs::remove_file(&target);
        notes.push(format!("removed {}", target.to_string_lossy()));
    }
    // Drop the scope's disabled override (removeBuiltinAgentOverride subset).
    let settings_path = if scope == "project" {
        crate::agents::project_settings_path(cwd)
    } else {
        Some(crate::paths::get_agent_dir().join("settings.json"))
    };
    if let Some(settings_path) = settings_path.filter(|p| p.exists()) {
        // #2627/#2680 (TE48 W3/W4): read-modify-write under the settings
        // file's cross-session lease; the save is atomic.
        let reset_result = crate::settings_lease::with_settings_file_lease(
            &settings_path,
            || -> Result<bool, String> {
                let raw = match std::fs::read_to_string(&settings_path) {
                    Ok(raw) => raw,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(false);
                    }
                    Err(error) => {
                        return Err(format!("cannot read settings.json: {error}"));
                    }
                };
                let mut root: Value = serde_json::from_str(&raw).map_err(|error| {
                    format!("settings.json is not valid JSON; refusing to overwrite it: {error}")
                })?;
                if !root.is_object() {
                    return Err("settings root is not an object.".to_string());
                }
                let mut changed = false;
                if let Some(entry) = root
                    .get_mut("subagents")
                    .and_then(|s| s.get_mut("agentOverrides"))
                    .and_then(|o| o.get_mut(name))
                    .and_then(Value::as_object_mut)
                {
                    changed = entry.remove("disabled").is_some();
                    if entry.is_empty() {
                        // Empty entries are pruned.
                        root.get_mut("subagents")
                            .and_then(|s| s.as_object_mut())
                            .and_then(|s| s.get_mut("agentOverrides"))
                            .and_then(|o| o.as_object_mut())
                            .map(|o| o.remove(name));
                    }
                }
                if changed {
                    crate::settings_lease::write_settings_json(&settings_path, &root)
                        .map_err(|error| error.to_string())?;
                }
                Ok(changed)
            },
        );
        match reset_result {
            Ok(true) => notes.push("removed the settings override".to_string()),
            Ok(false) => {}
            Err(message) => return ToolOutcome::error(message),
        }
    }
    if notes.is_empty() {
        return ToolOutcome::text(format!(
            "Nothing to reset for '{name}' in the {scope} scope (no custom file or override)."
        ));
    }
    let _ = settings;
    ToolOutcome::text(format!("Reset agent '{name}': {}.", notes.join("; ")))
}

/// `getAgentRefinementPath` (agent-refinements.ts:155):
/// `<cwd>/.rpi/subagents/refinements/<safeAgent>.md`.
fn refinement_path(cwd: &Path, agent: &str) -> PathBuf {
    let safe: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::paths::get_project_config_dir(cwd)
        .join("subagents")
        .join("refinements")
        .join(format!("{safe}.md"))
}

/// Blocked guidance patterns (`validateRefinementProposal` L456-457): global
/// rewrites, safety/tool overrides, settings/agent-file references.
fn refinement_guidance_is_safe(guidance: &str) -> bool {
    const BLOCKED: [&str; 12] = [
        "all agents",
        "every agent",
        "global:",
        "disable acceptance",
        "ignore acceptance",
        "bypass acceptance",
        "override acceptance",
        "disable safety",
        "ignore safety",
        "bypass tools",
        "override tools",
        "settings.json",
    ];
    let lowered = guidance.to_lowercase();
    !BLOCKED.iter().any(|pattern| lowered.contains(pattern))
        && !guidance.contains("```")
        && !guidance.contains("</rpi-subagents-refinement")
        && !guidance.contains("</pi-subagents-refinement")
}

/// Overlay file layout: metadata header + the current-overlay fenced block +
/// the snapshots JSON block (agent-refinements.ts file format). Fenced
/// markers are `rpi-subagents-refinement-*` (de-pi rename); the reader also
/// accepts the pre-rename `pi-subagents-refinement-*` spellings so overlays
/// written by older rpi builds keep parsing.
fn read_refinement(path: &Path) -> Option<(String, Value)> {
    let content = std::fs::read_to_string(path).ok()?;
    let fenced_current = content
        .split("```rpi-subagents-refinement-current\n")
        .nth(1)
        .or_else(|| content.split("```pi-subagents-refinement-current\n").nth(1));
    let current = fenced_current
        .and_then(|rest| rest.split("```").next())
        .unwrap_or("")
        .trim()
        .to_string();
    let fenced_snapshots = content
        .split("```rpi-subagents-refinement-snapshots-json\n")
        .nth(1)
        .or_else(|| {
            content
                .split("```pi-subagents-refinement-snapshots-json\n")
                .nth(1)
        });
    let snapshots = fenced_snapshots
        .and_then(|rest| rest.split("```").next())
        .and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok())
        .unwrap_or_else(|| json!([]));
    Some((current, snapshots))
}

fn write_refinement(path: &Path, agent: &str, current: &str, snapshots: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let revision = snapshots.as_array().map(|a| a.len()).unwrap_or(0);
    let content = format!(
        "---\nagent: {agent}\nrevision: {revision}\nupdatedAt: {}\n---\n\n```rpi-subagents-refinement-current\n{current}\n```\n\n```rpi-subagents-refinement-snapshots-json\n{}\n```\n",
        crate::artifacts::format_iso8601(crate::artifacts::now_millis()),
        serde_json::to_string_pretty(snapshots).unwrap_or_else(|_| "[]".to_string()),
    );
    let _ = std::fs::write(path, content);
}

/// `appendAgentRefinementOverlay` marker block read by the child prompt
/// assembly: `<rpi-subagents-refinement agent=... source=project>`.
pub fn agent_refinement_overlay(cwd: &Path, agent: &str) -> Option<String> {
    let (current, _) = read_refinement(&refinement_path(cwd, agent))?;
    if current.is_empty() {
        return None;
    }
    Some(format!(
        "<rpi-subagents-refinement agent=\"{agent}\" source=\"project\">\n{current}\n</rpi-subagents-refinement>\nThis refinement adjusts how you approach tasks. It does not override tool, task, output, acceptance, or safety instructions."
    ))
}

fn manage_refine(
    action: &str,
    name: &str,
    params: &Value,
    cwd: &Path,
    settings: &SettingsPair,
    trusted: bool,
) -> ToolOutcome {
    // Refinements live in the project tree (`refinement_path`); an untrusted
    // project must not read or write them.
    if !trusted {
        return ToolOutcome::error("Agent refinements require a trusted project.".to_string());
    }
    let agents =
        discover::discover_agents_trusted(cwd, "both", settings, None, trusted).unwrap_or_default();
    let Some(agent) = discover::resolve_agent_name(&agents, name).ok().flatten() else {
        return ToolOutcome::error(format!("Unknown agent: {name}"));
    };
    let path = refinement_path(cwd, name);
    match action {
        "refine.show" => {
            let (current, snapshots) = read_refinement(&path).unwrap_or((String::new(), json!([])));
            let count = snapshots.as_array().map(|a| a.len()).unwrap_or(0);
            if current.is_empty() && count == 0 {
                return ToolOutcome::text(format!("No refinement recorded for '{name}'."));
            }
            ToolOutcome::text(format!(
                "Refinement for '{name}' (revision {count}):\n\n{current}"
            ))
        }
        "refine.rollback" => {
            let Some((_, mut snapshots)) = read_refinement(&path) else {
                return ToolOutcome::error(format!("No refinement recorded for '{name}'."));
            };
            let Some(history) = snapshots.as_array_mut() else {
                return ToolOutcome::error(format!("Corrupt refinement history for '{name}'."));
            };
            let Some(last) = history.pop() else {
                return ToolOutcome::error(format!("No revision to roll back for '{name}'."));
            };
            let before = last["before"].as_str().unwrap_or("");
            write_refinement(&path, name, before, &snapshots);
            ToolOutcome::text(format!(
                "Rolled back refinement for '{name}' to the previous revision (now revision {}).",
                snapshots.as_array().map(|a| a.len()).unwrap_or(0)
            ))
        }
        _ => {
            // `refine`: replace the current overlay with validated guidance
            // (≤3 edits worth, no code fences / blocked patterns).
            let guidance = params
                .get("guidance")
                .or_else(|| params.get("message"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(guidance) = guidance else {
                return ToolOutcome::error(
                    "action \"refine\" requires non-empty guidance.".to_string(),
                );
            };
            if !refinement_guidance_is_safe(guidance) {
                return ToolOutcome::error(
                    "Refinement guidance rejected: code fences, global rewrites, acceptance/safety/tool overrides, or settings/agent-file references are not allowed."
                        .to_string(),
                );
            }
            let (before, mut snapshots) =
                read_refinement(&path).unwrap_or((String::new(), json!([])));
            if let Some(history) = snapshots.as_array_mut() {
                history.push(json!({
                    "revision": history.len(),
                    "at": crate::artifacts::format_iso8601(crate::artifacts::now_millis()),
                    "action": "refine",
                    "before": before,
                    "after": guidance,
                }));
                // Snapshot capacity mirrors the evidence caps (MAX_EVIDENCE_ITEMS=8).
                let overflow = history.len().saturating_sub(8);
                for _ in 0..overflow {
                    history.remove(0);
                }
            }
            write_refinement(&path, name, guidance, &snapshots);
            ToolOutcome::text(format!(
                "Refinement recorded for '{}' at {} (applies to future runs as a <rpi-subagents-refinement> overlay; the base definition is unchanged).",
                agent.name,
                path.to_string_lossy()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry scratch handle for the action-path tests.
    fn action_test_run(run_id: &str, state: &str, steps: Value) {
        use crate::runner::background::{ASYNC_RUNS, AsyncRunHandle};
        let run_dir = std::env::temp_dir().join(run_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        let _ = std::fs::create_dir_all(&run_dir);
        let handle = std::sync::Arc::new(AsyncRunHandle {
            run_id: run_id.to_string(),
            status: std::sync::Arc::new(std::sync::RwLock::new(json!({
                "runId": run_id,
                "mode": "parallel",
                "state": state,
                "steps": steps,
            }))),
            control: std::sync::Arc::new(crate::runner::background::AsyncControl::default()),
            run_dir: run_dir.clone(),
            started_ms: 0,
            status_write_degraded: std::sync::atomic::AtomicBool::new(false),
            pending_status_write_failure: std::sync::Mutex::new(None),
        });
        ASYNC_RUNS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id.to_string(), handle);
    }

    fn cleanup_run(run_id: &str) {
        let run_dir = std::env::temp_dir().join(run_id);
        crate::runner::background::unregister_run_for_test(run_id);
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    fn stop_action(id: &str, child_id: Option<&str>) -> ToolOutcome {
        let params = match child_id {
            Some(child_id) => json!({ "action": "stop", "id": id, "childId": child_id }),
            None => json!({ "action": "stop", "id": id }),
        };
        handle_management_action_with(
            "stop",
            None,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: None,
                runtime: None,
                params: Some(params),
            },
        )
    }

    #[test]
    fn update_merges_requested_fields_over_the_existing_definition() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-update-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("worker.md");
        std::fs::write(
            &target,
            "---\nname: worker\ndescription: old\nmodel: prov/model\nmutationTools: edit, write\n---\nkeep this body\n",
        )
        .unwrap();
        let existing = crate::agents::discover::agent_from_content(
            &std::fs::read_to_string(&target).unwrap(),
            &target,
            crate::agents::discover::AgentSource::User,
        )
        .unwrap()
        .expect("agent");
        let merged = merge_update_config(&existing, &json!({"description": "new"})).unwrap();
        assert_eq!(merged["description"], json!("new"));
        assert_eq!(merged["model"], json!("prov/model"));
        assert_eq!(merged["mutationTools"], json!(["edit", "write"]));
        assert_eq!(merged["systemPrompt"], json!("keep this body"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_config_round_trips_explicit_thinking_off_and_rejects_typos() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-thinking-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("writer.md");
        write_agent_config(
            &serde_json::json!({"description": "Writes", "thinking": " off "}),
            &target,
        )
        .unwrap();
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("thinking: off"), "{content}");
        write_agent_config(
            &serde_json::json!({"description": "Writes", "thinking": "max"}),
            &target,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("thinking: max")
        );
        write_agent_config(
            &serde_json::json!({
                "description": "Writes",
                "mutationTools": ["edit", "write"]
            }),
            &target,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("mutationTools: edit, write")
        );
        write_agent_config(
            &serde_json::json!({"description": "Writes", "mutationTools": false}),
            &target,
        )
        .unwrap();
        assert!(
            !std::fs::read_to_string(&target)
                .unwrap()
                .contains("mutationTools:")
        );
        for bad in ["hgih", "OFF", "none", "false"] {
            let error = write_agent_config(
                &serde_json::json!({"description": "Writes", "thinking": bad}),
                &target,
            )
            .unwrap_err();
            assert!(
                error.contains("config.thinking must be one of"),
                "{bad}: {error}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_config_preserves_empty_arrays_and_rejects_non_string_items() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-arrays-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arrays.md");
        write_agent_config(
            &serde_json::json!({
                "description": "Arrays",
                "allowedAgents": [],
                "tools": [],
                "extensions": [],
                "skills": [],
                "mutationTools": [],
            }),
            &target,
        )
        .unwrap();
        let content = std::fs::read_to_string(&target).unwrap();
        for key in [
            "allowedAgents",
            "tools",
            "extensions",
            "skills",
            "mutationTools",
        ] {
            // An explicit empty list must stay a written field: dropping the
            // line turned `allowedAgents: []` back into "unrestricted".
            assert!(
                content
                    .lines()
                    .any(|line| line.trim_end() == format!("{key}:")),
                "{key} must be written explicitly:
{content}"
            );
        }
        for bad in [
            serde_json::json!({"description": "Arrays", "tools": ["read", 3]}),
            serde_json::json!({"description": "Arrays", "allowedAgents": [null]}),
            serde_json::json!({"description": "Arrays", "skills": [1]}),
            serde_json::json!({"description": "Arrays", "mutationTools": ["edit", {}]}),
        ] {
            let error = write_agent_config(&bad, &target).unwrap_err();
            assert!(
                error.contains("must be a string") || error.contains("string array"),
                "{bad}: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                content,
                "invalid input must not write: {bad}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #2681 (49a3ec7f): a multiline description serializes as a YAML
    /// literal block (`description: |-` + two-space-indented lines) and
    /// survives the discovery parser round-trip; single-line descriptions
    /// keep the inline form.
    #[test]
    fn agent_config_serializes_multiline_description_as_block() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-agents-2681-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (label, description) in [
            ("single", "Summarize project documentation."),
            (
                "multi",
                "Summarize project documentation.\nKeep the summary concise.",
            ),
            (
                "indent",
                "Summarize documentation.\n\n  Keep examples indented.\n保留中文说明。",
            ),
        ] {
            let target = dir.join(format!("{label}.md"));
            write_agent_config(
                &json!({ "description": description, "systemPrompt": "body" }),
                &target,
            )
            .unwrap_or_else(|error| panic!("{label}: {error}"));
            let raw = std::fs::read_to_string(&target).unwrap();
            if description.contains('\n') {
                assert!(raw.contains("\ndescription: |-\n"), "{label}: {raw}");
            } else {
                assert!(raw.contains("\ndescription: Summarize project documentation.\n"));
            }
            let parsed = crate::agents::frontmatter::parse_frontmatter(&raw);
            assert_eq!(
                parsed.frontmatter.get("description").map(String::as_str),
                Some(description),
                "{label} round-trip"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TE51 review P1-8: async resume is session-scoped (async-resume.ts:
    /// 487-489). A run id from another parent session must not be revivable.
    #[test]
    fn resume_rejects_a_run_owned_by_another_session() {
        struct ScopedHost;
        impl crate::HostContext for ScopedHost {
            fn cwd(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
            fn parent_model(&self) -> Option<String> {
                None
            }
            fn parent_session(&self, _settings: &SettingsPair) -> Option<crate::ParentSession> {
                Some(crate::ParentSession {
                    file: None,
                    id: "session-current".to_string(),
                })
            }
        }
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("resume-scoped-{}", std::process::id());
        let runtime = crate::PluginRuntime::new().expect("plugin runtime");
        action_test_run(
            &run_id,
            "failed",
            json!([{ "agent": "w", "status": "failed", "sessionFile": "/tmp/s.json" }]),
        );
        {
            let runs = crate::runner::background::ASYNC_RUNS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let handle = runs.get(run_id.as_str()).expect("registered run");
            let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
            status["sessionId"] = json!("session-other");
        }
        let outcome = handle_management_action_with(
            "resume",
            None,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: Some(&ScopedHost),
                runtime: Some(&runtime),
                params: Some(json!({ "action": "resume", "id": run_id, "task": "continue" })),
            },
        );
        assert!(outcome.is_error, "{}", outcome.text);
        assert_eq!(
            outcome.text,
            format!("Async run '{run_id}' was not found in the active session.")
        );
        cleanup_run(&run_id);
    }

    /// TE38 W5 (#2242 @ b72714de): a stopped last child has no continuation
    /// to revive — resume rejects it with the upstream-verbatim message
    /// shape instead of resurrecting the session.
    #[test]
    fn resume_rejects_stopped_last_child() {
        struct ResumeHost;
        impl crate::HostContext for ResumeHost {
            fn cwd(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
            fn parent_model(&self) -> Option<String> {
                None
            }
            fn parent_session(&self, _settings: &SettingsPair) -> Option<crate::ParentSession> {
                None
            }
        }
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("resume-stopped-{}", std::process::id());
        let runtime = crate::PluginRuntime::new().expect("plugin runtime");
        // `stopped` status form.
        action_test_run(
            &run_id,
            "failed",
            json!([{ "agent": "w", "status": "stopped", "sessionFile": "/tmp/s.json" }]),
        );
        let outcome = handle_management_action_with(
            "resume",
            None,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: Some(&ResumeHost),
                runtime: Some(&runtime),
                params: Some(json!({ "action": "resume", "id": run_id, "task": "continue" })),
            },
        );
        assert!(outcome.is_error, "{}", outcome.text);
        assert_eq!(
            outcome.text,
            format!(
                "Async run '{run_id}' child 0 was stopped and cannot be resumed. Start a new run instead."
            )
        );
        cleanup_run(&run_id);
        // `stopped: true` boolean form (upstream accepts either spelling).
        let run_id2 = format!("resume-stopped2-{}", std::process::id());
        action_test_run(
            &run_id2,
            "failed",
            json!([{ "agent": "w", "stopped": true, "sessionFile": "/tmp/s.json" }]),
        );
        let outcome = handle_management_action_with(
            "resume",
            None,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: Some(&ResumeHost),
                runtime: Some(&runtime),
                params: Some(json!({ "action": "resume", "id": run_id2, "task": "continue" })),
            },
        );
        assert!(outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("was stopped and cannot be resumed"));
        cleanup_run(&run_id2);
    }

    #[test]
    fn stop_on_terminal_run_is_invalid_state() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("stop-terminal-{}", std::process::id());
        action_test_run(
            &run_id,
            "complete",
            json!([{"agent": "w", "status": "complete"}]),
        );
        let outcome = stop_action(&run_id, None);
        assert!(outcome.is_error, "{:?}", outcome.text);
        assert!(
            outcome.text.starts_with("invalid_state:"),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains("is complete; stop only supports running async runs"),
            "{}",
            outcome.text
        );
        cleanup_run(&run_id);
    }

    #[test]
    fn stop_child_id_resolution_and_rejection() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("stop-child-{}", std::process::id());
        action_test_run(
            &run_id,
            "running",
            json!([
                { "agent": "a", "status": "running" },
                { "agent": "b", "status": "complete" },
                { "agent": "c", "status": "pending" },
            ]),
        );
        // Unknown child: rejected, never widened to a run-level stop
        // (no runtime dependency was even needed for the rejection).
        let outcome = stop_action(&run_id, Some("nope"));
        assert!(outcome.is_error);
        assert!(
            outcome.text.contains("was not found under"),
            "{}",
            outcome.text
        );
        // Terminal child: invalid_state naming its status.
        let outcome = stop_action(&run_id, Some("step:1"));
        assert!(outcome.is_error);
        assert!(
            outcome.text.starts_with("invalid_state:"),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains("stop only supports pending or running children"),
            "{}",
            outcome.text
        );
        cleanup_run(&run_id);
    }

    struct TrustHost {
        trusted: Option<bool>,
        cwd: PathBuf,
    }
    impl crate::HostContext for TrustHost {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }
        fn parent_model(&self) -> Option<String> {
            None
        }
        fn parent_session(&self, _settings: &SettingsPair) -> Option<crate::ParentSession> {
            None
        }
        fn project_trusted(&self) -> Option<bool> {
            self.trusted
        }
    }

    fn write_project_agent(project: &Path, name: &str) {
        let dir = paths::get_project_config_dir(project).join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: project only\n---\nproject body\n"),
        )
        .unwrap();
    }

    fn manage_with_host(host: &dyn crate::HostContext, params: Value) -> ToolOutcome {
        let action = params["action"].as_str().unwrap_or("list").to_string();
        let agent = params
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_string);
        handle_management_action_with(
            &action,
            agent.as_deref(),
            &host.cwd(),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: Some(host),
                runtime: None,
                params: Some(params),
            },
        )
    }

    #[test]
    fn management_scope_is_honoured_and_project_scope_fails_closed() {
        let root = paths::temp_root_dir().join(format!("mgmt-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_project_agent(&project, "proj-only");
        std::fs::write(
            paths::get_project_config_dir(&project).join("agents/broken.md"),
            "---\nname: broken\ndescription: broken\nasync: nope\n---\nbody\n",
        )
        .unwrap();

        let untrusted = TrustHost {
            trusted: Some(false),
            cwd: project.clone(),
        };
        let outcome = manage_with_host(
            &untrusted,
            json!({ "action": "list", "agentScope": "project" }),
        );
        assert!(outcome.is_error);
        assert!(
            outcome
                .text
                .contains("Project scope requires a trusted project")
        );

        let outcome = manage_with_host(
            &untrusted,
            json!({ "action": "get", "agent": "proj-only", "agentScope": "project" }),
        );
        assert!(outcome.is_error);
        assert!(
            outcome
                .text
                .contains("Project scope requires a trusted project")
        );

        let trusted = TrustHost {
            trusted: Some(true),
            cwd: project.clone(),
        };
        let user_list =
            manage_with_host(&trusted, json!({ "action": "list", "agentScope": "user" }));
        assert!(!user_list.text.contains("proj-only"), "{}", user_list.text);
        let project_list = manage_with_host(
            &trusted,
            json!({ "action": "list", "agentScope": "project" }),
        );
        assert!(
            project_list.text.contains("proj-only"),
            "{}",
            project_list.text
        );
        let detail = manage_with_host(
            &trusted,
            json!({ "action": "get", "agent": "proj-only", "agentScope": "project" }),
        );
        assert!(!detail.is_error, "{}", detail.text);
        assert!(detail.text.contains("Agent: proj-only"), "{}", detail.text);

        let invalid =
            manage_with_host(&trusted, json!({ "action": "list", "agentScope": "bogus" }));
        assert!(invalid.is_error);
        assert!(
            invalid.text.contains("agentScope must be"),
            "{}",
            invalid.text
        );
        let invalid_type = manage_with_host(&trusted, json!({ "action": "list", "agentScope": 7 }));
        assert!(invalid_type.is_error);
        assert!(
            invalid_type.text.contains("agentScope must be"),
            "{}",
            invalid_type.text
        );
        let broken = manage_with_host(
            &trusted,
            json!({ "action": "get", "agent": "broken", "agentScope": "project" }),
        );
        assert!(broken.is_error, "{}", broken.text);
        assert!(
            broken.text.contains("invalid configuration"),
            "{}",
            broken.text
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_transcript_view_reads_bounded_tail() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-view-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Live transcript artifact with mixed events.
        let transcript = base.join("run_transcript.jsonl");
        let mut body = String::new();
        for index in 0..100 {
            body.push_str(
                &json!({
                    "type": "tool_execution_start",
                    "toolName": format!("tool-{index}"),
                })
                .to_string(),
            );
            body.push('\n');
        }
        body.push_str(
            &json!({
                "type": "message_end",
                "message": { "role": "assistant", "content": [{"type": "text", "text": "final answer\nsecond line"}] },
            })
            .to_string(),
        );
        body.push('\n');
        action_test_run(
            &run_id,
            "running",
            json!([{
                "agent": "worker",
                "status": "running",
                "transcriptPath": transcript.to_string_lossy(),
            }]),
        );
        std::fs::write(&transcript, body).unwrap();
        let text = format_status_transcript(
            &run_id,
            None,
            5,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.starts_with("Run: "), "{}", text);
        assert!(text.contains("Mode: parallel"), "{}", text);
        assert!(text.contains("Child: 0 (worker)"), "{}", text);
        assert!(text.contains("Transcript tail from "), "{}", text);
        assert!(text.contains("(tail truncated)"), "{}", text);
        assert!(text.contains("tool-96"), "{}", text);
        assert!(text.contains("assistant: final answer"), "{}", text);
        // The early tools were elided by the line bound.
        assert!(!text.contains("tool-1\n"), "{}", text);
        // Unknown view values are rejected; fleet stays [DEFER].
        let outcome = handle_management_action_with(
            "status",
            None,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
            &ActionDeps {
                host: None,
                runtime: None,
                params: Some(json!({ "action": "status", "id": run_id, "view": "fleet" })),
            },
        );
        assert!(outcome.is_error);
        assert!(
            outcome.text.contains("Valid: transcript"),
            "{}",
            outcome.text
        );
        cleanup_run(&run_id);
    }

    /// TE38 W6 (#2017 @ b72714de): the rendered body is byte-bounded —
    /// whole recent lines preferred, earlier lines collapse into the
    /// omission marker, and the surviving body stays under 32 KiB.
    #[test]
    fn status_transcript_body_byte_budget() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-budget-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // 100 lines × ~900 chars ≈ 90 KiB rendered body — over the budget.
        let transcript = base.join("run_transcript.jsonl");
        let mut body = String::new();
        for index in 0..100 {
            body.push_str(
                &json!({
                    "type": "message_end",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": format!("line-{index:03} {}", "x".repeat(900)) }] },
                })
                .to_string(),
            );
            body.push('\n');
        }
        action_test_run(
            &run_id,
            "running",
            json!([{
                "agent": "worker",
                "status": "running",
                "transcriptPath": transcript.to_string_lossy(),
            }]),
        );
        std::fs::write(&transcript, body).unwrap();
        let text = format_status_transcript(
            &run_id,
            None,
            500,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(
            text.contains("… [earlier lines omitted]"),
            "marker missing: {}",
            &text[text.len().saturating_sub(200)..]
        );
        assert!(text.contains("line-099"), "recent lines kept");
        assert!(!text.contains("line-000"), "oldest lines dropped");
        // Surviving body (marker .. end) stays under the byte budget.
        let body_start = text.find("… [earlier lines omitted]").expect("marker");
        let rendered = &text[body_start..];
        assert!(
            rendered.len() <= 32 * 1024 + 64,
            "body budget exceeded: {}",
            rendered.len()
        );
        cleanup_run(&run_id);
    }

    #[test]
    fn transcript_renders_tool_call_arguments_and_bounds_previews() {
        // #3059: Pi stores a tool call's input in `arguments`; the old
        // `args` field never exists on a session record.
        let record = json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "running it" },
                    { "type": "toolCall", "name": "bash", "arguments": { "command": "ls -la" } },
                    { "type": "toolCall", "name": "read", "args": { "path": "/tmp/ignored" } },
                    { "type": "toolResult", "name": "bash", "result": "ok", "isError": false }
                ]
            }
        });
        let rendered = render_session_message(&record).expect("renders");
        assert_eq!(
            rendered,
            "assistant: running it\n[tool: bash {\"command\":\"ls -la\"}]\n[tool: read]\n[tool result: ok]"
        );
        // The input preview is capped at 240 characters + an ellipsis.
        let long = "x".repeat(400);
        let record = json!({
            "type": "message",
            "message": {"role": "assistant", "content": [
                { "type": "toolCall", "toolName": "bash", "arguments": { "command": long } }
            ]}
        });
        let rendered = render_session_message(&record).expect("renders");
        assert!(rendered.ends_with("…]"), "{rendered}");
        let preview = rendered
            .split_once("[tool: bash ")
            .and_then(|(_, rest)| rest.strip_suffix(']'))
            .expect("preview");
        assert_eq!(preview.chars().count(), TRANSCRIPT_PREVIEW_MAX_CHARS + 1);
        // A record without renderable parts yields nothing.
        assert!(
            render_session_message(
                &json!({"type":"message","message":{"role":"assistant","content":[]}})
            )
            .is_none()
        );
    }

    #[test]
    fn transcript_renders_real_tool_result_messages_and_null_arguments() {
        let record = json!({
            "type": "message",
            "message": {"role": "toolResult", "content": [{"type": "text", "text": "exit 0"}]}
        });
        assert_eq!(
            render_session_message(&record).as_deref(),
            Some("toolResult: exit 0")
        );
        let record = json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "toolCall", "name": "bash", "arguments": null}]
            }
        });
        assert_eq!(
            render_session_message(&record).as_deref(),
            Some("assistant: [tool: bash null]")
        );
    }

    #[test]
    fn status_transcript_falls_back_to_session_file() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-session-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let session = base.join("session.jsonl");
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "sessionFile": session.to_string_lossy(),
            }]),
        );
        std::fs::write(
            &session,
            concat!(
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"do the thing\"}}\n",
                "{\"type\":\"summary\",\"summary\":\"ignored\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}]}}\n",
            ),
        )
        .unwrap();
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.contains("Session transcript tail from "), "{}", text);
        assert!(text.contains("user: do the thing"), "{}", text);
        assert!(text.contains("assistant: done"), "{}", text);
        assert!(
            text.contains("assistant: [tool: bash {\"command\":\"ls\"}]"),
            "{}",
            text
        );
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_reads_the_production_stdout_prefix() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-prefix-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let transcript = base.join("run_transcript.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "stdout: {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"rendered from stdout\"}]}}\n",
                "stderr: a warning line\n",
            ),
        )
        .unwrap();
        action_test_run(
            &run_id,
            "running",
            json!([{
                "agent": "worker",
                "status": "running",
                "transcriptPath": transcript.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.contains("assistant: rendered from stdout"), "{text}");
        assert!(text.contains("stderr: a warning line"), "{text}");
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_prefers_session_records_over_the_artifact() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-priority-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let transcript = base.join("run_transcript.jsonl");
        let session = base.join("session.jsonl");
        std::fs::write(
            &transcript,
            "stdout: {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"artifact text\"}]}}\n",
        )
        .unwrap();
        std::fs::write(
            &session,
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}},{\"type\":\"text\",\"text\":\"session text\"}]}}\n",
        )
        .unwrap();
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": transcript.to_string_lossy(),
                "sessionFile": session.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.contains("Session transcript tail from "), "{text}");
        assert!(text.contains("[tool: bash {\"command\":\"ls\"}]"), "{text}");
        assert!(!text.contains("artifact text"), "{text}");
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_refuses_paths_outside_trusted_roots() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-outside-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let outside = std::env::temp_dir().join(format!("{run_id}-secret.txt"));
        std::fs::write(&outside, "TOP-SECRET-CONTENT\n").unwrap();
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": outside.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(!text.contains("TOP-SECRET-CONTENT"), "{text}");
        assert!(text.contains("outside trusted roots"), "{text}");
        let _ = std::fs::remove_file(&outside);
        cleanup_run(&run_id);
    }

    #[cfg(unix)]
    #[test]
    fn status_transcript_refuses_a_symlink_leaf() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-symlink-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let outside = std::env::temp_dir().join(format!("{run_id}-target.txt"));
        std::fs::write(&outside, "SYMLINK-SECRET\n").unwrap();
        let link = base.join("link.txt");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": link.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(!text.contains("SYMLINK-SECRET"), "{text}");
        assert!(text.contains("refusing a symlink"), "{text}");
        let _ = std::fs::remove_file(&outside);
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_refuses_relative_and_dotdot_paths() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-dotdot-{}", std::process::id());
        let base = paths::temp_artifacts_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let secret = std::env::temp_dir().join(format!("{run_id}-secret.txt"));
        std::fs::write(&secret, "DOTDOT-SECRET\n").unwrap();
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": base.join("../../../").join(format!("{run_id}-secret.txt")).to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(!text.contains("DOTDOT-SECRET"), "{text}");
        assert!(text.contains("outside trusted roots"), "{text}");
        // A relative status path is refused before any filesystem lookup.
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": "relative.jsonl",
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.contains("non-absolute"), "{text}");
        let _ = std::fs::remove_file(&secret);
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_bounds_a_large_artifact_to_the_tail() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-large-{}", std::process::id());
        let base = paths::temp_artifacts_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let transcript = base.join("run_transcript.jsonl");
        let mut body = String::new();
        body.push_str("stdout: {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"FIRST-MARKER\"}]}}\n");
        let filler = format!(
            "stdout: {{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
            "filler ".repeat(180)
        );
        for _ in 0..2000 {
            body.push_str(&filler);
        }
        body.push_str("stdout: {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"LAST-MARKER\"}]}}\n");
        assert!(body.len() > 2 * 1024 * 1024);
        std::fs::write(&transcript, body).unwrap();
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": transcript.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            500,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(!text.contains("FIRST-MARKER"), "old tail must be dropped");
        assert!(text.contains("LAST-MARKER"), "newest line must survive");
        assert!(
            text.len() < 64 * 1024,
            "body must stay bounded: {}",
            text.len()
        );
        cleanup_run(&run_id);
    }

    #[cfg(unix)]
    #[test]
    fn status_transcript_rejects_a_symlinked_root_read() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-rootlink-{}", std::process::id());
        let link = paths::temp_root_dir().join(format!("rootlink-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/", &link).unwrap();
        let canary = std::env::temp_dir().join(format!("{run_id}-canary.jsonl"));
        std::fs::write(
            &canary,
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ROOTLINK-SECRET\"}]}}\n",
        )
        .unwrap();
        let canary_through_link = link.join(canary.strip_prefix("/").unwrap());
        action_test_run(
            &run_id,
            "complete",
            json!([{
                "agent": "worker",
                "status": "complete",
                "transcriptPath": canary_through_link.to_string_lossy(),
            }]),
        );
        let settings = SettingsPair {
            session_dir: Some(link.to_string_lossy().to_string()),
            ..SettingsPair::default()
        };
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &settings,
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(!text.contains("ROOTLINK-SECRET"), "{text}");
        assert!(
            text.contains("outside trusted roots")
                || text.contains("resolves outside trusted roots"),
            "{text}"
        );
        let roots = status_transcript_roots(
            Path::new("/tmp"),
            &settings,
            &crate::config::ExtensionConfig::new(),
        );
        assert!(
            !roots.iter().any(|root| root.parent().is_none()),
            "{roots:?}"
        );
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&canary);
        cleanup_run(&run_id);
    }

    #[test]
    fn status_transcript_roots_reject_a_filesystem_root_session_dir() {
        let settings = SettingsPair {
            session_dir: Some("/".to_string()),
            ..SettingsPair::default()
        };
        let config = crate::config::ExtensionConfig {
            default_session_dir: Some("/".to_string()),
            ..crate::config::ExtensionConfig::new()
        };
        let roots = status_transcript_roots(Path::new("/tmp"), &settings, &config);
        assert!(
            !roots.iter().any(|root| root.parent().is_none()),
            "{roots:?}"
        );
    }

    #[test]
    fn status_transcript_skips_malformed_records_instead_of_echoing_them() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("transcript-malformed-{}", std::process::id());
        let base = paths::temp_root_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let transcript = base.join("run_transcript.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "stdout: {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"good line\"}]}}\n",
                "NOT-JSON-ARBITRARY-CONTENT\n",
            ),
        )
        .unwrap();
        action_test_run(
            &run_id,
            "running",
            json!([{
                "agent": "worker",
                "status": "running",
                "transcriptPath": transcript.to_string_lossy(),
            }]),
        );
        let text = format_status_transcript(
            &run_id,
            None,
            80,
            Path::new("/tmp"),
            &SettingsPair::default(),
            &crate::config::ExtensionConfig::new(),
        )
        .expect("view renders");
        assert!(text.contains("assistant: good line"), "{text}");
        assert!(!text.contains("NOT-JSON-ARBITRARY-CONTENT"), "{text}");
        assert!(text.contains("skipped 1 malformed record"), "{text}");
        cleanup_run(&run_id);
    }

    #[test]
    fn debug_run_reports_lifecycle_without_transcripts() {
        let _guard = crate::runner::background::tests::REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = format!("debug-run-{}", std::process::id());
        // format_run_lifecycle_debug reads events.jsonl from the real async
        // runs dir (the same place live runs write).
        let base = crate::runner::background::async_runs_dir().join(&run_id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // An events log whose payloads carry task text — the debug view must
        // not print them.
        std::fs::write(
            base.join("events.jsonl"),
            concat!(
                "{\"type\":\"run.started\",\"runId\":\"x\"}\n",
                "{\"type\":\"control.steer\",\"message\":\"SECRET TASK TEXT\"}\n",
            ),
        )
        .unwrap();
        action_test_run(
            &run_id,
            "running",
            json!([{"agent": "w", "status": "running"}]),
        );
        let text = format_run_lifecycle_debug(&run_id).expect("debug renders");
        assert!(text.starts_with("Run lifecycle debug"), "{}", text);
        assert!(text.contains(&format!("Run: {run_id}")), "{}", text);
        assert!(text.contains("State: running"), "{}", text);
        assert!(text.contains("Events: 2 entries"), "{}", text);
        assert!(text.contains("Last event: control.steer"), "{}", text);
        assert!(!text.contains("SECRET TASK TEXT"), "{}", text);
        assert!(
            format_run_lifecycle_debug("no-such-run-xyz").is_err(),
            "unknown run is an error"
        );
        cleanup_run(&run_id);
    }

    #[test]
    fn list_capabilities_mode_shapes() {
        let agents = crate::agents::builtin::load_builtin_agents(None);
        let listing = format_agent_capabilities_list(&agents, None);
        assert!(
            listing.starts_with("Executable agents (capabilities):"),
            "{}",
            listing
        );
        // Oracle declares thinking high and an explicit tool list.
        assert!(listing.contains("Thinking: high"), "{}", listing);
        assert!(
            listing.contains("Tools: read, grep, find, ls, bash"),
            "{}",
            listing
        );
        assert!(
            listing.contains("Model: inherits current session"),
            "{}",
            listing
        );
        // Structured records: every builtin appears once with the stable fields.
        let snapshot = agent_capabilities_snapshot(&agents, None);
        let records = snapshot["agents"].as_array().unwrap();
        assert_eq!(records.len(), agents.len());
        for record in records {
            assert!(record["name"].is_string());
            assert!(record["executable"].as_bool() == Some(true));
            assert!(record["tools"].is_object());
            // Upstream row shape: `model` carries value/thinking ONLY.
            assert!(record["model"].as_object().unwrap().contains_key("value"));
            assert!(
                !record["model"]
                    .as_object()
                    .unwrap()
                    .contains_key("allowedAgents")
            );
            assert!(record["execution"].is_object());
            assert!(record["output"].is_object());
            assert!(record["extensions"].is_object());
        }
        assert_eq!(snapshot["restrictedCount"], json!(0));
    }

    #[test]
    fn capability_rows_report_the_acceptance_role() {
        // #2213 wording pin (upstream formatAcceptanceSummary): an
        // acceptance ROLE reads `Acceptance role: <role>` in the prose row
        // and rides `acceptance.role` in the structured record. Seeded on
        // a builtin because none declares a role (the old builtin-only
        // assertion was vacuous).
        let mut agents = crate::agents::builtin::load_builtin_agents(None);
        agents[0].acceptance_role = Some("read-only".to_string());
        let listing = format_agent_capabilities_list(&agents, None);
        assert!(
            listing.contains("; Acceptance role: read-only"),
            "listing was: {listing}"
        );
        let snapshot = agent_capabilities_snapshot(&agents, None);
        let seeded = &snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == json!(agents[0].name))
            .unwrap();
        assert_eq!(seeded["acceptance"]["role"], json!("read-only"));
    }

    #[test]
    fn capabilities_surface_partitions_the_ceiling() {
        // #2338 regression: BOTH capability surfaces (text + snapshot)
        // must partition non-allowlisted agents instead of reporting every
        // agent as executable (the old snapshot hardcoded
        // `executable: true` / `restrictedCount: 0`).
        let agents = crate::agents::builtin::load_builtin_agents(None);
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        let survivor = names[0];
        let ceiling = crate::launch::args::DescendantAllowlist {
            allowed_agents: vec![survivor.to_string()],
            sources: vec!["project .agents/agents.yaml".to_string()],
        };
        let listing = format_agent_capabilities_list(&agents, Some(&ceiling));
        assert!(
            listing.contains("Restricted agents (not executable in this session; capability ceiling: project .agents/agents.yaml):"),
            "{}",
            listing
        );
        let snapshot = agent_capabilities_snapshot(&agents, Some(&ceiling));
        let records = snapshot["agents"].as_array().unwrap();
        assert_eq!(records.len(), agents.len());
        let restricted: Vec<&Value> = records
            .iter()
            .filter(|r| r["executable"].as_bool() == Some(false))
            .collect();
        assert_eq!(restricted.len(), agents.len() - 1);
        assert_eq!(snapshot["restrictedCount"], json!(agents.len() - 1));
        assert_eq!(
            snapshot["capabilityCeilingSources"],
            json!(["project .agents/agents.yaml"])
        );
        for record in &restricted {
            assert_eq!(
                record["restrictionSources"],
                json!(["project .agents/agents.yaml"]),
                "restricted rows carry the ceiling sources"
            );
        }
        // The survivor stays executable; upstream OMITS the
        // restrictionSources key on executable rows (no null).
        let executable: Vec<&Value> = records
            .iter()
            .filter(|r| r["executable"].as_bool() == Some(true))
            .collect();
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0]["name"], json!(survivor));
        assert!(
            executable[0].get("restrictionSources").is_none(),
            "executable rows omit restrictionSources entirely"
        );
    }

    #[test]
    fn list_and_detail_format() {
        let agents = crate::agents::builtin::load_builtin_agents(None);
        let listing = format_agent_list_with_ceiling(&agents, None);
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
