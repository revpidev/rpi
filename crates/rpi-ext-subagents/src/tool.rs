//! `subagent` tool: parameter schema, validation, single foreground
//! delegation dispatch and result assembly (FR-P0-01).

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::agents::discover::{self, ContextMode};
use crate::artifacts::{self, ArtifactDirPreference};
use crate::config::SettingsPair;
use crate::runner::budget;
use crate::runner::foreground::{self, ForegroundRunInput};
use crate::session_fork;
use crate::HostContext;

/// Records of completed foreground runs for `{ action: "status" }`
/// (P0 memory; the async run registry is FR-P1-04).
#[derive(Debug, Clone)]
pub struct ForegroundRunRecord {
    pub run_id: String,
    pub agent: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RunMemory {
    pub runs: Vec<ForegroundRunRecord>,
    /// P0 counting cap for `maxSubagentSpawnsPerRun` (single-run spawns;
    /// the cross-process claim ledger is FR-P1-04).
    pub spawns_by_run: std::collections::BTreeMap<String, u64>,
}

pub static FOREGROUND_RUN_MEMORY: Mutex<RunMemory> = Mutex::new(RunMemory {
    runs: Vec::new(),
    spawns_by_run: std::collections::BTreeMap::new(),
});

pub struct ToolOutcome {
    pub text: String,
    pub details: Value,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn text(text: String) -> Self {
        Self {
            text,
            details: json!({ "mode": "single", "results": [] }),
            is_error: false,
        }
    }

    pub fn error(text: String) -> Self {
        Self {
            text,
            details: json!({ "mode": "single", "results": [] }),
            is_error: true,
        }
    }

    pub fn to_tool_result(&self) -> Value {
        json!({
            "content": [{ "type": "text", "text": self.text }],
            "details": self.details,
            "isError": self.is_error,
        })
    }
}

/// Parameter schema (schemas.ts:257-378 P0 subset; all optional, no
/// additionalProperties restriction — upstream TypeBox behavior).
pub fn tool_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "description": "Agent name or alias to delegate to (or the management target for action calls)."
            },
            "task": {
                "type": "string",
                "description": "Complete task text for the child subagent."
            },
            "action": {
                "type": "string",
                "minLength": 1,
                "description": "Management/control action (list, get, status, doctor). Presence switches to management mode."
            },
            "context": {
                "type": "string",
                "enum": ["fresh", "fork"],
                "description": "Explicit context overrides every child: fresh (isolated session) or fork (branch of the parent session)."
            },
            "async": {
                "type": "boolean",
                "description": "Not supported in this version (P1); P0 delegation is foreground/blocking."
            },
            "timeoutMs": {
                "type": "integer",
                "description": "Run timeout in milliseconds; positive integer. Alias of maxRuntimeMs."
            },
            "maxRuntimeMs": {
                "type": "integer",
                "description": "Alias of timeoutMs; when both are given they must be equal."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory for the child session."
            },
            "artifacts": {
                "type": "boolean",
                "description": "Write the artifact trail (input/output/transcript/meta); default true."
            },
            "sessionDir": {
                "type": "string",
                "description": "Root directory for child session files (default: derived from the parent session)."
            },
            "output": {
                "type": ["string", "boolean"],
                "description": "Output file the child result is saved to (string path); false disables."
            },
            "model": {
                "type": "string",
                "description": "Per-run model override (provider/id)."
            },
            "agentScope": {
                "type": "string",
                "enum": ["user", "project", "both"],
                "description": "Discovery scope for list/agent resolution; default both."
            },
            "workflowScript": {
                "type": "string",
                "minLength": 1,
                "description": "Reserved for compatibility with the upstream pi-subagents description. NOT SUPPORTED in rpi (see ADR-0016); use {agent, task} for direct delegation."
            }
        }
    })
}

/// `resolveForegroundTimeout` (executor 2272-2289).
fn resolve_foreground_timeout(
    timeout_ms: Option<&Value>,
    max_runtime_ms: Option<&Value>,
    default_ms: Option<u64>,
) -> std::result::Result<Option<u64>, String> {
    let as_positive = |value: Option<&Value>,
                       name: &str|
     -> std::result::Result<Option<u64>, String> {
        match value {
            None => Ok(None),
            Some(Value::Number(number)) if number.is_u64() && number.as_u64().unwrap_or(0) > 0 => {
                Ok(number.as_u64())
            }
            _ => Err(format!("{name} must be a positive integer.")),
        }
    };
    let timeout = as_positive(timeout_ms, "timeoutMs")?;
    let max_runtime = as_positive(max_runtime_ms, "maxRuntimeMs")?;
    if timeout.is_none() && max_runtime.is_none() {
        return Ok(default_ms);
    }
    if let (Some(timeout), Some(max_runtime)) = (timeout, max_runtime) {
        if timeout != max_runtime {
            return Err(
                "timeoutMs and maxRuntimeMs are aliases; provide only one value or use the same value for both."
                    .to_string(),
            );
        }
    }
    Ok(timeout.or(max_runtime))
}

/// The whole tool entry point. Returns the AgentToolResult JSON.
pub fn execute_subagent_tool(
    params: &Value,
    host: &dyn HostContext,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
) -> Value {
    let outcome = execute_subagent_tool_inner(params, host, settings, config, runtime);
    outcome.to_tool_result()
}

fn execute_subagent_tool_inner(
    params: &Value,
    host: &dyn HostContext,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
) -> ToolOutcome {
    let object = params.as_object().cloned().unwrap_or_default();

    // ADR-0016: workflowScript is a schema-level placeholder that must fail
    // loudly instead of letting the model assume JS execution.
    if let Some(script) = object.get("workflowScript") {
        if script.as_str().is_some_and(|s| !s.trim().is_empty()) {
            return ToolOutcome::error(
                "workflowScript is not supported in rpi (see ADR-0016): this build has no JavaScript engine. Use { agent, task } for direct single delegation; composite workflows arrive with P1.".to_string(),
            );
        }
    }

    if let Some(action) = object.get("action").and_then(Value::as_str) {
        if !action.is_empty() {
            return crate::actions::handle_management_action(
                action,
                object.get("agent").and_then(Value::as_str),
                &host.cwd(),
                settings,
                config,
            );
        }
    }

    // P0 is foreground-only (requirements §3.1: asyncByDefault effective from
    // FR-P1-04).
    if object.get("async") == Some(&Value::Bool(true)) {
        return ToolOutcome::error(
            "async:true is not supported in this version; background runs arrive with P1 (FR-P1-04). Omit async for a foreground blocking run.".to_string(),
        );
    }

    let Some(agent_name) = object
        .get("agent")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ToolOutcome::error(
            "Provide { agent, task } for delegation, or { action } for management (list, get, status, doctor).".to_string(),
        );
    };
    let task = object
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Depth guard (executor 5726-5741).
    let config_max_depth = config.max_subagent_depth.as_ref().and_then(Value::as_u64);
    let depth = budget::check_depth(config_max_depth);
    if depth.blocked {
        return ToolOutcome::error(budget::depth_blocked_message(&depth));
    }

    let cwd_param = object.get("cwd").and_then(Value::as_str);
    let base_cwd = host.cwd();
    let effective_cwd = cwd_param
        .map(crate::paths::expand_tilde_and_resolve)
        .unwrap_or_else(|| base_cwd.clone());

    let scope = object
        .get("agentScope")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "user" | "project" | "both"))
        .unwrap_or("both");
    let agents = match discover::discover_agents(&effective_cwd, scope, settings, None) {
        Ok(agents) => agents,
        Err(error) => return ToolOutcome::error(error),
    };
    let agent = match discover::resolve_agent_name(&agents, agent_name) {
        Ok(Some(agent)) => agent.clone(),
        Ok(None) => return ToolOutcome::error(format!("Unknown agent: {agent_name}")),
        Err(message) => return ToolOutcome::error(message),
    };

    // Timeout chain: call value > agent frontmatter > config > 30min
    // (applySingleAgentLaunchDefaults + resolveSingleAgentLaunchTimeout).
    let timeout = match resolve_foreground_timeout(
        object.get("timeoutMs"),
        object.get("maxRuntimeMs"),
        agent
            .default_timeout_ms
            .or_else(|| config.resolve_default_timeout_ms())
            .or(Some(foreground::DEFAULT_FOREGROUND_TIMEOUT_MS)),
    ) {
        Ok(timeout) => timeout,
        Err(error) => return ToolOutcome::error(error),
    };

    // Context policy (executor 2165-2203): explicit param (unknown → fresh)
    // overrides the agent default.
    let context = match object.get("context").and_then(Value::as_str) {
        Some("fork") => ContextMode::Fork,
        Some("fresh") => ContextMode::Fresh,
        Some(_) => ContextMode::Fresh,
        None => match agent.default_context {
            Some(ContextMode::Fork) => ContextMode::Fork,
            _ => ContextMode::Fresh,
        },
    };

    let run_id = budget::random_run_id();
    let parent_session_file = host.parent_session_file(settings);

    // Session root (executor 5929-5966).
    let session_root = match object.get("sessionDir").and_then(Value::as_str) {
        Some(dir) => crate::paths::expand_tilde_and_resolve(dir),
        None => match &config.default_session_dir {
            Some(dir) => crate::paths::expand_tilde_and_resolve(dir),
            None => parent_session_file
                .as_ref()
                .map(|file| {
                    file.parent()
                        .unwrap_or(&effective_cwd)
                        .join(file.file_stem().unwrap_or_default())
                })
                .unwrap_or_else(mkdtemp_session_root),
        },
    };
    let session_root = if object.get("sessionDir").is_some() || config.default_session_dir.is_some()
    {
        // Explicit roots are used verbatim (no runId join — executor 5930-5931).
        session_root
    } else {
        session_root.join(&run_id)
    };

    // Fork branch (fork-context.ts).
    let (session_file, thinking_override) = (None, None);
    let (session_file, thinking_override) = if context == ContextMode::Fork {
        let branch_file = session_root.join("fork.jsonl");
        match session_fork::create_fork_session(
            parent_session_file.as_deref(),
            &branch_file,
            &effective_cwd,
        ) {
            Ok(resolution) => (Some(resolution.session_file), {
                if resolution.thinking_override_off {
                    Some("off".to_string())
                } else {
                    thinking_override
                }
            }),
            Err(error) => return ToolOutcome::error(error),
        }
    } else {
        (session_file, thinking_override)
    };
    let session_dir = if context == ContextMode::Fork {
        None
    } else {
        Some(session_root.join("run-0"))
    };

    // Model: per-run override > frontmatter > parent session model.
    let parent_model = host.parent_model();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| agent.model.clone())
        .or(parent_model);
    let thinking = match &thinking_override {
        Some(level) => Some(level.clone()),
        None => crate::launch::args::effective_thinking(&agent, None),
    };

    // Artifacts.
    let artifacts_enabled = object.get("artifacts") != Some(&Value::Bool(false));
    let preference = match ArtifactDirPreference::parse(Some(config.artifact_dir_preference())) {
        Ok(preference) => preference,
        Err(_) => ArtifactDirPreference::Project,
    };
    let artifacts_dir = artifacts_enabled.then(|| {
        artifacts::get_artifacts_dir(
            parent_session_file.as_deref(),
            Some(&effective_cwd),
            preference,
        )
    });

    // Output path: per-run output (string) or agent frontmatter output.
    let output_path = match object.get("output") {
        Some(Value::Bool(false)) => None,
        Some(Value::String(path)) if !path.trim().is_empty() => {
            Some(crate::paths::expand_tilde_and_resolve(path))
        }
        _ => agent
            .output
            .as_deref()
            .map(crate::paths::expand_tilde_and_resolve),
    };

    // Spawn cap (P0 counting form).
    let max_spawns = budget::resolve_max_spawns_per_run(
        config
            .max_subagent_spawns_per_run
            .as_ref()
            .and_then(Value::as_u64),
    );
    {
        let mut memory = FOREGROUND_RUN_MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let count = memory.spawns_by_run.entry(run_id.clone()).or_insert(0);
        *count += 1;
        if *count > max_spawns {
            return ToolOutcome::error(format!(
                "Run fan-out budget exceeded: {max_spawns} subagent spawns per run."
            ));
        }
    }

    // Fanout authorization: only when the explicit allowlist names `subagent`.
    let fanout_authorized = agent
        .tools
        .as_ref()
        .map(|tools| tools.iter().any(|t| t == "subagent"))
        .unwrap_or(false);
    let self_extension = crate::launch::binary::resolve_self_extension_path()
        .map(|p| p.to_string_lossy().to_string());
    if fanout_authorized && self_extension.is_none() {
        return ToolOutcome::error(
            "This agent authorizes nested subagents (tools includes \"subagent\") but the subagents extension library path could not be resolved for child injection. Set RPI_SUBAGENT_EXTENSION_PATH to the installed librpi_ext_subagents shared library.".to_string(),
        );
    }

    let child_max_depth = budget::resolve_child_max_depth(
        budget::resolve_current_max_depth(config_max_depth),
        agent.max_subagent_depth,
    );

    // Fork task preamble (executor 4119-4122).
    let task_text = if context == ContextMode::Fork {
        session_fork::wrap_fork_task(&task, None)
    } else {
        task.clone()
    };

    let input = ForegroundRunInput {
        agent_name: agent.name.clone(),
        agent_system_prompt: agent.system_prompt.clone(),
        agent_system_prompt_mode: agent.system_prompt_mode,
        agent_tools: agent.tools.clone(),
        agent_extensions: agent.extensions.clone(),
        agent_subagent_only_extensions: agent.subagent_only_extensions.clone(),
        agent_inherit_project_context: agent.inherit_project_context,
        agent_inherit_skills: agent.inherit_skills,
        task: task_text,
        task_delivery: None,
        cwd: effective_cwd.clone(),
        session_dir,
        session_file: session_file.clone(),
        model,
        thinking,
        run_id: run_id.clone(),
        timeout_ms: timeout,
        child_max_subagent_depth: child_max_depth,
        artifacts_dir: artifacts_dir.clone(),
        include_jsonl: false,
        include_transcript: true,
        parent_session_id: parent_session_file.as_ref().map(|p| {
            p.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        }),
        self_extension,
        fanout_authorized,
    };

    let result = runtime.block_on(foreground::run_foreground(&input));

    // Output file (finalizeSingleOutput P0 subset): write the full output to
    // the declared path on success.
    let mut saved_output_path = None;
    if let Some(output_path) = &output_path {
        if result.exit_code == 0
            && !result.final_output.trim().is_empty()
            && artifacts::write_artifact(output_path, &result.final_output).is_ok()
        {
            saved_output_path = Some(output_path.clone());
        }
    }

    // Details assembly (types.ts:1014-1115 + compactForegroundResult).
    let mut single = json!({
        "index": 0,
        "agent": agent.name,
        "task": "[prompt redacted]",
        "context": context.as_str(),
        "exitCode": result.exit_code,
        "usage": result.usage,
        "timedOut": result.timed_out,
    });
    if let Some(signal) = &result.process_signal {
        single["processSignal"] = json!(signal);
    }
    if let Some(model) = &result.model {
        single["model"] = json!(model);
    }
    if let Some(thinking) = &result.thinking {
        single["thinking"] = json!(thinking);
    }
    if let Some(error) = &result.error {
        single["error"] = json!(error);
    }
    if let Some(session_file) = &result.session_file {
        single["sessionFile"] = json!(session_file.to_string_lossy());
    }
    if let Some(paths) = &result.artifact_paths {
        single["artifactPaths"] = paths.to_json();
    }
    if let Some(truncation) = &result.truncation {
        single["truncation"] = truncation.clone();
    }
    single["finalOutput"] = json!(result.final_output);
    if let Some(saved) = &saved_output_path {
        single["savedOutputPath"] = json!(saved.to_string_lossy());
    }

    let mut details = json!({
        "mode": "single",
        "runId": run_id,
        "results": [single],
    });
    if let Some(timeout) = timeout {
        details["timeoutMs"] = json!(timeout);
    }
    if let Some(dir) = &artifacts_dir {
        if let Some(paths) = &result.artifact_paths {
            details["artifacts"] = json!({
                "dir": dir.to_string_lossy(),
                "files": [paths.to_json()],
            });
        }
    }
    details["totalChildUsage"] = result.usage.clone();
    details["totalCost"] = json!(result
        .usage
        .get("cost")
        .and_then(Value::as_f64)
        .unwrap_or(0.0));

    {
        let mut memory = FOREGROUND_RUN_MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        memory.runs.push(ForegroundRunRecord {
            run_id: run_id.clone(),
            agent: agent.name.clone(),
            exit_code: result.exit_code,
            duration_ms: result.duration_ms,
            error: result.error.clone(),
        });
    }

    // Result text (executor 4333-4349).
    if result.exit_code != 0 {
        let text = format_failed_single_run_output(
            &result.error,
            &result.final_output,
            result.artifact_paths.as_ref(),
        );
        return ToolOutcome {
            text,
            details,
            is_error: true,
        };
    }
    let text = if result.final_output.trim().is_empty() {
        "(no output)".to_string()
    } else {
        result.final_output.clone()
    };
    ToolOutcome {
        text,
        details,
        is_error: false,
    }
}

/// `formatFailedSingleRunOutput` (executor 1846-1857).
fn format_failed_single_run_output(
    error: &Option<String>,
    output: &str,
    artifact_paths: Option<&artifacts::ArtifactPaths>,
) -> String {
    let error = error.clone().unwrap_or_else(|| "Failed".to_string());
    let output = output.trim();
    let mut lines = vec![error.clone()];
    if !output.is_empty() && output != error.trim() {
        lines.push(String::new());
        lines.push("Output:".to_string());
        lines.push(output.to_string());
    }
    if let Some(paths) = artifact_paths {
        if paths.output_path.exists() {
            lines.push(String::new());
            lines.push(format!(
                "Output artifact: {}",
                paths.output_path.to_string_lossy()
            ));
        }
    }
    lines.join("\n")
}

/// `getSubagentSessionRoot` fallback (extension/index.ts:224-231): no parent
/// session → a fresh temp directory.
fn mkdtemp_session_root() -> PathBuf {
    let base = crate::paths::temp_dir().join("rpi-subagent-session-");
    let base = base.to_string_lossy().to_string();
    for _ in 0..32 {
        let candidate = format!("{base}{}", budget::random_run_id());
        let path = PathBuf::from(&candidate);
        if std::fs::create_dir(&path).is_ok() {
            return path;
        }
    }
    PathBuf::from(format!("{base}{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_resolution_rules() {
        // aliases must match
        assert_eq!(
            resolve_foreground_timeout(None, None, Some(500)).unwrap(),
            Some(500)
        );
        assert_eq!(resolve_foreground_timeout(None, None, None).unwrap(), None);
        assert_eq!(
            resolve_foreground_timeout(Some(&json!(100)), None, Some(500)).unwrap(),
            Some(100)
        );
        assert_eq!(
            resolve_foreground_timeout(None, Some(&json!(100)), None).unwrap(),
            Some(100)
        );
        let err =
            resolve_foreground_timeout(Some(&json!(100)), Some(&json!(200)), None).unwrap_err();
        assert!(err.contains("aliases"));
        let err = resolve_foreground_timeout(Some(&json!(0)), None, None).unwrap_err();
        assert_eq!(err, "timeoutMs must be a positive integer.");
    }
}
