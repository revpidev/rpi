//! Parallel fan-out runs (FR-P1-01 / FR-P1-03): one `tasks: [...]` call
//! expands bounded-concurrency children, isolates per-task failures, and
//! aggregates results in submission order.
//!
//! Port of pi-subagents `src/runs/shared/parallel-utils.ts` (`mapConcurrent`,
//! `aggregateParallelOutputs`, MAX_PARALLEL_CONCURRENCY) and the workflow
//! `runs.all` admission semantics (scripted-workflow.ts:178-194: batch
//! admitted at once, each child collects failure instead of rejecting).

use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::agents::discover::{self, AgentConfig};
use crate::p1::launch_child::{self, ChildOutcome, ChildSpec, OutputOverride, RunCtx};
use crate::PluginRuntime;

/// Key pattern shared with the workflow sandbox (`KEY_PATTERN`,
/// scripted-workflow.ts:3).
pub const KEY_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._-]{0,127}";

/// Fields a task entry may not carry (upstream `validateRunCall`
/// scripted-workflow.ts:152-171 — one child per entry, no nested composition).
const FORBIDDEN_ENTRY_FIELDS: [&str; 7] = [
    "action",
    "workflowScript",
    "tasks",
    "steps",
    "parallel",
    "concurrency",
    "chainDir",
];

#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub key: String,
    pub spec: ChildSpec,
    /// Per-task `worktree` override over the top-level default (FR-P1-06).
    pub worktree_override: Option<bool>,
}

/// Parse and validate the `tasks` array (workflow `runs.all` items +
/// `resolveTopLevelParallelMaxTasks` cap).
pub fn parse_tasks(tasks: &Value, max_tasks: u64) -> Result<Vec<TaskEntry>, String> {
    let Some(items) = tasks.as_array() else {
        return Err("tasks must be an array of task objects.".to_string());
    };
    if items.is_empty() {
        return Err("tasks must contain at least one task.".to_string());
    }
    if items.len() as u64 > max_tasks {
        return Err(format!(
            "Parallel run exceeded the task limit: {} tasks requested, max {max_tasks} (parallel.maxTasks).",
            items.len()
        ));
    }
    let mut entries = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!("tasks[{index}] must be an object."));
        };
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("task-{index}"));
        if !valid_key(&key) {
            return Err(format!(
                "Invalid task key '{key}': keys must match {KEY_PATTERN}."
            ));
        }
        if !seen_keys.insert(key.clone()) {
            return Err(format!(
                "Duplicate task key '{key}': task keys must be unique."
            ));
        }
        for field in FORBIDDEN_ENTRY_FIELDS {
            if object.contains_key(field) {
                return Err(format!(
                    "tasks[{index}].{field} is not allowed inside a task entry."
                ));
            }
        }
        let agent_name = object
            .get("agent")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("tasks[{index}] is missing a non-empty agent."))?;
        let task = object
            .get("task")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("tasks[{index}] is missing a non-empty task."))?;
        let output = match object.get("output") {
            Some(Value::Bool(false)) => OutputOverride::Disabled,
            Some(Value::String(path)) if !path.trim().is_empty() => {
                OutputOverride::Path(crate::paths::expand_tilde_and_resolve(path))
            }
            _ => OutputOverride::Inherit,
        };
        let worktree_override = object.get("worktree").and_then(Value::as_bool);
        entries.push(TaskEntry {
            key,
            worktree_override,
            spec: ChildSpec {
                agent_name: agent_name.to_string(),
                task: task.to_string(),
                model: str_field(object, "model"),
                thinking: str_field(object, "thinking"),
                context: context_field(object),
                cwd: object
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(crate::paths::expand_tilde_and_resolve),
                output,
                timeout_ms: positive_u64(object.get("timeoutMs"))
                    .or_else(|| positive_u64(object.get("maxRuntimeMs"))),
                child_index: index as u32,
                skills: string_list(object.get("skill"))
                    .or_else(|| string_list(object.get("skills"))),
                gate: object
                    .get("gate")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
                turn_budget: None,
                tool_budget: None,
                session_file: None,
                steer_inbox: None,
                skill_fallback_cwd: None,
                skill_primary_cwd: None,
            },
        });
    }
    Ok(entries)
}

/// `KEY_PATTERN.test` (scripted-workflow.ts:3).
pub fn valid_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 128 {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap_or_default();
    if !(first.is_ascii_alphanumeric()) {
        return false;
    }
    key.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn str_field(object: &serde_json::Map<String, Value>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn context_field(object: &serde_json::Map<String, Value>) -> Option<discover::ContextMode> {
    match object.get("context").and_then(Value::as_str) {
        Some("fork") => Some(discover::ContextMode::Fork),
        Some("fresh") => Some(discover::ContextMode::Fresh),
        Some(_) => Some(discover::ContextMode::Fresh),
        None => None,
    }
}

fn positive_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(n)) if n.is_u64() && n.as_u64().unwrap_or(0) > 0 => n.as_u64(),
        _ => None,
    }
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        Some(Value::String(raw)) => {
            let items: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Some(items)
        }
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

/// Worktree plan for a parallel batch (FR-P1-06): children whose index is
/// enabled get an isolated worktree (cwd redirect), a captured patch after
/// the run, a handoff manifest, and rollback-safe cleanup.
pub struct WorktreePlan {
    pub toplevel: std::path::PathBuf,
    pub base_commit: String,
    pub base_dir: std::path::PathBuf,
    pub enabled: Vec<bool>,
    pub config: crate::config::ExtensionConfig,
    /// Patches + manifest land beside the base dir (upstream handoffs/).
    pub manifest_path: Option<std::path::PathBuf>,
    pub diffs: std::sync::Mutex<Vec<(usize, String, String, crate::p1::worktree::WorktreeDiff)>>,
    /// Worktrees kept dirty (patch capture failed) — `finalize` retries
    /// their cleanup once the handoff manifest exists.
    pub kept: std::sync::Mutex<Vec<crate::p1::worktree::WorktreeInfo>>,
}

/// One aggregated task result (`ParallelTaskResult`).
#[derive(Debug, Clone)]
pub struct ParallelTaskOutcome {
    pub agent: String,
    pub output: String,
    pub exit_code: i32,
    pub error: Option<String>,
    pub timed_out: bool,
    pub output_target_path: Option<std::path::PathBuf>,
    pub output_target_exists: bool,
    pub details: Value,
}

/// Run the task batch with bounded concurrency (`mapConcurrent`,
/// parallel-utils.ts:167-198): worker-pool shape, results written back to
/// their submission index, one failure never aborts the others
/// (`collectFailure` semantics).
pub fn run_parallel(
    entries: &[TaskEntry],
    agents: &[AgentConfig],
    ctx: &RunCtx,
    runtime: &PluginRuntime,
    concurrency: usize,
    worktree: Option<std::sync::Arc<WorktreePlan>>,
) -> Result<Vec<ParallelTaskOutcome>, String> {
    runtime.block_on(run_parallel_async(
        entries,
        agents,
        ctx,
        concurrency,
        worktree,
    ))
}

/// Async core (see [`run_parallel`]) — call directly from runtime tasks
/// (background runner); never wrap in a nested `block_on`.
pub async fn run_parallel_async(
    entries: &[TaskEntry],
    agents: &[AgentConfig],
    ctx: &RunCtx,
    concurrency: usize,
    worktree: Option<std::sync::Arc<WorktreePlan>>,
) -> Result<Vec<ParallelTaskOutcome>, String> {
    let concurrency = concurrency.max(1);
    let outcomes: Vec<Option<Result<ParallelTaskOutcome, String>>> = async {
        // Owned entries: the stream closure must not borrow the items —
        // `map` over `&(index, &TaskEntry)` trips the HRTB bound (rust#89937).
        let indexed: Vec<(usize, TaskEntry)> = entries.iter().cloned().enumerate().collect();
        let agents = Arc::new(agents.to_vec());
        let results = stream::iter(indexed)
            .map(|(index, entry)| {
                let agents = Arc::clone(&agents);
                let plan = worktree.clone();
                async move {
                    let outcome = launch_one(&entry, &agents, ctx, plan.as_deref()).await;
                    (index, outcome)
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<(usize, Option<Result<ParallelTaskOutcome, String>>)>>()
            .await;
        let mut slots: Vec<Option<Result<ParallelTaskOutcome, String>>> = vec![None; entries.len()];
        for (index, outcome) in results {
            slots[index] = outcome;
        }
        slots
    }
    .await;
    // A task whose launch failed (agent resolution, worktree creation, launch
    // error) projects to SKIPPED below with its reason preserved; per-entry
    // failures never abort the batch.
    Ok(outcomes
        .into_iter()
        .zip(entries.iter())
        .map(|(outcome, entry)| {
            let skipped = |reason: String| ParallelTaskOutcome {
                agent: entry.spec.agent_name.clone(),
                output: format!("(skipped — {reason})"),
                exit_code: -1,
                error: Some(reason),
                timed_out: false,
                output_target_path: None,
                output_target_exists: false,
                details: json!({ "skipped": true }),
            };
            match outcome {
                Some(Ok(outcome)) => outcome,
                Some(Err(reason)) => skipped(reason),
                None => skipped("launch failed without a reason".to_string()),
            }
        })
        .collect())
}

/// Launch one entry. `Some(Ok(..))` = ran; `Some(Err(reason))` = skipped
/// with the reason preserved (agent resolution or worktree creation failed);
/// `None` = unexpected launch failure without a message.
async fn launch_one(
    entry: &TaskEntry,
    agents: &[AgentConfig],
    ctx: &RunCtx,
    worktree: Option<&WorktreePlan>,
) -> Option<Result<ParallelTaskOutcome, String>> {
    let agent = match discover::resolve_agent_name(agents, &entry.spec.agent_name) {
        Ok(Some(agent)) => agent.clone(),
        Ok(None) => return Some(Err(format!("Unknown agent: {}", entry.spec.agent_name))),
        Err(message) => return Some(Err(message)),
    };
    // Worktree isolation (FR-P1-06): create → redirect cwd → run → capture
    // patch → journal → cleanup. Creation failure skips the child entirely
    // (upstream: "creation failure does not start the child") — with the
    // failure text kept so the aggregate can say why.
    let mut spec = entry.spec.clone();
    let mut prepared = None;
    if let Some(plan) = worktree {
        if plan
            .enabled
            .get(entry.spec.child_index as usize)
            .copied()
            .unwrap_or(false)
        {
            let cwd = spec.cwd.clone().unwrap_or_else(|| ctx.base_cwd.clone());
            let cwd_relative = match crate::p1::worktree::resolve_repo_cwd_relative(&cwd) {
                Ok(relative) => relative,
                Err(message) => return Some(Err(message)),
            };
            let info = match crate::p1::worktree::create_worktree(
                &plan.toplevel,
                &cwd_relative,
                &ctx.run_id,
                entry.spec.child_index as usize,
                &plan.base_commit,
                &plan.base_dir,
                Some(&entry.spec.agent_name),
                &plan.config,
            ) {
                Ok(info) => info,
                Err(message) => return Some(Err(message)),
            };
            spec.cwd = Some(info.agent_cwd.clone());
            prepared = Some(info);
        }
    }
    let outcome: ChildOutcome = match launch_child::run_child_async(&spec, &agent, ctx).await {
        Ok(outcome) => outcome,
        Err(message) => return Some(Err(message)),
    };
    if let (Some(plan), Some(info)) = (worktree, prepared) {
        let patch_dir = plan.base_dir.join("patches");
        if let Ok(diff) = crate::p1::worktree::capture_worktree_diff(
            &info,
            &outcome.agent_name,
            &plan.base_commit,
            &patch_dir,
        ) {
            let status = if outcome.result.exit_code == 0 {
                "complete"
            } else {
                "failed"
            };
            let _ = crate::p1::worktree::cleanup_worktree(
                &plan.toplevel,
                &info,
                plan.manifest_path.as_deref(),
            );
            plan.diffs.lock().unwrap_or_else(|e| e.into_inner()).push((
                entry.spec.child_index as usize,
                outcome.agent_name.clone(),
                status.to_string(),
                diff,
            ));
        } else {
            // Dirty worktree without a recorded patch: keep it for
            // inspection (cleanup_worktree refuses) — remember it so
            // `finalize_worktree_handoff` can retry once the manifest
            // journals the surviving patches.
            let _ = crate::p1::worktree::cleanup_worktree(
                &plan.toplevel,
                &info,
                plan.manifest_path.as_deref(),
            );
            plan.kept
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(info);
        }
    }
    Some(Ok(project_outcome(entry, &outcome, &ctx.run_id)))
}

/// Write the handoff manifest after a worktree batch finishes (upstream
/// writes it before cleanup so `handoffRecordsPatch` passes), then retry the
/// cleanup of worktrees kept dirty earlier — now that the manifest journals
/// the surviving patches, `cleanup_worktree` accepts them.
pub fn finalize_worktree_handoff(plan: &WorktreePlan, run_id: &str, cwd: &std::path::Path) {
    let diffs = plan.diffs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let path = (!diffs.is_empty()).then(|| {
        crate::p1::worktree::write_handoff_manifest(
            &plan.base_dir,
            run_id,
            "parallel",
            cwd,
            &plan.base_commit,
            &diffs,
        )
    });
    let kept: Vec<_> = plan
        .kept
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    for info in kept {
        let _ = crate::p1::worktree::cleanup_worktree(&plan.toplevel, &info, path.as_deref());
    }
}

/// `ParallelTaskResult` projection off the shared child outcome.
pub fn project_outcome(
    entry: &TaskEntry,
    outcome: &ChildOutcome,
    run_id: &str,
) -> ParallelTaskOutcome {
    let output_target_path = outcome.saved_output_path.clone();
    let output_target_exists = output_target_path
        .as_ref()
        .is_some_and(|path| path.exists());
    ParallelTaskOutcome {
        agent: outcome.agent_name.clone(),
        output: outcome.result.final_output.clone(),
        exit_code: outcome.result.exit_code,
        error: outcome.result.error.clone(),
        timed_out: outcome.result.timed_out,
        output_target_path,
        output_target_exists,
        details: child_details(entry, outcome, run_id),
    }
}

/// Per-child details entry (types.ts:1014-1115 `results[]` items).
pub fn child_details(entry: &TaskEntry, outcome: &ChildOutcome, run_id: &str) -> Value {
    let result = &outcome.result;
    let mut single = json!({
        "index": entry.spec.child_index,
        "key": entry.key,
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
    // Acceptance ledger (FR-P1-09): drain this child's entry so parallel
    // results carry the same acceptance info as single runs.
    {
        let mut ledger_map = crate::p1::launch_child::GATE_LEDGER
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ledger) = ledger_map.remove(&(run_id.to_string(), entry.spec.child_index)) {
            single["acceptance"] = ledger;
        }
    }
    single
}

/// `aggregateParallelOutputs` (parallel-utils.ts:230-257): per-task sections
/// with status lines, joined by blank lines.
pub fn aggregate_parallel_outputs(results: &[ParallelTaskOutcome]) -> String {
    results
        .iter()
        .map(|r| {
            let header = format!(
                "=== Parallel Task {} ({}) ===",
                r.details["index"].as_u64().unwrap_or(0) + 1,
                r.agent
            );
            let has_output = !r.output.trim().is_empty();
            let status = if r.timed_out {
                Some(match &r.error {
                    Some(error) => format!("TIMED OUT: {error}"),
                    None => "TIMED OUT".to_string(),
                })
            } else if r.exit_code == -1 {
                // Skip reason rides in `error` (upstream "(skipped — …)").
                Some(match &r.error {
                    Some(error) => format!("SKIPPED: {error}"),
                    None => "SKIPPED".to_string(),
                })
            } else if r.exit_code != 0 {
                Some(match &r.error {
                    Some(error) => format!("FAILED (exit code {}): {error}", r.exit_code),
                    None => format!("FAILED (exit code {})", r.exit_code),
                })
            } else if let Some(error) = &r.error {
                Some(format!("WARNING: {error}"))
            } else if !has_output && r.output_target_path.is_some() && !r.output_target_exists {
                Some(format!(
                    "EMPTY OUTPUT (expected output file missing: {})",
                    r.output_target_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default()
                ))
            } else if !has_output && r.output_target_path.is_none() {
                Some("EMPTY OUTPUT (no textual response returned)".to_string())
            } else {
                None
            };
            let body = match status {
                Some(status) if has_output => format!("{status}\n{}", r.output),
                Some(status) => status,
                None => r.output.clone(),
            };
            format!("{header}\n{body}")
        })
        .collect::<Vec<String>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pattern_validation() {
        assert!(valid_key("a"));
        assert!(valid_key("scan-1"));
        assert!(valid_key("A.b_C-9"));
        assert!(!valid_key(""));
        assert!(!valid_key("-x"));
        assert!(!valid_key("a b"));
        assert!(!valid_key("a/b"));
        assert!(!valid_key(&"x".repeat(129)));
    }

    #[test]
    fn task_parsing_validates_entries() {
        let tasks = json!([
            {"key": "a", "agent": "scout", "task": "t"},
            {"key": "a", "agent": "scout", "task": "t"},
        ]);
        assert!(parse_tasks(&tasks, 8).is_err());
        let tasks = json!([{"key": "bad key", "agent": "scout", "task": "t"}]);
        assert!(parse_tasks(&tasks, 8).is_err());
        let tasks = json!([{"key": "a", "agent": "scout", "task": "t", "action": "list"}]);
        assert!(parse_tasks(&tasks, 8).is_err());
        let tasks = json!([{"key": "a", "agent": "scout"}]);
        assert!(parse_tasks(&tasks, 8).is_err());
        let tasks = json!([{"key": "a", "agent": "scout", "task": "t"}]);
        let entries = parse_tasks(&tasks, 8).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].spec.child_index, 0);
        // maxTasks cap.
        let many = json!([
            {"agent": "a", "task": "t"}, {"agent": "a", "task": "t"},
            {"agent": "a", "task": "t"},
        ]);
        assert!(parse_tasks(&many, 2).is_err());
        // Generated keys for entries without one.
        let entries = parse_tasks(&many, 8).unwrap();
        assert_eq!(entries[0].key, "task-0");
        assert_eq!(entries[2].key, "task-2");
    }

    #[test]
    fn aggregation_status_lines() {
        fn mk(
            _key: &str,
            agent: &str,
            output: &str,
            exit: i32,
            error: Option<&str>,
            timed_out: bool,
            index: u64,
        ) -> ParallelTaskOutcome {
            ParallelTaskOutcome {
                agent: agent.into(),
                output: output.into(),
                exit_code: exit,
                error: error.map(str::to_string),
                timed_out,
                output_target_path: None,
                output_target_exists: false,
                details: json!({ "index": index }),
            }
        }
        let results = vec![
            mk("a", "scout", "found it", 0, None, false, 0),
            mk("b", "worker", "", 3, Some("boom"), false, 1),
            mk("c", "worker", "", 0, None, true, 2),
            mk("d", "reviewer", "", 0, None, false, 3),
        ];
        let text = aggregate_parallel_outputs(&results);
        assert!(text.contains("=== Parallel Task 1 (scout) ===\nfound it"));
        assert!(text.contains("FAILED (exit code 3): boom"));
        assert!(text.contains("TIMED OUT"));
        assert!(text.contains("EMPTY OUTPUT (no textual response returned)"));
    }
}
