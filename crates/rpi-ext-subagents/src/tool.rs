//! `subagent` tool: parameter schema, validation, single foreground
//! delegation dispatch and result assembly (FR-P0-01).

use std::sync::Mutex;

use serde_json::{json, Value};

use crate::agents::discover;
use crate::artifacts;
use crate::config::SettingsPair;
use crate::runner::budget;
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
                "description": "Run in the background (FR-P1-04): returns a receipt immediately; completion arrives as a session message. Default: true for tasks/steps, config.asyncByDefault for single runs; pass false for a foreground blocking run."
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
                "description": "Per-run model override (provider/id); children without their own model inherit it."
            },
            "thinking": {
                "type": "string",
                "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                "description": "Per-run thinking level override; children without their own level inherit it."
            },
            "tasks": {
                "type": "array",
                "minItems": 1,
                "description": "Parallel composition (ADR-0018): each item {key, agent, task, model?, thinking?, context?, cwd?, output?, worktree?, timeoutMs?, skill?} runs as one child; bounded by parallel.concurrency, failures are isolated, results aggregate in order."
            },
            "steps": {
                "type": "array",
                "minItems": 1,
                "description": "Chain composition (ADR-0018): sequential children; task templates may interpolate {task} (original), {previous} (prior step output), {outputs.<name>} (bound step output) and {chain_dir}; per-step output/reads/skill/progress overrides; a failed step stops the chain and returns completed steps."
            },
            "concurrency": {
                "type": "integer",
                "minimum": 1,
                "description": "Parallel concurrency cap for tasks; overrides parallel.concurrency (default 4)."
            },
            "gate": {
                "type": "string",
                "minLength": 1,
                "description": "Acceptance gate shorthand (FR-P1-09): the command runs host-side after the child; a failing gate fails the run. Equivalent to acceptance {level: verified, verify: [the command]}."
            },
            "turnBudget": {
                "type": "object",
                "description": "Turn budget {maxTurns, graceTurns=1} (FR-P1-09): injected into child instructions; children wrap up before the cap."
            },
            "toolBudget": {
                "type": "object",
                "description": "Tool budget {soft?, hard, block?} (FR-P1-09): soft warns, hard caps tool calls."
            },
            "usageBudget": {
                "type": "object",
                "description": "Usage budget {tokens?{soft,hard}, costUsd?{soft,hard}} (FR-P1-09): exhausted budgets skip further launches in composite runs."
            },
            "foregroundOnly": {
                "type": "boolean",
                "description": "Never force this run to background, even with forceTopLevelAsync."
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

/// The whole tool entry point. Returns the AgentToolResult JSON.
/// `tool_call_id` is the in-flight dispatch id (ADR-0015): present → the
/// run installs a `toolUpdate` streaming seam; None (test seams) keeps the
/// run non-streaming.
pub fn execute_subagent_tool(
    params: &Value,
    host: &dyn HostContext,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
    tool_call_id: Option<&str>,
) -> Value {
    let outcome =
        execute_subagent_tool_inner(params, host, settings, config, runtime, tool_call_id);
    outcome.to_tool_result()
}

fn execute_subagent_tool_inner(
    params: &Value,
    host: &dyn HostContext,
    settings: &SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
    tool_call_id: Option<&str>,
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
            let deps = crate::actions::ActionDeps {
                host: Some(host),
                runtime: Some(runtime),
                params: Some(params.clone()),
            };
            return crate::actions::handle_management_action_with(
                action,
                object.get("agent").and_then(Value::as_str),
                &host.cwd(),
                settings,
                config,
                &deps,
            );
        }
    }

    // Composite entries (FR-P1-01/02/03, ADR-0018): `tasks` (parallel) and
    // `steps` (chain) are mutually exclusive and do not combine with a
    // top-level single delegation.
    let has_tasks = object.get("tasks").is_some();
    let has_steps = object.get("steps").is_some();
    let has_single = object
        .get("agent")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if (has_tasks as u8) + (has_steps as u8) + (has_single as u8) > 1 {
        return ToolOutcome::error(
            "Use one of { tasks }, { steps }, or a single { agent, task } — they cannot be combined.".to_string(),
        );
    }

    // Async resolution (FR-P1-04): explicit param > composite default
    // (async) > config.asyncByDefault (default true). `forceTopLevelAsync`
    // forces depth-0 single runs unless `foregroundOnly` (top-level-async.ts).
    let mut wants_async = match object.get("async") {
        Some(Value::Bool(flag)) => *flag,
        _ if has_tasks || has_steps => true,
        _ => config.resolve_async_by_default(),
    };
    let foreground_only = object
        .get("foregroundOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let depth_check =
        budget::check_depth(config.max_subagent_depth.as_ref().and_then(Value::as_u64));
    if !wants_async
        && !foreground_only
        && depth_check.depth == 0
        && config.force_top_level_async == Some(true)
    {
        wants_async = true;
    }

    // Timeout aliases must agree before any execution path
    // (resolveForegroundTimeout, executor 2272-2289).
    if let Err(error) = check_timeout_aliases(&object) {
        return ToolOutcome::error(error);
    }

    if !has_tasks && !has_steps && !has_single {
        return ToolOutcome::error(
            "Provide { agent, task } for delegation, { tasks } or { steps } for composition (see ADR-0018), or { action } for management (list, get, status, doctor).".to_string(),
        );
    }

    // Depth guard (executor 5726-5741).
    let config_max_depth = config.max_subagent_depth.as_ref().and_then(Value::as_u64);
    let depth = budget::check_depth(config_max_depth);
    if depth.blocked {
        return ToolOutcome::error(budget::depth_blocked_message(&depth));
    }

    let scope = object
        .get("agentScope")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "user" | "project" | "both"))
        .unwrap_or("both");

    // Shared per-call context (cwd, session root, artifacts, model registry).
    let mut ctx = crate::p1::launch_child::RunCtx::from_host(host, &object, config.clone());
    // TE09 FR-A: install the streaming seam under a real dispatch id. The
    // composite paths decide per shape below (single/chain stream, parallel
    // does not — upstream wraps only those).
    ctx.frame_sink = tool_call_id.and_then(|id| host.tool_update_sink(id));
    let agents = match ctx.discover(scope) {
        Ok(agents) => agents,
        Err(error) => return ToolOutcome::error(error),
    };

    if wants_async {
        return dispatch_async(&object, &agents, &ctx, host, runtime, has_tasks, has_steps);
    }
    if has_tasks {
        return dispatch_tasks(&object, &agents, &ctx, runtime);
    }
    if has_steps {
        return dispatch_steps(&object, &agents, &ctx, runtime);
    }

    let mut spec = crate::p1::launch_child::ChildSpec::from_params(&object);
    // Top-level `gate` shorthand (FR-P1-09): verified acceptance for the run.
    if let Some(gate) = object
        .get("gate")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        if spec.gate.is_none() {
            spec.gate = Some(gate.to_string());
        }
        let _ = crate::p1::acceptance::normalize_gate(gate);
    }
    let agent = match discover::resolve_agent_name(&agents, &spec.agent_name) {
        Ok(Some(agent)) => agent.clone(),
        Ok(None) => return ToolOutcome::error(format!("Unknown agent: {}", spec.agent_name)),
        Err(message) => return ToolOutcome::error(message),
    };

    let outcome = match crate::p1::launch_child::run_child(&spec, &agent, &ctx, runtime) {
        Ok(outcome) => outcome,
        Err(error) => return ToolOutcome::error(error),
    };
    let result = &outcome.result;

    let details = assemble_single_details(&ctx, &spec, &agent, &outcome);

    {
        let mut memory = FOREGROUND_RUN_MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        memory.runs.push(ForegroundRunRecord {
            run_id: ctx.run_id.clone(),
            agent: outcome.agent_name.clone(),
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

/// Timeout alias agreement (`resolveForegroundTimeout` alias rule).
fn check_timeout_aliases(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    let positive = |value: Option<&Value>, name: &str| -> Result<Option<u64>, String> {
        match value {
            None => Ok(None),
            Some(Value::Number(n)) if n.is_u64() && n.as_u64().unwrap_or(0) > 0 => Ok(n.as_u64()),
            _ => Err(format!("{name} must be a positive integer.")),
        }
    };
    let timeout = positive(object.get("timeoutMs"), "timeoutMs")?;
    let max_runtime = positive(object.get("maxRuntimeMs"), "maxRuntimeMs")?;
    if let (Some(timeout), Some(max_runtime)) = (timeout, max_runtime) {
        if timeout != max_runtime {
            return Err(
                "timeoutMs and maxRuntimeMs are aliases; provide only one value or use the same value for both."
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// Single-run `details` assembly (types.ts:1014-1115 + compactForegroundResult).
fn assemble_single_details(
    ctx: &crate::p1::launch_child::RunCtx,
    spec: &crate::p1::launch_child::ChildSpec,
    agent: &discover::AgentConfig,
    outcome: &crate::p1::launch_child::ChildOutcome,
) -> Value {
    let run_id = &ctx.run_id;
    let result = &outcome.result;
    let mut single = json!({
        "index": 0,
        "agent": outcome.agent_name,
        "task": "[prompt redacted]",
        "context": outcome.context.as_str(),
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
    if result.attempted_models.len() > 1 {
        single["attemptedModels"] = json!(result.attempted_models);
    }
    if let Some(error) = &result.error {
        single["error"] = json!(error);
    }
    // Acceptance ledger (FR-P1-09): inferred level + gate outcome.
    {
        let mut ledger_map = crate::p1::launch_child::GATE_LEDGER
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ledger) = ledger_map.remove(&(ctx.run_id.clone(), 0)) {
            single["acceptance"] = ledger;
        }
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
    if let Some(saved) = &outcome.saved_output_path {
        single["savedOutputPath"] = json!(saved.to_string_lossy());
    }
    let mut details = json!({
        "mode": "single",
        "runId": run_id,
        "results": [single],
    });
    let timeout = spec
        .timeout_ms
        .or(ctx.top_timeout_ms)
        .or(agent.default_timeout_ms)
        .or_else(|| ctx.config.resolve_default_timeout_ms())
        .or(Some(
            crate::runner::foreground::DEFAULT_FOREGROUND_TIMEOUT_MS,
        ));
    if let Some(timeout) = timeout {
        details["timeoutMs"] = json!(timeout);
    }
    if let Some(dir) = &ctx.artifacts_dir {
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
    details
}

/// Per-task worktree enablement (FR-P1-06): each task takes its own
/// `worktree` override over the top-level default; `None` when nothing ends
/// up enabled (no plan, no repo probe).
fn worktree_enabled_flags(
    entries: &[crate::p1::parallel::TaskEntry],
    top_worktree: bool,
) -> Option<Vec<bool>> {
    let enabled: Vec<bool> = entries
        .iter()
        .map(|entry| entry.worktree_override.unwrap_or(top_worktree))
        .collect();
    enabled.iter().any(|e| *e).then_some(enabled)
}

/// Build the worktree plan for a parallel batch when any task opts in
/// (top-level `worktree: true` or per-task `worktree: true`).
fn build_worktree_plan(
    entries: &[crate::p1::parallel::TaskEntry],
    object: &serde_json::Map<String, Value>,
    ctx: &crate::p1::launch_child::RunCtx,
) -> Result<Option<std::sync::Arc<crate::p1::parallel::WorktreePlan>>, String> {
    let top_worktree = object
        .get("worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(enabled) = worktree_enabled_flags(entries, top_worktree) else {
        return Ok(None);
    };
    let (toplevel, base_commit) = crate::p1::worktree::resolve_repo_base(&ctx.base_cwd)?;
    let base_dir = crate::p1::worktree::resolve_worktree_base_dir(&ctx.config, &toplevel)?;
    Ok(Some(std::sync::Arc::new(
        crate::p1::parallel::WorktreePlan {
            toplevel,
            base_commit,
            base_dir,
            enabled,
            config: ctx.config.clone(),
            manifest_path: None,
            diffs: std::sync::Mutex::new(Vec::new()),
            kept: std::sync::Mutex::new(Vec::new()),
        },
    )))
}

/// `tasks` composition dispatch (FR-P1-01, ADR-0018).
fn dispatch_tasks(
    object: &serde_json::Map<String, Value>,
    agents: &[discover::AgentConfig],
    ctx: &crate::p1::launch_child::RunCtx,
    runtime: &crate::PluginRuntime,
) -> ToolOutcome {
    // Parallel batches never stream (upstream has no onUpdate wrapper on
    // the parallel composition — TE09 Out list); drop any dispatch sink.
    let mut ctx = ctx.clone();
    ctx.frame_sink = None;
    let ctx = &ctx;
    let max_tasks = ctx.config.parallel_max_tasks();
    let entries = match crate::p1::parallel::parse_tasks(
        object.get("tasks").unwrap_or(&Value::Null),
        max_tasks,
    ) {
        Ok(entries) => entries,
        Err(error) => return ToolOutcome::error(error),
    };
    let concurrency = ctx.config.parallel_concurrency(object.get("concurrency")) as usize;
    // Worktree isolation (FR-P1-06): top-level `worktree: true` defaults
    // every task in (per-task `worktree: false` opts out) and a per-task
    // `worktree: true` opts in on its own — `build_worktree_plan` returns
    // None when no task ends up enabled.
    let worktree_plan = match build_worktree_plan(&entries, object, ctx) {
        Ok(plan) => plan,
        Err(error) => return ToolOutcome::error(error),
    };
    let outcomes = match crate::p1::parallel::run_parallel(
        &entries,
        agents,
        ctx,
        runtime,
        concurrency,
        worktree_plan.clone(),
    ) {
        Ok(outcomes) => outcomes,
        Err(error) => return ToolOutcome::error(error),
    };
    if let Some(plan) = &worktree_plan {
        crate::p1::parallel::finalize_worktree_handoff(plan, &ctx.run_id, &ctx.base_cwd);
    }
    record_runs(
        &outcomes
            .iter()
            .map(|o| (o.agent.clone(), o.exit_code, 0, o.error.clone()))
            .collect::<Vec<_>>(),
        &ctx.run_id,
    );

    let any_failed = outcomes.iter().any(|o| o.exit_code != 0);
    let text = crate::p1::parallel::aggregate_parallel_outputs(&outcomes);
    let total_usage = sum_usage(&outcomes);
    let details = json!({
        "mode": "parallel",
        "runId": ctx.run_id,
        "results": outcomes.iter().map(|o| o.details.clone()).collect::<Vec<_>>(),
        "totalChildUsage": total_usage.clone(),
        "totalCost": total_usage["cost"].as_f64().unwrap_or(0.0),
    });
    ToolOutcome {
        text,
        details,
        is_error: any_failed,
    }
}

/// `steps` composition dispatch (FR-P1-02, ADR-0018).
fn dispatch_steps(
    object: &serde_json::Map<String, Value>,
    agents: &[discover::AgentConfig],
    ctx: &crate::p1::launch_child::RunCtx,
    runtime: &crate::PluginRuntime,
) -> ToolOutcome {
    let steps = match crate::p1::chain::parse_steps(object.get("steps").unwrap_or(&Value::Null)) {
        Ok(steps) => steps,
        Err(error) => return ToolOutcome::error(error),
    };
    let original_task = object
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (completed, failed) =
        match crate::p1::chain::run_chain(&steps, agents, ctx, runtime, &original_task) {
            Ok(result) => result,
            Err(error) => return ToolOutcome::error(error),
        };
    record_runs(
        &completed
            .iter()
            .map(|s| (s.agent.clone(), s.exit_code, 0, s.error.clone()))
            .collect::<Vec<_>>(),
        &ctx.run_id,
    );

    // Chain output: the last completed step's output (or the failure text).
    let (text, is_error) = match (&completed.last(), &failed) {
        (Some(last), None) => (last.output.clone(), false),
        (_, Some(failure)) => {
            let error = failure
                .error
                .clone()
                .unwrap_or_else(|| "Chain step failed".to_string());
            let mut lines = vec![format!(
                "Chain failed at step {} ({}): {error}",
                failure.index + 1,
                failure.agent
            )];
            for step in &completed {
                lines.push(format!(
                    "\n=== Step {} ({}) ===\n{}",
                    step.index + 1,
                    step.agent,
                    step.output
                ));
            }
            (lines.join("\n"), true)
        }
        (None, None) => ("(no output)".to_string(), false),
    };
    let mut results: Vec<Value> = completed.iter().map(|s| s.details.clone()).collect();
    if let Some(failure) = &failed {
        results.push(failure.details.clone());
    }
    let details = json!({
        "mode": "chain",
        "runId": ctx.run_id,
        "results": results,
        "chainStepCount": steps.len(),
    });
    ToolOutcome {
        text,
        details,
        is_error,
    }
}

/// Async (background) dispatch (FR-P1-04): budget preflight, run registration,
/// spawn the runner task, return the receipt immediately.
fn dispatch_async(
    object: &serde_json::Map<String, Value>,
    agents: &[discover::AgentConfig],
    ctx: &crate::p1::launch_child::RunCtx,
    host: &dyn HostContext,
    runtime: &crate::PluginRuntime,
    has_tasks: bool,
    has_steps: bool,
) -> ToolOutcome {
    let body = if has_tasks {
        let entries = match crate::p1::parallel::parse_tasks(
            object.get("tasks").unwrap_or(&Value::Null),
            ctx.config.parallel_max_tasks(),
        ) {
            Ok(entries) => entries,
            Err(error) => return ToolOutcome::error(error),
        };
        let concurrency = ctx.config.parallel_concurrency(object.get("concurrency")) as usize;
        // Same worktree opt-in as the foreground path (FR-P1-06): top-level
        // or per-task `worktree: true`.
        let worktree_plan = match build_worktree_plan(&entries, object, ctx) {
            Ok(plan) => plan,
            Err(error) => return ToolOutcome::error(error),
        };
        crate::runner::background::AsyncBody::Tasks {
            entries,
            concurrency,
            worktree_plan,
        }
    } else if has_steps {
        let steps = match crate::p1::chain::parse_steps(object.get("steps").unwrap_or(&Value::Null))
        {
            Ok(steps) => steps,
            Err(error) => return ToolOutcome::error(error),
        };
        let original_task = object
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        crate::runner::background::AsyncBody::Steps {
            steps,
            original_task,
        }
    } else {
        let spec = crate::p1::launch_child::ChildSpec::from_params(object);
        match discover::resolve_agent_name(agents, &spec.agent_name) {
            Ok(Some(_)) => {}
            Ok(None) => return ToolOutcome::error(format!("Unknown agent: {}", spec.agent_name)),
            Err(message) => return ToolOutcome::error(message),
        }
        crate::runner::background::AsyncBody::Single {
            spec: Box::new(spec),
        }
    };

    let session_id = ctx.parent_session_file.as_ref().map(|p| {
        p.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let planned = match &body {
        crate::runner::background::AsyncBody::Single { .. } => 1,
        crate::runner::background::AsyncBody::Tasks { entries, .. } => entries.len() as u64,
        crate::runner::background::AsyncBody::Steps { steps, .. } => steps.len() as u64,
    };

    // Budget preflight (ADR-0019 §4): session spawn ledger first (releases on
    // nothing — spawns are cumulative), then the active-async capacity slot.
    let spawn_ledger = crate::runner::background::SpawnBudgetLedger::open(
        session_id.as_deref().unwrap_or("no-session"),
    );
    if let Err(error) = spawn_ledger.reserve(planned, ctx.config.max_subagent_spawns_per_session())
    {
        return ToolOutcome::error(error);
    }
    let capacity = crate::runner::background::ActiveAsyncCapacity::open(
        session_id.as_deref().unwrap_or("no-session"),
    );
    let limit = ctx
        .config
        .max_active_async_runs_per_session()
        .unwrap_or(u64::MAX);
    let slot = match capacity.acquire(&ctx.run_id, limit) {
        Ok(slot) => slot,
        Err(error) => return ToolOutcome::error(error),
    };

    let handle = crate::runner::background::start_run(&ctx.run_id, session_id.as_deref(), &body);
    let notify = crate::runner::background::AsyncNotify {
        calls: host.async_calls(),
    };
    let drive_ctx = clone_ctx_for_async(ctx);
    let drive_handle = handle.clone();
    runtime.spawn(crate::runner::background::drive_run_and_release(
        drive_handle,
        drive_ctx,
        body,
        notify,
        session_id,
        slot,
    ));

    let receipt = crate::runner::background::receipt(&ctx.run_id, &handle.run_dir);
    ToolOutcome {
        text: format!(
            "Background run {} started ({}); completion will arrive as a session message. Use subagent({{action:\"status\"}}) to inspect.",
            ctx.run_id,
            receipt["status"].as_str().unwrap_or("running")
        ),
        details: receipt,
        is_error: false,
    }
}

/// `RunCtx` clone for the driver task. The streaming seam is dispatch-owned
/// (it dies with the toolExecute call); the async run never frames.
fn clone_ctx_for_async(ctx: &crate::p1::launch_child::RunCtx) -> crate::p1::launch_child::RunCtx {
    let mut clone = ctx.clone();
    clone.frame_sink = None;
    clone
}

fn record_runs(records: &[(String, i32, u64, Option<String>)], run_id: &str) {
    let mut memory = FOREGROUND_RUN_MEMORY
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (agent, exit_code, duration_ms, error) in records {
        memory.runs.push(ForegroundRunRecord {
            run_id: run_id.to_string(),
            agent: agent.clone(),
            exit_code: *exit_code,
            duration_ms: *duration_ms,
            error: error.clone(),
        });
    }
}

fn sum_usage(outcomes: &[crate::p1::parallel::ParallelTaskOutcome]) -> Value {
    // Aggregate over the per-child usage blocks, keeping the single-run
    // usage shape (upstream totalChildUsage): token counters and cost sum;
    // a missing block contributes nothing.
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut cost = 0.0f64;
    for outcome in outcomes {
        let usage = outcome.details["usage"].clone();
        input += usage["input"].as_u64().unwrap_or(0);
        output += usage["output"].as_u64().unwrap_or(0);
        cache_read += usage["cacheRead"].as_u64().unwrap_or(0);
        cache_write += usage["cacheWrite"].as_u64().unwrap_or(0);
        cost += usage["cost"].as_f64().unwrap_or(0.0);
    }
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "cost": cost,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_alias_rules() {
        let parse = |raw: &str| {
            let value: Value = serde_json::from_str(raw).unwrap();
            check_timeout_aliases(value.as_object().unwrap())
        };
        assert!(parse(r#"{"timeoutMs":100,"maxRuntimeMs":100}"#).is_ok());
        assert!(parse(r#"{"timeoutMs":100}"#).is_ok());
        assert!(parse(r#"{"maxRuntimeMs":100}"#).is_ok());
        let err = parse(r#"{"timeoutMs":100,"maxRuntimeMs":200}"#).unwrap_err();
        assert!(err.contains("aliases"));
        assert_eq!(
            parse(r#"{"timeoutMs":0}"#).unwrap_err(),
            "timeoutMs must be a positive integer."
        );
    }

    #[test]
    fn worktree_flags_per_task_opt_in() {
        let entries = |raw: &str| {
            crate::p1::parallel::parse_tasks(&serde_json::from_str::<Value>(raw).unwrap(), 32)
                .unwrap()
        };
        // No top-level flag, no per-task flags → no plan.
        assert_eq!(
            worktree_enabled_flags(
                &entries(r#"[{"agent":"worker","task":"a"},{"agent":"worker","task":"b"}]"#),
                false
            ),
            None
        );
        // Top-level true defaults every task in; per-task false opts out.
        assert_eq!(
            worktree_enabled_flags(
                &entries(
                    r#"[{"agent":"worker","task":"a"},{"agent":"worker","task":"b","worktree":false}]"#
                ),
                true
            ),
            Some(vec![true, false])
        );
        // Per-task true opts in on its own (FR-P1-06 child-level switch).
        assert_eq!(
            worktree_enabled_flags(
                &entries(
                    r#"[{"agent":"worker","task":"a"},{"agent":"worker","task":"b","worktree":true}]"#
                ),
                false
            ),
            Some(vec![false, true])
        );
    }
}
