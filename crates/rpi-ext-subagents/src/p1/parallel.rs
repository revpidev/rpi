//! Parallel fan-out runs (FR-P1-01 / FR-P1-03): one `tasks: [...]` call
//! expands bounded-concurrency children, isolates per-task failures, and
//! aggregates results in submission order.
//!
//! Port of pi-subagents `src/runs/shared/parallel-utils.ts` (`mapConcurrent`,
//! `aggregateParallelOutputs`, MAX_PARALLEL_CONCURRENCY) and the workflow
//! `runs.all` admission semantics (scripted-workflow.ts:178-194: batch
//! admitted at once, each child collects failure instead of rejecting).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

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
                context_profile: object.get("context").and_then(Value::as_str) == Some("profile"),
                cwd: object
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(crate::paths::expand_tilde_and_resolve),
                output,
                output_mode: match object.get("outputMode").and_then(Value::as_str) {
                    Some("inline") | Some("file-only") => object
                        .get("outputMode")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                },
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
    pub diffs: std::sync::Mutex<Vec<(usize, String, String, crate::p1::worktree::WorktreeDiff)>>,
    /// Worktrees awaiting cleanup — `finalize` publishes the patch records
    /// first, then attempts removal (upstream two-pass handoff ordering).
    pub pending: std::sync::Mutex<Vec<PendingWorktreeCleanup>>,
}

/// One worktree whose cleanup is deferred until the handoff manifest has
/// journaled its patch (R7.1.5.1). `reason` records a capture failure so the
/// preserved cleanup task carries the original diagnostic.
#[derive(Debug, Clone)]
pub struct PendingWorktreeCleanup {
    pub info: crate::p1::worktree::WorktreeInfo,
    pub patch_path: Option<std::path::PathBuf>,
    pub reason: Option<String>,
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

/// Per-child lifecycle events for the parallel batch (rpi#29). `Started`
/// fires when the child's launch slot is taken (before agent resolution
/// and child spawn — upstream sets the step `running` inside the task fn,
/// subagent-runner.ts:4061); `Finished` fires when the child's outcome is
/// known, while the rest of the batch keeps running (upstream writes the
/// step terminal inside each task, subagent-runner.ts:3740-3755).
///
/// v0.66 #1060/#1474 (R7.1.6.1): `Finished` carries the child's
/// [`StepTerminal`] so a stop-requested child is recorded `stopped`, never
/// `failed` (04 §3.3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelStepEvent {
    Started,
    Finished {
        exit_code: i32,
        error: Option<String>,
        terminal: StepTerminal,
    },
}

/// Terminal classification for one settled child (R7.1.6.1; 04 §3.3.5).
/// Upstream keeps the same five-way distinction in
/// `SubagentResultStatus`/`ExecutionProjectionStatus` (v0.66 types.ts:398/537)
/// and derives run outcomes in the same precedence order (run-history.ts:141-152:
/// stopped > interrupted > timed-out > unexplained signal > exit 0 > failed);
/// rpi names the interrupt face `Paused` because `interrupt` is its pause
/// marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepTerminal {
    Complete,
    Failed,
    Stopped,
    Paused,
    TimedOut,
}

impl StepTerminal {
    /// Step-status string written into `status.json` / result documents
    /// (`complete`/`failed`/`stopped`/`paused`/`timed_out`, R7.1.6.1).
    pub fn as_status(self) -> &'static str {
        match self {
            StepTerminal::Complete => "complete",
            StepTerminal::Failed => "failed",
            StepTerminal::Stopped => "stopped",
            StepTerminal::Paused => "paused",
            StepTerminal::TimedOut => "timed_out",
        }
    }
}

/// Run-level control snapshot used by [`classify_terminal`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunControlFlags {
    pub stop_requested: bool,
    pub interrupt_requested: bool,
}

/// Live control probe, read when a child settles (a stop can arrive
/// mid-batch). Background runs wire it to the handle's control flags;
/// foreground batches have no run handle and pass `None`.
pub type RunControlProbe = Arc<dyn Fn() -> RunControlFlags + Send + Sync>;

/// Classify one settled child (upstream run-history.ts:141-152 precedence:
/// stopped > interrupted > timedOut > unexplained signal > exit 0 > failed).
///
/// Approximation: upstream's `isUnexplainedProcessSignal` (process-signal.ts:
/// 19-28) also treats `turnBudgetExceeded`/`forcedDrainAfterFinalSuccess` as
/// *explained* (→ failed); the rpi subprocess result carries no such flags
/// (budget enforcement lives in the child prompt/acceptance layer), so a
/// non-zero signal exit classifies as stopped.
pub fn classify_terminal(
    exit_code: i32,
    timed_out: bool,
    process_signal: Option<&str>,
    control: RunControlFlags,
) -> StepTerminal {
    if control.stop_requested {
        StepTerminal::Stopped
    } else if control.interrupt_requested {
        StepTerminal::Paused
    } else if timed_out {
        StepTerminal::TimedOut
    } else if exit_code != 0 && process_signal.is_some() {
        // Unexplained signal (`isUnexplainedProcessSignal`, process-signal.ts:
        // 19-28): killed by a signal nobody ordered reads as stopped.
        StepTerminal::Stopped
    } else if exit_code == 0 {
        StepTerminal::Complete
    } else {
        StepTerminal::Failed
    }
}

/// One explicit output claim (R7.1.6.3): resolved path + task/step label.
#[derive(Debug, Clone)]
pub struct OutputClaim {
    pub owner: String,
    pub path: PathBuf,
}

/// Build one output claim from a task/step override plus the agent's
/// frontmatter default.
///
/// Inherited **relative** defaults are excluded: upstream isolates them per
/// task in a parallel namespace (`child-launch-plan.ts:129-146`), so they are
/// not a launch-time collision. Inherited **absolute** defaults have no
/// namespace and stay shared, so upstream's check sees them and so does this
/// one (observation 2 of the TE16 independent review, 2026-09-09).
pub fn output_claim(
    owner: String,
    output: &crate::p1::launch_child::OutputOverride,
    agent_output: Option<&str>,
) -> Option<OutputClaim> {
    match output {
        crate::p1::launch_child::OutputOverride::Path(path) => Some(OutputClaim {
            owner,
            path: path.clone(),
        }),
        crate::p1::launch_child::OutputOverride::Disabled => None,
        crate::p1::launch_child::OutputOverride::Inherit => {
            let raw = agent_output?;
            if !Path::new(raw).is_absolute() {
                return None;
            }
            Some(OutputClaim {
                owner,
                path: crate::paths::expand_tilde_and_resolve(raw),
            })
        }
    }
}

/// `resolveSingleOutputClaimPath` (single-output.ts:175-185): realpath the
/// deepest existing ancestor and append the still-missing segments, so
/// symlinked parents compare equal without requiring the leaf to exist.
pub fn resolve_output_claim_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) if parent != existing => {
                missing.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for segment in missing.iter().rev() {
        resolved.push(segment);
    }
    resolved
}

/// Every collision group (claim path -> all owners), stable by first claim.
pub fn find_output_collisions(claims: &[OutputClaim]) -> Vec<(PathBuf, Vec<String>)> {
    let mut groups: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for claim in claims {
        let resolved = resolve_output_claim_path(&claim.path);
        match groups.iter_mut().find(|(path, _)| *path == resolved) {
            Some((_, owners)) => owners.push(claim.owner.clone()),
            None => groups.push((resolved, vec![claim.owner.clone()])),
        }
    }
    groups.retain(|(_, owners)| owners.len() > 1);
    groups
}

/// Fail-closed pre-flight for explicit output paths (upstream
/// async-execution.ts:1189-1200: `Parallel tasks N (agent) and M (agent)
/// resolve output to the same path: <path>. Use distinct output paths.`).
/// The rpi message lists every conflicting path and all its claimants
/// (FR-F R2), not just the first pair.
pub fn validate_output_collisions(claims: &[OutputClaim]) -> Result<(), String> {
    let collisions = find_output_collisions(claims);
    if collisions.is_empty() {
        return Ok(());
    }
    let detail = collisions
        .iter()
        .map(|(path, owners)| format!("{} <- {}", path.to_string_lossy(), owners.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "Output path collision before launch: {detail}. Use distinct output paths."
    ))
}

/// Sink receiving `(submission index, event)`. The async runner mirrors
/// these into the run status document as they happen so `subagent_wait`
/// shows live per-child states instead of a batch-long `queued`.
pub type ParallelStepSink = std::sync::Arc<dyn Fn(usize, ParallelStepEvent) + Send + Sync>;

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
        None,
        None,
    ))
}

/// Async core (see [`run_parallel`]) — call directly from runtime tasks
/// (background runner); never wrap in a nested `block_on`. `on_step`
/// receives per-child lifecycle events as they happen (rpi#29); the
/// foreground path passes `None` (it streams live frames instead).
pub async fn run_parallel_async(
    entries: &[TaskEntry],
    agents: &[AgentConfig],
    ctx: &RunCtx,
    concurrency: usize,
    worktree: Option<std::sync::Arc<WorktreePlan>>,
    on_step: Option<ParallelStepSink>,
    control_probe: Option<RunControlProbe>,
) -> Result<Vec<ParallelTaskOutcome>, String> {
    let concurrency = concurrency.max(1);
    // rpi#30: the run-wide global child cap (upstream per-run Semaphore,
    // subagent-runner.ts:1932 + parallel-utils.ts:167-226): every child
    // holds one permit for its whole execution, bounding the TOTAL
    // concurrently running children regardless of the per-batch
    // concurrency (default 20, `globalConcurrencyLimit` config) — a
    // `concurrency: 50` batch is still capped at 20 simultaneous children.
    let global_permits = Arc::new(Semaphore::new(
        ctx.config
            .global_concurrency_limit()
            .clamp(1, u32::MAX as u64) as usize,
    ));
    let outcomes: Vec<Option<Result<ParallelTaskOutcome, String>>> = async {
        // Owned entries: the stream closure must not borrow the items —
        // `map` over `&(index, &TaskEntry)` trips the HRTB bound (rust#89937).
        let indexed: Vec<(usize, TaskEntry)> = entries.iter().cloned().enumerate().collect();
        let agents = Arc::new(agents.to_vec());
        let results = stream::iter(indexed)
            .map(|(index, entry)| {
                let agents = Arc::clone(&agents);
                let plan = worktree.clone();
                let on_step = on_step.clone();
                let control_probe = control_probe.clone();
                let permits = Arc::clone(&global_permits);
                async move {
                    // Upstream worker shape (parallel-utils.ts:210-219):
                    // acquire the global permit, run the whole task, drop
                    // it. `Started` fires only after the slot is taken, so
                    // the mirrored step `running` count tracks the cap.
                    let _global_permit = permits
                        .acquire()
                        .await
                        .expect("global semaphore is never closed");
                    if let Some(sink) = &on_step {
                        sink(index, ParallelStepEvent::Started);
                    }
                    let outcome = launch_one(
                        &entry,
                        &agents,
                        ctx,
                        plan.as_deref(),
                        control_probe.as_ref(),
                    )
                    .await;
                    if let Some(sink) = &on_step {
                        let control = control_probe
                            .as_ref()
                            .map(|probe| probe())
                            .unwrap_or_default();
                        let (exit_code, error, terminal) = match &outcome {
                            Some(Ok(result)) => (
                                result.exit_code,
                                result.error.clone(),
                                classify_terminal(
                                    result.exit_code,
                                    result.timed_out,
                                    result.details["processSignal"].as_str(),
                                    control,
                                ),
                            ),
                            Some(Err(reason)) => (
                                -1,
                                Some(reason.clone()),
                                classify_terminal(-1, false, None, control),
                            ),
                            None => (
                                -1,
                                Some("launch failed without a reason".to_owned()),
                                classify_terminal(-1, false, None, control),
                            ),
                        };
                        sink(
                            index,
                            ParallelStepEvent::Finished {
                                exit_code,
                                error,
                                terminal,
                            },
                        );
                    }
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
    control_probe: Option<&RunControlProbe>,
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
        match crate::p1::worktree::capture_worktree_diff(
            &info,
            &outcome.agent_name,
            &plan.base_commit,
            &patch_dir,
        ) {
            Ok(diff) => {
                let control = control_probe.map(|probe| probe()).unwrap_or_default();
                let status = classify_terminal(
                    outcome.result.exit_code,
                    outcome.result.timed_out,
                    outcome.result.process_signal.as_deref(),
                    control,
                )
                .as_status()
                .to_string();
                plan.diffs.lock().unwrap_or_else(|e| e.into_inner()).push((
                    entry.spec.child_index as usize,
                    outcome.agent_name.clone(),
                    status,
                    diff.clone(),
                ));
                // Cleanup is deferred: `finalize_worktree_handoff` publishes
                // the patch record first so cleanup can verify it.
                plan.pending.lock().unwrap_or_else(|e| e.into_inner()).push(
                    PendingWorktreeCleanup {
                        info,
                        patch_path: Some(diff.patch_path),
                        reason: None,
                    },
                );
            }
            Err(reason) => {
                // R7.1.5.1: a worktree whose patch could not be captured and
                // validated is preserved — finalize records the reason.
                plan.pending.lock().unwrap_or_else(|e| e.into_inner()).push(
                    PendingWorktreeCleanup {
                        info,
                        patch_path: None,
                        reason: Some(reason),
                    },
                );
            }
        }
    }
    Some(Ok(project_outcome(entry, &outcome, &ctx.run_id)))
}

/// Write the handoff manifest after a worktree batch finishes, then clean up
/// the pending worktrees — the first manifest pass journals the patches so
/// `cleanup_worktree` can verify them, the second records the cleanup report
/// (upstream writes the manifest twice, subagent-executor.ts:3647-3649).
pub fn finalize_worktree_handoff(
    plan: &WorktreePlan,
    run_id: &str,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let diffs = plan.diffs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let pending: Vec<_> = plan
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    if diffs.is_empty() && pending.is_empty() {
        return None;
    }
    let pending_tasks: Vec<crate::p1::worktree::WorktreeCleanupTask> = pending
        .iter()
        .map(|entry| {
            crate::p1::worktree::WorktreeCleanupTask::pending(
                entry.info.index,
                &entry.info.path,
                &entry.info.branch,
            )
        })
        .collect();
    let manifest = crate::p1::worktree::write_handoff_manifest(
        &plan.base_dir,
        run_id,
        "parallel",
        cwd,
        &plan.base_commit,
        &diffs,
        &pending_tasks,
    );
    let mut cleanup_tasks: Vec<crate::p1::worktree::WorktreeCleanupTask> = Vec::new();
    for entry in pending {
        match crate::p1::worktree::cleanup_worktree(
            &plan.toplevel,
            &entry.info,
            &plan.base_commit,
            entry.patch_path.as_deref(),
            Some(&manifest),
        ) {
            Ok(()) => cleanup_tasks.push(crate::p1::worktree::WorktreeCleanupTask::removed(
                entry.info.index,
                &entry.info.path,
                &entry.info.branch,
            )),
            Err(reason) => {
                let reason = match &entry.reason {
                    Some(capture_error) => format!("{reason}; capture error: {capture_error}"),
                    None => reason,
                };
                cleanup_tasks.push(crate::p1::worktree::WorktreeCleanupTask::preserved(
                    entry.info.index,
                    &entry.info.path,
                    &entry.info.branch,
                    &reason,
                ));
            }
        }
    }
    Some(crate::p1::worktree::write_handoff_manifest(
        &plan.base_dir,
        run_id,
        "parallel",
        cwd,
        &plan.base_commit,
        &diffs,
        &cleanup_tasks,
    ))
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
    fn terminal_classification_matches_fixture_vectors() {
        // R7.1.6.1: the target-track fixture owns the vector set; the
        // classifier and its persisted status strings must match every case.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/subagents-v066/terminal-classification.json");
        let raw = std::fs::read_to_string(&path).expect("TE13 fixture is present");
        let fixture: Value = serde_json::from_str(&raw).unwrap();
        let cases = fixture["terminal_vectors"]["cases"]
            .as_array()
            .expect("TE16 extends terminal-classification.json");
        for case in cases {
            let input = &case["input"];
            let control = RunControlFlags {
                stop_requested: input["stopRequested"].as_bool().unwrap_or(false),
                interrupt_requested: input["interruptRequested"].as_bool().unwrap_or(false),
            };
            let terminal = classify_terminal(
                input["exitCode"].as_i64().unwrap_or(0) as i32,
                input["timedOut"].as_bool().unwrap_or(false),
                input["processSignal"].as_str(),
                control,
            );
            assert_eq!(
                terminal.as_status(),
                case["expected"]["stepStatus"].as_str().unwrap(),
                "case {}",
                case["name"]
            );
        }
        // Precedence spot-checks independent of the fixture file.
        let stopped = RunControlFlags {
            stop_requested: true,
            ..RunControlFlags::default()
        };
        assert_eq!(
            classify_terminal(0, false, None, stopped),
            StepTerminal::Stopped
        );
        let paused = RunControlFlags {
            interrupt_requested: true,
            ..RunControlFlags::default()
        };
        assert_eq!(
            classify_terminal(0, false, None, paused),
            StepTerminal::Paused
        );
    }

    #[test]
    fn explicit_output_collisions_are_rejected_before_launch() {
        // T-12/T-13: explicit output paths are normalized (realpath of the
        // existing ancestor) and every conflicting claim is listed; distinct
        // paths and missing leaves do not false-positive.
        let base = std::env::temp_dir().join(format!(
            "rpi-sub-collide-{}-{}",
            std::process::id(),
            crate::artifacts::now_millis()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let same = base.join("report.md");
        let claims = vec![
            OutputClaim {
                owner: "tasks[0] (scout)".to_string(),
                path: same.clone(),
            },
            OutputClaim {
                owner: "tasks[1] (worker)".to_string(),
                path: base.join(".").join("report.md"),
            },
            OutputClaim {
                owner: "tasks[2] (reviewer)".to_string(),
                path: same.clone(),
            },
        ];
        let error = validate_output_collisions(&claims).expect_err("duplicate output");
        assert!(error.contains("tasks[0] (scout)"), "{error}");
        assert!(error.contains("tasks[1] (worker)"), "{error}");
        assert!(error.contains("tasks[2] (reviewer)"), "{error}");
        assert!(error.contains("report.md"), "{error}");
        assert!(error.contains("Use distinct output paths."), "{error}");

        let distinct = vec![
            OutputClaim {
                owner: "tasks[0] (scout)".to_string(),
                path: base.join("a.md"),
            },
            OutputClaim {
                owner: "tasks[1] (worker)".to_string(),
                path: base.join("b.md"),
            },
            OutputClaim {
                owner: "steps[0] (scout)".to_string(),
                path: base.join("a.md"),
            },
        ];
        assert!(validate_output_collisions(&distinct).is_err());
        let no_collision = vec![
            OutputClaim {
                owner: "tasks[0] (scout)".to_string(),
                path: base.join("a.md"),
            },
            OutputClaim {
                owner: "tasks[1] (worker)".to_string(),
                path: base.join("b.md"),
            },
        ];
        validate_output_collisions(&no_collision).expect("distinct paths launch");
        assert!(find_output_collisions(&no_collision).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn output_claim_inherited_defaults_follow_upstream() {
        // Independent-review observation 2: inherited RELATIVE defaults are
        // isolated upstream and excluded here; inherited ABSOLUTE defaults
        // have no namespace and still participate.
        use crate::p1::launch_child::OutputOverride;
        let explicit = output_claim(
            "tasks[0] (scout)".to_string(),
            &OutputOverride::Path(PathBuf::from("/tmp/a.md")),
            None,
        )
        .expect("explicit path");
        assert_eq!(explicit.path, PathBuf::from("/tmp/a.md"));
        assert!(output_claim("x".to_string(), &OutputOverride::Disabled, None).is_none());
        assert!(
            output_claim(
                "tasks[1] (scout)".to_string(),
                &OutputOverride::Inherit,
                Some("context.md")
            )
            .is_none(),
            "inherited relative defaults are isolated upstream"
        );
        let inherited_absolute = output_claim(
            "tasks[2] (scout)".to_string(),
            &OutputOverride::Inherit,
            Some("/tmp/shared-report.md"),
        )
        .expect("inherited absolute default");
        assert_eq!(
            inherited_absolute.path,
            PathBuf::from("/tmp/shared-report.md")
        );
        assert!(
            output_claim(
                "tasks[3] (scout)".to_string(),
                &OutputOverride::Inherit,
                None
            )
            .is_none(),
            "no agent default → no claim"
        );
        let colliding = vec![
            inherited_absolute,
            output_claim(
                "tasks[4] (worker)".to_string(),
                &OutputOverride::Inherit,
                Some("/tmp/shared-report.md"),
            )
            .expect("second inherited absolute default"),
        ];
        assert!(
            validate_output_collisions(&colliding).is_err(),
            "two inherited absolute defaults collide"
        );
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
