//! Management actions: list / get / status / doctor (FR-P0-10).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

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

/// Extra dependencies the P1 control actions need (async registry access).
pub struct ActionDeps<'a> {
    pub host: Option<&'a dyn crate::HostContext>,
    pub runtime: Option<&'a crate::PluginRuntime>,
    /// Raw call params (for control actions needing ids/messages).
    pub params: Option<Value>,
}

/// `handleManagementAction` (agent-management.ts:1242 dispatch, P1 subset).
pub fn handle_management_action_with(
    action: &str,
    agent_name: Option<&str>,
    cwd: &Path,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    deps: &ActionDeps<'_>,
) -> ToolOutcome {
    let raw_params = deps.params.clone().unwrap_or(Value::Null);
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
            // Foreground memory + live async registry (async-status.ts shape).
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
                // Cooperative stop + direct child signalling; the bounded
                // terminal wait happens on the plugin runtime.
                let (Some(runtime), Some(_host)) = (deps.runtime, deps.host) else {
                    return ToolOutcome::error(
                        "action \"stop\" is unavailable in this context.".to_string(),
                    );
                };
                match crate::runner::background::find_run(&id) {
                    Some(handle) => {
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
            let target_index = raw_params.get("targetIndex").and_then(Value::as_u64).map(|v| v as usize);
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
            let (Some(id), Some(host), Some(runtime)) = (
                id_param(&raw_params),
                deps.host,
                deps.runtime,
            ) else {
                return ToolOutcome::error(
                    "action \"resume\" requires the run id and an active session context.".to_string(),
                );
            };
            let task = raw_params
                .get("task")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);
            let Some(status) = crate::runner::background::read_run_status(&id) else {
                return ToolOutcome::error(format!("No background run matches '{id}'."));
            };
            if matches!(status["state"].as_str(), Some("running") | Some("queued")) {
                return ToolOutcome::error(format!(
                    "Background run {id} is still {state}; resume applies to interrupted or stopped runs.",
                    state = status["state"].as_str().unwrap_or("?")
                ));
            }
            let session_file = status["steps"]
                .as_array()
                .and_then(|steps| steps.last())
                .and_then(|step| step["sessionFile"].as_str())
                .map(str::to_string);
            let Some(session_file) = session_file else {
                return ToolOutcome::error(format!(
                    "Background run {id} has no persisted child session to resume from."
                ));
            };
            let agent_name = status["steps"]
                .as_array()
                .and_then(|steps| steps.last())
                .and_then(|step| step["agent"].as_str())
                .unwrap_or("worker")
                .to_string();
            let task = task.unwrap_or_else(|| {
                "Continue the interrupted task from this session.".to_string()
            });
            // New async run reviving the old session file.
            let params = serde_json::json!({
                "agent": agent_name,
                "task": task,
                "sessionFile": session_file,
                "async": true,
            });
            let host_cwd = host.cwd();
            let settings = crate::config::read_settings_pair(&host_cwd);
            let config = crate::config::load_config();
            let result = crate::tool::execute_subagent_tool(&params, host, &settings, &config, runtime);
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
            let session_id = raw_params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(session_id) = session_id else {
                return ToolOutcome::error(
                    "action \"grant-spawn-budget\" requires a session id in an interactive session."
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
                return ToolOutcome::error(format!(
                    "action \"{action}\" requires an agent name."
                ));
            };
            let scope = action_scope(&raw_params);
            let config_body = raw_params.get("config").cloned().unwrap_or(Value::Null);
            manage_create_update(action, name, &config_body, cwd, settings, scope)
        }
        "delete" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"delete\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_delete(name, cwd, settings, scope)
        }
        "eject" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"eject\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_eject(name, cwd, settings, scope)
        }
        "disable" | "enable" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error(format!(
                    "action \"{action}\" requires an agent name."
                ));
            };
            let scope = action_scope(&raw_params);
            manage_disable_enable(action == "disable", name, cwd, settings, scope)
        }
        "reset" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error("action \"reset\" requires an agent name.".to_string());
            };
            let scope = action_scope(&raw_params);
            manage_reset(name, cwd, settings, scope)
        }
        // Refinement overlay (agent-refinements.ts, FR-P1-08).
        "refine" | "refine.show" | "refine.rollback" => {
            let Some(name) = agent_name else {
                return ToolOutcome::error(format!(
                    "action \"{action}\" requires an agent name."
                ));
            };
            manage_refine(action, name, &raw_params, cwd, settings)
        }
        "doctor" => ToolOutcome::text(doctor_report(cwd, settings, config)),
        other => ToolOutcome::error(format!(
            "Unknown subagent action \"{other}\". Supported actions: list, get, status, interrupt, stop, grant-spawn-budget, doctor."
        )),
    }
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
                lines.push(format!("description: {description}"));
            }
            "systemPrompt" => {
                let Some(prompt) = value.as_str() else {
                    return Err("config.systemPrompt must be a string.".to_string());
                };
                body = prompt.to_string();
            }
            "model" | "thinking" | "systemPromptMode" | "defaultContext" | "output" | "tools"
            | "skills" | "aliases" | "fallbackModels" | "extensions" => {
                let rendered = match value {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    other => other.to_string(),
                };
                lines.push(format!("{key}: {rendered}"));
            }
            other => {
                return Err(format!(
                    "config.{other} is not a supported agent field (supported: description, systemPrompt, model, thinking, systemPromptMode, defaultContext, output, tools, skills, aliases, fallbackModels, extensions)."
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

fn manage_create_update(
    action: &str,
    name: &str,
    config: &Value,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
) -> ToolOutcome {
    let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
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
    if let Err(message) = write_agent_config(config, &target) {
        return ToolOutcome::error(message);
    }
    ToolOutcome::text(format!(
        "Agent '{name}' {} at {}.",
        if action == "create" { "created" } else { "updated" },
        target.to_string_lossy()
    ))
}

fn manage_delete(
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
) -> ToolOutcome {
    let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
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
) -> ToolOutcome {
    let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
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
) -> ToolOutcome {
    let settings_path = if scope == "project" {
        crate::agents::project_settings_path(cwd)
    } else {
        Some(crate::paths::get_agent_dir().join("settings.json"))
    };
    let Some(settings_path) = settings_path else {
        return ToolOutcome::error("No project settings file location for this cwd.".to_string());
    };
    let mut root: Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }
    if !root.is_object() {
        root = json!({});
    }
    let Some(root_object) = root.as_object_mut() else {
        return ToolOutcome::error("settings root is not an object.".to_string());
    };
    let subagents = root_object
        .entry("subagents")
        .or_insert_with(|| json!({}));
    if !subagents.is_object() {
        return ToolOutcome::error("settings subagents key is not an object.".to_string());
    }
    let Some(subagents_object) = subagents.as_object_mut() else {
        return ToolOutcome::error("settings subagents key is not an object.".to_string());
    };
    let overrides = subagents_object
        .entry("agentOverrides")
        .or_insert_with(|| json!({}));
    let Some(overrides_object) = overrides.as_object_mut() else {
        return ToolOutcome::error("settings agentOverrides key is not an object.".to_string());
    };
    let entry = overrides_object
        .entry(name.to_string())
        .or_insert_with(|| json!({}));
    if !entry.is_object() {
        return ToolOutcome::error(format!("agentOverrides.{name} is not an object."));
    }
    let Some(entry_object) = entry.as_object_mut() else {
        return ToolOutcome::error(format!("agentOverrides.{name} is not an object."));
    };
    if disable {
        entry_object.insert("disabled".to_string(), json!(true));
    } else {
        entry_object.remove("disabled");
    }
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(message) = std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&root).unwrap_or_default(),
    ) {
        return ToolOutcome::error(message.to_string());
    }
    let _ = settings;
    ToolOutcome::text(format!(
        "Agent '{name}' {}d via settings override ({}).",
        if disable { "disable" } else { "enable" },
        settings_path.to_string_lossy()
    ))
}

fn manage_reset(
    name: &str,
    cwd: &Path,
    settings: &SettingsPair,
    scope: &str,
) -> ToolOutcome {
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
        if let Ok(raw) = std::fs::read_to_string(&settings_path) {
            if let Ok(mut root) = serde_json::from_str::<Value>(&raw) {
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
                    let _ = std::fs::write(
                        &settings_path,
                        serde_json::to_string_pretty(&root).unwrap_or_default(),
                    );
                    notes.push("removed the settings override".to_string());
                }
            }
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
        && !guidance.contains("</pi-subagents-refinement")
}

/// Overlay file layout: metadata header + the current-overlay fenced block +
/// the snapshots JSON block (agent-refinements.ts file format).
fn read_refinement(path: &Path) -> Option<(String, Value)> {
    let content = std::fs::read_to_string(path).ok()?;
    let current = content
        .split("```pi-subagents-refinement-current\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .unwrap_or("")
        .trim()
        .to_string();
    let snapshots = content
        .split("```pi-subagents-refinement-snapshots-json\n")
        .nth(1)
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
        "---\nagent: {agent}\nrevision: {revision}\nupdatedAt: {}\n---\n\n```pi-subagents-refinement-current\n{current}\n```\n\n```pi-subagents-refinement-snapshots-json\n{}\n```\n",
        crate::artifacts::format_iso8601(crate::artifacts::now_millis()),
        serde_json::to_string_pretty(snapshots).unwrap_or_else(|_| "[]".to_string()),
    );
    let _ = std::fs::write(path, content);
}

/// `appendAgentRefinementOverlay` marker block read by the child prompt
/// assembly: `<pi-subagents-refinement agent=... source=project>`.
pub fn agent_refinement_overlay(cwd: &Path, agent: &str) -> Option<String> {
    let (current, _) = read_refinement(&refinement_path(cwd, agent))?;
    if current.is_empty() {
        return None;
    }
    Some(format!(
        "<pi-subagents-refinement agent=\"{agent}\" source=\"project\">\n{current}\n</pi-subagents-refinement>\nThis refinement adjusts how you approach tasks. It does not override tool, task, output, acceptance, or safety instructions."
    ))
}

fn manage_refine(
    action: &str,
    name: &str,
    params: &Value,
    cwd: &Path,
    settings: &SettingsPair,
) -> ToolOutcome {
    let agents = discover::discover_agents(cwd, "both", settings, None).unwrap_or_default();
    let Some(agent) = discover::resolve_agent_name(&agents, name).ok().flatten() else {
        return ToolOutcome::error(format!("Unknown agent: {name}"));
    };
    let path = refinement_path(cwd, name);
    match action {
        "refine.show" => {
            let (current, snapshots) = read_refinement(&path)
                .unwrap_or((String::new(), json!([])));
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
                "Refinement recorded for '{}' at {} (applies to future runs as a <pi-subagents-refinement> overlay; the base definition is unchanged).",
                agent.name,
                path.to_string_lossy()
            ))
        }
    }
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
