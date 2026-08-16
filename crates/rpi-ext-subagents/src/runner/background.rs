//! Background (async) run registry: in-host detached runner tasks,
//! `status.json` + `events.jsonl` + control-channel files under the run dir,
//! completion notification via `sendMessage` + `events.emit`, and the
//! session-level budget ledgers (FR-P1-04).
//!
//! rpi shape per ADR-0019: the runner is a tokio task on the plugin runtime
//! (no separate runner process — rpi has no JS host and rpi CLI re-exec would
//! need new flags, both out of bounds). Headless hosts auto-drain at
//! `agent_end`; interactive hosts harvest on `session_shutdown`; orphans from
//! crashed hosts are reaped at plugin init by the stale-run reconciler
//! (`owner_pid` liveness). Upstream map: `src/runs/background/async-execution.ts`
//! (spawnRunner/dir layout), `subagent-runner.ts` (status lifecycle),
//! `control-channel.ts` (file channel), `notify.ts` (result file + sendMessage),
//! `spawn-budget.ts`, `active-async-capacity.ts`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::p1::launch_child::RunCtx;

/// Run dir layout under the temp root (types.ts:2024-2028).
pub fn async_runs_dir() -> PathBuf {
    crate::paths::temp_root_dir().join("async-subagent-runs")
}
pub fn async_results_dir() -> PathBuf {
    crate::paths::temp_root_dir().join("async-subagent-results")
}
pub fn spawn_budgets_dir() -> PathBuf {
    crate::paths::temp_root_dir().join("spawn-budgets")
}

/// `AsyncStatus.state` machine (types.ts:1331).
pub const STATE_QUEUED: &str = "queued";
pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETE: &str = "complete";
pub const STATE_FAILED: &str = "failed";
pub const STATE_PAUSED: &str = "paused";
pub const STATE_STOPPED: &str = "stopped";
pub const STATE_REJECTED: &str = "rejected";

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso8601(millis: u64) -> String {
    crate::artifacts::format_iso8601(millis)
}

/// In-process handle to one async run. `control` carries the cooperative
/// flags the runner task polls between children (`stop` → terminal,
/// `interrupt` → pause).
#[derive(Debug)]
pub struct AsyncRunHandle {
    pub run_id: String,
    pub status: Arc<RwLock<Value>>,
    pub control: Arc<AsyncControl>,
    pub run_dir: PathBuf,
    /// Wall-clock start (epoch ms) for the completion-notification
    /// `durationMs` (notify.ts buildCompletionDetails).
    pub started_ms: u64,
}

#[derive(Debug, Default)]
pub struct AsyncControl {
    pub stop_requested: Mutex<bool>,
    pub interrupt_requested: Mutex<bool>,
}

impl AsyncControl {
    pub fn request_stop(&self) {
        *self
            .stop_requested
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
    }
    pub fn stop_requested(&self) -> bool {
        *self
            .stop_requested
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    pub fn request_interrupt(&self) {
        *self
            .interrupt_requested
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
    }
}

/// Registry of live and recently finished async runs (process-wide; one host
/// process at a time owns each run, per ADR-0019 ownership rules).
pub static ASYNC_RUNS: Mutex<BTreeMap<String, Arc<AsyncRunHandle>>> = Mutex::new(BTreeMap::new());

fn register_run(handle: Arc<AsyncRunHandle>) {
    ASYNC_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle.run_id.clone(), handle);
}

fn unregister_run(run_id: &str) {
    ASYNC_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(run_id);
}

/// Terminal runs stay registered (the registry doc contract: "live and
/// recently finished"). `subagent_wait` and the `stop` action observe the
/// terminal transition by polling this table — unregistering on finish made
/// the run vanish from their snapshot set, so both waited out their full
/// timeout instead of returning within one poll cycle (upstream keeps
/// terminal runs in the provider items too; `subagent-wait.ts:552` filters
/// them at wait START, not on completion). Bound the retention so a long
/// session does not accumulate handles: past the cap the oldest terminal
/// entries (by `started_ms`) drop off.
const MAX_RETAINED_TERMINAL_RUNS: usize = 64;

fn prune_terminal_runs() {
    let terminal_states = [
        STATE_COMPLETE,
        STATE_FAILED,
        STATE_STOPPED,
        STATE_PAUSED,
        STATE_REJECTED,
    ];
    let is_terminal = |handle: &Arc<AsyncRunHandle>| {
        let status = handle.status.read().unwrap_or_else(|e| e.into_inner());
        terminal_states.contains(&status["state"].as_str().unwrap_or(""))
    };
    let mut runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let mut terminal: Vec<(u64, String)> = runs
        .values()
        .filter(|handle| is_terminal(handle))
        .map(|handle| (handle.started_ms, handle.run_id.clone()))
        .collect();
    if terminal.len() <= MAX_RETAINED_TERMINAL_RUNS {
        return;
    }
    terminal.sort_by_key(|(started, _)| *started);
    let excess = terminal.len() - MAX_RETAINED_TERMINAL_RUNS;
    for (_, run_id) in terminal.into_iter().take(excess) {
        runs.remove(&run_id);
    }
}

/// Find a run by exact id or unique id prefix (async-status id resolution).
pub fn find_run(query: &str) -> Option<Arc<AsyncRunHandle>> {
    let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = runs.get(query) {
        return Some(handle.clone());
    }
    let mut matches: Vec<&Arc<AsyncRunHandle>> = runs
        .values()
        .filter(|h| h.run_id.starts_with(query))
        .collect();
    if matches.len() == 1 {
        return Some(matches.remove(0).clone());
    }
    None
}

/// `find_run` restricted to non-terminal runs — the candidate set for the
/// control actions (steer/interrupt). Terminal runs stay registered for
/// wait/status reads, but steering a finished run would write an inbox no
/// child will ever read.
pub fn find_active_run(query: &str) -> Option<Arc<AsyncRunHandle>> {
    let handle = find_run(query)?;
    let terminal = {
        let status = handle.status.read().unwrap_or_else(|e| e.into_inner());
        matches!(
            status["state"].as_str().unwrap_or(""),
            STATE_COMPLETE | STATE_FAILED | STATE_STOPPED | STATE_PAUSED | STATE_REJECTED
        )
    };
    if terminal {
        None
    } else {
        Some(handle)
    }
}

/// Snapshot the status document.
pub fn status_snapshot(handle: &AsyncRunHandle) -> Value {
    handle
        .status
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn update_status(handle: &AsyncRunHandle, mutate: impl FnOnce(&mut Value)) {
    let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
    mutate(&mut status);
    status["updatedAt"] = json!(iso8601(now_millis()));
    let _ = crate::artifacts::write_metadata(&handle.run_dir.join("status.json"), &status);
}

fn append_event(run_dir: &Path, event: &str, data: Value) {
    let mut line = json!({
        "type": event,
        "ts": iso8601(now_millis()),
    });
    if let (Some(target), Some(source)) = (line.as_object_mut(), data.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    crate::artifacts::append_jsonl(&run_dir.join("events.jsonl"), &line.to_string());
}

// ---------------------------------------------------------------------------
// Budget ledgers (ADR-0019 §4)
// ---------------------------------------------------------------------------

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    // ADR-0019 §4: ledger/slot file names are `<sha256(sessionId)>.json`
    // (same as upstream).
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Session spawn budget file (spawn-budget.ts `state.subagentSpawns`):
/// `{sessionId, count, configuredLimit, granted, grantHistory}`; spawn before
/// run, atomically bumped, never decremented.
pub struct SpawnBudgetLedger {
    path: PathBuf,
}

impl SpawnBudgetLedger {
    pub fn open(session_id: &str) -> Self {
        let dir = spawn_budgets_dir();
        Self {
            path: dir.join(format!("{}.json", sha256_hex(session_id))),
        }
    }

    fn read(&self) -> Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| json!({ "count": 0, "granted": 0, "grantHistory": [] }))
    }

    fn write(&self, value: &Value) {
        let _ = crate::artifacts::write_metadata(&self.path, value);
    }

    /// Run `body` under the ledger's advisory cross-process lock (flock on a
    /// sibling lockfile): the read-modify-write of `reserve`/`grant` is
    /// atomic against other host processes (ADR-0019 §4 "atomically
    /// bumped"). The lock lives on the fd — a crashed holder releases it
    /// automatically.
    fn with_lock<T>(&self, body: impl FnOnce(&Self) -> T) -> Result<T, String> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let lock_path = self.path.with_extension("lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .map_err(|e| format!("failed to open the spawn-budget lock: {e}"))?;
            // Safety: flock(2) on a regular file fd we own.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err("failed to lock the spawn-budget ledger".to_string());
            }
            let result = body(self);
            let _ = file.sync_data();
            Ok(result)
        }
        #[cfg(not(unix))]
        {
            // No flock on this platform — degrade to the unlocked RMW (the
            // ledger still resolves to a whole file via atomic rename).
            Ok(body(self))
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> Value {
        self.read()
    }

    /// `preflight/reserveSpawnBudget` (spawn-budget.ts:59-86): the effective
    /// ceiling is `configured + granted` (grants extend it); the used count
    /// never resets within a session. The check-then-bump runs under the
    /// ledger lock so concurrent hosts cannot oversubscribe.
    pub fn reserve(&self, amount: u64, configured_limit: Option<u64>) -> Result<(), String> {
        self.with_lock(|ledger| {
            let mut state = ledger.read();
            let count = state["count"].as_u64().unwrap_or(0);
            let granted = state["granted"].as_u64().unwrap_or(0);
            if let Some(limit) = configured_limit {
                if count + amount > limit + granted {
                    return Err(format!(
                        "Subagent spawn budget exhausted for this session: {} of {} spawns used (grant more with subagent({{action:\"grant-spawn-budget\"}})).",
                        count,
                        limit + granted
                    ));
                }
            }
            state["count"] = json!(count + amount);
            ledger.write(&state);
            Ok(())
        })?
    }

    /// `grantSpawnBudget` (spawn-budget.ts:99-118): grants extend the
    /// effective ceiling; the cumulative grant is capped at the *original*
    /// configured limit (`grantRemaining`). Locked like `reserve`.
    pub fn grant(&self, extra: u64, configured_limit: Option<u64>) -> Result<u64, String> {
        if extra == 0 {
            return Err(
                "action='grant-spawn-budget' requires additional to be a positive integer."
                    .to_string(),
            );
        }
        self.with_lock(|ledger| {
            let mut state = ledger.read();
            let granted = state["granted"].as_u64().unwrap_or(0);
            let Some(limit) = configured_limit else {
                return Err(
                    "The current session has no configured spawn cap, so it does not need a budget grant."
                        .to_string(),
                );
            };
            let grant_remaining = limit.saturating_sub(granted);
            if extra > grant_remaining {
                return Err(format!(
                    "Spawn budget grant rejected: {extra} requested but only {grant_remaining} of the original configured limit remains grantable."
                ));
            }
            let next = granted + extra;
            state["granted"] = json!(next);
            if let Some(history) = state["grantHistory"].as_array_mut() {
                history.push(json!({
                    "at": iso8601(now_millis()),
                    "granted": extra,
                }));
                // grantHistory keeps the last 20 entries (spawn-budget.ts).
                let overflow = history.len().saturating_sub(20);
                for _ in 0..overflow {
                    history.remove(0);
                }
            }
            ledger.write(&state);
            Ok(next)
        })?
    }
}

/// Active async capacity slots (active-async-capacity.ts): one owner file per
/// slot; terminal runs release theirs. File-based so concurrent host
/// processes in the same scope share the ledger.
pub struct ActiveAsyncCapacity {
    dir: PathBuf,
}

impl ActiveAsyncCapacity {
    pub fn open(session_id: &str) -> Self {
        Self {
            dir: spawn_budgets_dir()
                .parent()
                .map(|p| p.join("session-active-async-capacity"))
                .unwrap_or_else(|| {
                    crate::paths::temp_root_dir().join("session-active-async-capacity")
                })
                .join(sha256_hex(session_id)),
        }
    }

    /// Acquire the first free slot ≤ limit; `Err` when all are taken.
    pub fn acquire(&self, run_id: &str, limit: u64) -> Result<PathBuf, String> {
        for slot in 0..limit.max(1) {
            let slot_dir = self.dir.join(format!("slot-{slot}"));
            let owner = slot_dir.join("owner.json");
            if owner.exists() {
                // Steal slots whose owner process died (reconcile semantics).
                if let Some(owner_pid) = std::fs::read_to_string(&owner)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .and_then(|v| v["ownerPid"].as_u64())
                {
                    if !pid_alive(owner_pid) {
                        let _ = std::fs::remove_dir_all(&slot_dir);
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            let _ = std::fs::create_dir_all(&slot_dir);
            #[cfg(unix)]
            {
                // Slot dirs are 0700 like upstream (active-async-capacity.ts:
                // 125,244 — `mkdirSync(..., {mode: 0o700})`).
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&slot_dir, std::fs::Permissions::from_mode(0o700));
            }
            let claimed = std::fs::create_dir(slot_dir.join("capacity.claim"));
            if claimed.is_err() {
                continue;
            }
            let _ = crate::artifacts::write_metadata(
                &owner,
                &json!({
                    "reservationToken": run_id,
                    "runId": run_id,
                    "reservedAt": iso8601(now_millis()),
                    "ownerPid": std::process::id(),
                    "kind": "runner",
                }),
            );
            return Ok(slot_dir);
        }
        Err(format!(
            "Active async run limit reached for this session ({limit} concurrent background runs)."
        ))
    }

    pub fn release(&self, slot_dir: &Path) {
        let _ = std::fs::remove_dir_all(slot_dir);
    }
}

fn pid_alive(pid: u64) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(0) probes liveness without sending a signal.
    #[cfg(unix)]
    {
        // Safety: kill(2) with pid>0 and signal 0.
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            return true;
        }
        // ESRCH → gone; EPERM → alive but owned by someone else.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        // Best-effort on non-unix: assume alive so slots are never stolen
        // from running owners.
        let _ = pid;
        true
    }
}

/// Kernel boot-relative start time of `pid` (`/proc/<pid>/stat` field 22,
/// ADR-0019 §3 `ownerBootId`): distinguishes a live pid from a reused one.
pub fn process_boot_id(pid: u64) -> Option<u64> {
    #[cfg(unix)]
    {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the comm paren (which may contain spaces) — overall
        // field 3 is the first after it, so starttime (field 22) is index 19.
        let after_comm = raw.rsplit_once(')')?.1;
        after_comm
            .split_whitespace()
            .nth(19)
            .and_then(|token| token.parse().ok())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Whether `pid` is alive AND started at `boot_id` (pid-reuse guard). A
/// recorded `boot_id` of `None` (non-unix) degrades to a liveness check.
fn pid_matches(pid: u64, boot_id: Option<u64>) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match boot_id {
        Some(expected) => process_boot_id(pid) == Some(expected),
        None => true,
    }
}

/// Persist a spawned child pid for `run_id` (ADR-0019 crash branch): the
/// stale-run reconciler reads these to signal orphaned children whose owner
/// host died. Append-only, so parallel children never clobber each other.
pub fn record_child_pid(run_id: &str, pid: u32) {
    if pid == 0 {
        return;
    }
    let run_dir = async_runs_dir().join(run_id);
    // Only async runs keep a run directory; foreground runs have no
    // reconciler surface (their dispatch dies with the host).
    if !run_dir.is_dir() {
        return;
    }
    crate::artifacts::append_jsonl(
        &run_dir.join("children.jsonl"),
        &json!({
            "pid": pid,
            "bootId": process_boot_id(pid as u64),
            "startedAt": iso8601(now_millis()),
        })
        .to_string(),
    );
}

/// Read the recorded child pids for a run directory (empty when absent).
fn recorded_child_pids(run_dir: &Path) -> Vec<(u64, Option<u64>)> {
    std::fs::read_to_string(run_dir.join("children.jsonl"))
        .ok()
        .map(|raw| {
            raw.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter_map(|entry| {
                    let pid = entry["pid"].as_u64()?;
                    let boot_id = entry["bootId"].as_u64();
                    Some((pid, boot_id))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Runner lifecycle
// ---------------------------------------------------------------------------

/// The composite body to execute — mirrors the foreground dispatch shapes
/// (single / parallel tasks / chain steps). `Single` carries the full
/// resolved `ChildSpec` so async keeps every foreground field (output, gate,
/// model, cwd, budgets…); only the steer inbox is re-pointed at the run dir.
pub enum AsyncBody {
    Single {
        spec: Box<crate::p1::launch_child::ChildSpec>,
    },
    Tasks {
        entries: Vec<crate::p1::parallel::TaskEntry>,
        concurrency: usize,
        worktree_plan: Option<std::sync::Arc<crate::p1::parallel::WorktreePlan>>,
    },
    Steps {
        steps: Vec<crate::p1::chain::StepSpec>,
        original_task: String,
    },
}

/// Receipt returned to the model immediately (async-execution.ts receipt).
pub fn receipt(run_id: &str, run_dir: &Path) -> Value {
    json!({
        "mode": "async",
        "runId": run_id,
        "status": "running",
        "statusFile": run_dir.join("status.json").to_string_lossy(),
        "eventsFile": run_dir.join("events.jsonl").to_string_lossy(),
        "hint": "Continue other work; completion arrives as a session message. Inspect with subagent({action:\"status\"}); control with stop/interrupt/steer/resume.",
    })
}

/// Initialize the run directory and status document, register the run.
pub fn start_run(run_id: &str, session_id: Option<&str>, body: &AsyncBody) -> Arc<AsyncRunHandle> {
    let run_dir = async_runs_dir().join(run_id);
    // 0700 (C3): the run dir holds status/events/steer messages — task text
    // and model output other local users should not read.
    let _ = crate::paths::create_private_dir_all(&run_dir);
    let _ = std::fs::create_dir_all(run_dir.join("control"));
    let status = Arc::new(RwLock::new(json!({
        "runId": run_id,
        "sessionId": session_id,
        "mode": match body {
            AsyncBody::Single { .. } => "single",
            AsyncBody::Tasks { .. } => "parallel",
            AsyncBody::Steps { .. } => "chain",
        },
        "state": STATE_RUNNING,
        "createdAt": iso8601(now_millis()),
        "updatedAt": iso8601(now_millis()),
        "ownerPid": std::process::id(),
        "ownerBootId": process_boot_id(std::process::id() as u64),
        "steps": [],
    })));
    let handle = Arc::new(AsyncRunHandle {
        run_id: run_id.to_string(),
        status: status.clone(),
        control: Arc::new(AsyncControl::default()),
        run_dir: run_dir.clone(),
        started_ms: now_millis(),
    });
    {
        let mut current = status.write().unwrap_or_else(|e| e.into_inner());
        let step_agents: Vec<Value> = match body {
            AsyncBody::Single { spec, .. } => {
                vec![json!({ "agent": spec.agent_name, "status": "pending" })]
            }
            AsyncBody::Tasks { entries, .. } => entries
                .iter()
                .map(|e| json!({ "key": e.key, "agent": e.spec.agent_name, "status": "pending" }))
                .collect(),
            AsyncBody::Steps { steps, .. } => steps
                .iter()
                .map(|s| json!({ "agent": s.agent_name, "status": "pending" }))
                .collect(),
        };
        current["steps"] = Value::Array(step_agents);
    }
    update_status(&handle, |_| {});
    append_event(&run_dir, "run.started", json!({ "runId": run_id }));
    register_run(handle.clone());
    handle
}

/// Drive one async run to a terminal state. Called as a spawned runtime
/// task; performs the composite body, updates the status document, writes the
/// result file, notifies the parent session via `sendMessage`, and emits the
/// bus event.
pub async fn drive_run(
    handle: Arc<AsyncRunHandle>,
    mut ctx: RunCtx,
    body: AsyncBody,
    notify: AsyncNotify,
) {
    // Cooperative stop: honor a request that arrived before the child spawn.
    if handle.control.stop_requested() {
        finish_stopped(&handle, &notify).await;
        return;
    }
    // TE09 FR-C: single/chain runs mirror each child's currentTool/currentPath
    // into their status steps so `subagent_wait` shows live tool lines
    // (upstream asyncWaitUpdate reads step.currentTool, subagent-wait.ts:327).
    // The dispatch-owned frame sink never crosses into the background task.
    ctx.frame_sink = None;
    if matches!(body, AsyncBody::Single { .. } | AsyncBody::Steps { .. }) {
        let status_handle = handle.clone();
        ctx.step_status = Some(std::sync::Arc::new(move |index: u32, activity: &Value| {
            set_step_field(&status_handle, index as usize, |step| {
                step["currentTool"] = activity["currentTool"].clone();
                step["currentPath"] = activity["currentPath"].clone();
                if let Some(started_at) = activity["currentToolStartedAt"].as_u64() {
                    step["currentToolStartedAt"] = json!(started_at);
                }
                if let Some(turns) = activity["turnCount"].as_u64() {
                    step["turnCount"] = json!(turns);
                }
                if let Some(tools) = activity["toolCount"].as_u64() {
                    step["toolCount"] = json!(tools);
                }
            });
        }));
    }
    let agents = match ctx.discover("both") {
        Ok(agents) => agents,
        Err(error) => {
            finish_failed(&handle, &error, &notify).await;
            return;
        }
    };
    match body {
        AsyncBody::Single { mut spec } => {
            let agent = match crate::agents::discover::resolve_agent_name(&agents, &spec.agent_name)
            {
                Ok(Some(agent)) => agent.clone(),
                Ok(None) => {
                    finish_failed(
                        &handle,
                        &format!("Unknown agent: {}", spec.agent_name),
                        &notify,
                    )
                    .await;
                    return;
                }
                Err(message) => {
                    finish_failed(&handle, &message, &notify).await;
                    return;
                }
            };
            // The run dir steer inbox is the async control surface; every
            // other field keeps its foreground meaning (B2: no silent loss).
            spec.steer_inbox = Some(steer_inbox_dir(&handle.run_dir, 0));
            mark_step(&handle, 0, "running");
            match crate::p1::launch_child::run_child_async(&spec, &agent, &ctx).await {
                Ok(outcome) => {
                    record_step_result(&handle, 0, &outcome.result);
                    if let Some(session_file) = &outcome.result.session_file {
                        set_step_field(&handle, 0, |step| {
                            step["sessionFile"] = json!(session_file.to_string_lossy());
                        });
                    }
                    let ok = outcome.result.exit_code == 0;
                    finish(
                        &handle,
                        if ok { STATE_COMPLETE } else { STATE_FAILED },
                        &outcome.result.final_output,
                        &notify,
                    )
                    .await;
                }
                Err(error) => finish_failed(&handle, &error, &notify).await,
            }
        }
        AsyncBody::Tasks {
            entries,
            concurrency,
            worktree_plan,
        } => {
            for (index, _) in entries.iter().enumerate() {
                mark_step(&handle, index, "queued");
            }
            let outcome = crate::p1::parallel::run_parallel_async(
                &entries,
                &agents,
                &ctx,
                concurrency,
                worktree_plan.clone(),
            )
            .await;
            // Parallel handoff manifest path rides the completion
            // notification (`SubagentNotifyDetails.handoffPath`).
            let handoff_path = worktree_plan.as_ref().and_then(|plan| {
                crate::p1::parallel::finalize_worktree_handoff(plan, &ctx.run_id, &ctx.base_cwd)
            });
            match outcome {
                Ok(outcomes) => {
                    let mut aggregate = String::new();
                    for (index, outcome) in outcomes.iter().enumerate() {
                        let state = if outcome.exit_code == 0 {
                            "complete"
                        } else {
                            "failed"
                        };
                        mark_step(&handle, index, state);
                        set_step_field(&handle, index, |step| {
                            step["exitCode"] = json!(outcome.exit_code);
                            if let Some(error) = &outcome.error {
                                step["error"] = json!(error);
                            }
                        });
                        aggregate.push_str(outcome.details["finalOutput"].as_str().unwrap_or(""));
                        aggregate.push('\n');
                    }
                    let any_failed = outcomes.iter().any(|o| o.exit_code != 0);
                    finish_with_handoff(
                        &handle,
                        if any_failed {
                            STATE_FAILED
                        } else {
                            STATE_COMPLETE
                        },
                        aggregate.trim(),
                        handoff_path.as_deref(),
                        &notify,
                    )
                    .await;
                }
                Err(error) => finish_failed(&handle, &error, &notify).await,
            }
        }
        AsyncBody::Steps {
            steps,
            original_task,
        } => {
            mark_step(&handle, 0, "running");
            match crate::p1::chain::run_chain_async(&steps, &agents, &ctx, &original_task).await {
                Ok((completed, failed)) => {
                    for step in &completed {
                        mark_step(
                            &handle,
                            step.index,
                            if step.exit_code == 0 {
                                "complete"
                            } else {
                                "failed"
                            },
                        );
                        set_step_field(&handle, step.index, |target| {
                            target["exitCode"] = json!(step.exit_code);
                            if let Some(error) = &step.error {
                                target["error"] = json!(error);
                            }
                        });
                    }
                    let last_output = completed
                        .last()
                        .map(|s| s.output.clone())
                        .unwrap_or_default();
                    match failed {
                        Some(failure) => {
                            mark_step(&handle, failure.index, "failed");
                            finish_failed(
                                &handle,
                                failure.error.as_deref().unwrap_or("chain step failed"),
                                &notify,
                            )
                            .await
                        }
                        None => finish(&handle, STATE_COMPLETE, &last_output, &notify).await,
                    }
                }
                Err(error) => finish_failed(&handle, &error, &notify).await,
            }
        }
    }
}

fn mark_step(handle: &Arc<AsyncRunHandle>, index: usize, state: &str) {
    update_status(handle, |status| {
        if let Some(steps) = status["steps"].as_array_mut() {
            if let Some(step) = steps.get_mut(index) {
                step["status"] = json!(state);
            }
        }
    });
}

fn set_step_field(handle: &Arc<AsyncRunHandle>, index: usize, mutate: impl FnOnce(&mut Value)) {
    update_status(handle, |status| {
        if let Some(steps) = status["steps"].as_array_mut() {
            if let Some(step) = steps.get_mut(index) {
                mutate(step);
            }
        }
    });
}

fn record_step_result(
    handle: &Arc<AsyncRunHandle>,
    index: usize,
    result: &crate::runner::foreground::ForegroundRunResult,
) {
    set_step_field(handle, index, |step| {
        step["status"] = if result.exit_code == 0 {
            json!("complete")
        } else {
            json!("failed")
        };
        step["exitCode"] = json!(result.exit_code);
        if let Some(model) = &result.model {
            step["model"] = json!(model);
        }
        if !result.attempted_models.is_empty() {
            step["attemptedModels"] = json!(result.attempted_models);
        }
        if let Some(error) = &result.error {
            step["error"] = json!(error);
        }
        // Terminal steps drop the live-activity fields (upstream shows
        // currentTool only for pending/running steps).
        if let Some(object) = step.as_object_mut() {
            for key in [
                "currentTool",
                "currentToolArgs",
                "currentToolStartedAt",
                "currentPath",
            ] {
                object.remove(key);
            }
        }
    });
}

/// Drive a run and release its capacity slot on any terminal path — named
/// async fn (an async *block* wrapping `drive_run` trips rustc's HRTB-closure
/// limitation, rust#89937).
pub async fn drive_run_and_release(
    handle: Arc<AsyncRunHandle>,
    ctx: RunCtx,
    body: AsyncBody,
    notify: AsyncNotify,
    session_id: Option<String>,
    slot_dir: PathBuf,
) {
    drive_run(handle, ctx, body, notify).await;
    let capacity = ActiveAsyncCapacity::open(session_id.as_deref().unwrap_or("no-session"));
    capacity.release(&slot_dir);
}

/// Completion notification channel (notify.ts): result file first, then
/// `sendMessage`; the result file is removed after the message is accepted.
#[derive(Clone)]
pub struct AsyncNotify {
    pub calls: Option<crate::AsyncHostCalls>,
}

impl AsyncNotify {
    /// `sendCompletion` (notify.ts:169-187 + buildCompletionDetails
    /// :196-238) over the rpi status document. The wire shape is upstream's:
    /// `{customType: "subagent-notify", content, display}` — the details
    /// live inside the content text and the renderer re-parses
    /// (`parseSubagentNotifyContent`). `display` is true for anything a
    /// completed background run would not show inline (upstream:
    /// `source === "foreground" || status !== "completed"`; rpi has no
    /// detached foreground face, so just the status test).
    async fn send(
        &self,
        handle: &Arc<AsyncRunHandle>,
        state: &str,
        output: &str,
        handoff_path: Option<&Path>,
    ) {
        let status = status_snapshot(handle);
        let run_id = &handle.run_id;
        let result_path = async_results_dir().join(format!("{run_id}.json"));
        let result = json!({
            "runId": run_id,
            "state": state,
            "output": output,
            "completedAt": iso8601(now_millis()),
        });
        let _ = crate::artifacts::write_metadata(&result_path, &result);

        let notify_status: &'static str = match state {
            STATE_COMPLETE => "completed",
            STATE_STOPPED => "stopped",
            _ => "failed",
        };
        // agent: the run's last declared step agent (single/chain carry the
        // child agents; a parallel batch reports its mode — upstream's
        // CompletionNotification.agent comes from the run record).
        let steps = status["steps"].as_array().cloned().unwrap_or_default();
        let agent = steps
            .iter()
            .rev()
            .find_map(|step| step["agent"].as_str().map(str::to_string))
            .unwrap_or_else(|| status["mode"].as_str().unwrap_or("unknown").to_string());
        let session_file = steps
            .iter()
            .find_map(|step| step["sessionFile"].as_str().map(str::to_string));
        let details = crate::messages::SubagentNotifyDetails {
            agent,
            status: notify_status,
            source: None,
            task_info: None,
            result_preview: output.to_string(),
            duration_ms: Some(now_millis().saturating_sub(handle.started_ms)),
            handoff_path: handoff_path.map(|p| p.to_string_lossy().to_string()),
            session_label: session_file.as_deref().map(|_| "Session file".to_string()),
            session_value: session_file,
        };
        let content = crate::messages::format_single_completion(&details);
        let display = notify_status != "completed";
        if let Some(calls) = &self.calls {
            let message = json!({
                "customType": "subagent-notify",
                "content": content,
                "display": display,
            });
            // sendMessage is synchronous through the ABI envelope; fire it
            // and treat acceptance as any non-error response. The options
            // carry `triggerTurn: true` — upstream sendCompletion always
            // sends one (notify.ts:178-182: `result.triggerTurn !== false`,
            // true by default) so a finished run wakes the parent session to
            // process the result; the rpi host default is false (a silent
            // append), which would lose that wake-up semantics.
            let response = crate::host_call_static(
                calls,
                "sendMessage",
                json!({ "message": message, "options": { "triggerTurn": true } }),
            );
            if response.get("error").is_none() {
                let _ = std::fs::remove_file(&result_path);
            }
        }
        // Bus event (subagent:async-complete) is observation-only.
        if let Some(calls) = &self.calls {
            let _ = crate::host_call_static(
                calls,
                "events.emit",
                json!({
                    "channel": "subagent:async-complete",
                    "data": { "runId": run_id, "state": state },
                }),
            );
        }
    }
}

/// `finish` without a handoff manifest (single/chain paths).
async fn finish(handle: &Arc<AsyncRunHandle>, state: &str, output: &str, notify: &AsyncNotify) {
    finish_with_handoff(handle, state, output, None, notify).await;
}

async fn finish_with_handoff(
    handle: &Arc<AsyncRunHandle>,
    state: &str,
    output: &str,
    handoff_path: Option<&Path>,
    notify: &AsyncNotify,
) {
    let stopped = handle.control.stop_requested();
    let state = if stopped { STATE_STOPPED } else { state };
    update_status(handle, |status| {
        status["state"] = json!(state);
        status["processTerminal"] = json!({
            "kind": "runner-exit",
            "at": iso8601(now_millis()),
        });
        if stopped {
            status["stopped"] = json!(true);
            status["stoppedReason"] = json!("stop-request");
        }
    });
    append_event(
        handle.run_dir.as_path(),
        "run.finished",
        json!({ "runId": handle.run_id, "state": state }),
    );
    notify.send(handle, state, output, handoff_path).await;
    // Terminal handle stays registered (see prune_terminal_runs): wait/stop
    // poll this table for the terminal transition.
    prune_terminal_runs();
}

async fn finish_failed(handle: &Arc<AsyncRunHandle>, error: &str, notify: &AsyncNotify) {
    update_status(handle, |status| {
        status["state"] = json!(STATE_FAILED);
        status["error"] = json!(error);
    });
    append_event(
        handle.run_dir.as_path(),
        "run.failed",
        json!({ "runId": handle.run_id, "error": error }),
    );
    notify.send(handle, STATE_FAILED, error, None).await;
    prune_terminal_runs();
}

async fn finish_stopped(handle: &Arc<AsyncRunHandle>, notify: &AsyncNotify) {
    finish(handle, STATE_STOPPED, "Stopped before completion.", notify).await;
}

// ---------------------------------------------------------------------------
// Control actions (control-channel.ts file semantics)
// ---------------------------------------------------------------------------

/// Steer inbox dir for child `<index>` of a run (control-channel.ts
/// `steer-targets/<index>`).
pub fn steer_inbox_dir(run_dir: &Path, index: usize) -> PathBuf {
    run_dir
        .join("control")
        .join("steer-targets")
        .join(index.to_string())
}

/// `SteerRequest` (control-channel.ts:66-77) written into the child inbox.
pub fn deliver_steer(
    run_id: &str,
    message: &str,
    mode: &str,
    target_index: Option<usize>,
) -> Result<Value, String> {
    let handle = find_active_run(run_id)
        .ok_or_else(|| format!("No active background run matches '{run_id}'."))?;
    let index = target_index.unwrap_or(0);
    let inbox = steer_inbox_dir(&handle.run_dir, index);
    std::fs::create_dir_all(&inbox).map_err(|e| e.to_string())?;
    let request_id = crate::runner::budget::random_run_id();
    let request = json!({
        "type": "steer",
        "id": request_id,
        "ts": iso8601(now_millis()),
        "message": message,
        "mode": match mode {
            "steer" | "follow_up" | "auto" => mode,
            _ => "steer",
        },
        "targetIndex": index,
    });
    let request_path = inbox.join(format!("{request_id}.json"));
    std::fs::write(&request_path, request.to_string()).map_err(|e| e.to_string())?;
    append_event(
        handle.run_dir.as_path(),
        "control.steer",
        json!({ "runId": handle.run_id, "id": request_id, "targetIndex": index }),
    );
    Ok(request)
}

/// Read a finished or live run's status.json by id (resume lookup). The id
/// is model-controlled and joins a filesystem path — path-shaped ids are
/// rejected before the lookup (C1).
pub fn read_run_status(run_id: &str) -> Option<Value> {
    if crate::paths::ensure_safe_component(run_id, "Run id").is_err() {
        return None;
    }
    if let Some(handle) = find_run(run_id) {
        return Some(status_snapshot(&handle));
    }
    let path = async_runs_dir().join(run_id).join("status.json");
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `requestAsyncInterrupt`: pause marker; the run becomes `paused` between
/// children and can be `resume`d.
pub fn interrupt_run(query: &str) -> Result<Value, String> {
    let handle =
        find_active_run(query).ok_or_else(|| format!("No active async run matches '{query}'."))?;
    handle.control.request_interrupt();
    let control_dir = handle.run_dir.join("control");
    let _ = std::fs::create_dir_all(&control_dir);
    let _ = std::fs::write(
        control_dir.join("interrupt.json"),
        json!({ "requestId": crate::runner::budget::random_run_id(), "ts": iso8601(now_millis()) })
            .to_string(),
    );
    append_event(
        handle.run_dir.as_path(),
        "control.interrupt",
        json!({ "runId": handle.run_id }),
    );
    Ok(status_snapshot(&handle))
}

/// All live runs for the status listing.
pub fn list_runs() -> Vec<Value> {
    let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    runs.values().map(|h| status_snapshot(h)).collect()
}

/// Has any run in the non-terminal states? (auto-drain probe)
fn has_outstanding_work() -> bool {
    let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    runs.values().any(|h| {
        matches!(
            status_snapshot(h)["state"].as_str().unwrap_or(""),
            STATE_QUEUED | STATE_RUNNING
        )
    })
}

/// `drainOutstandingWork` (auto-drain.ts L37-73): headless `agent_end` —
/// wait for every outstanding run to reach a terminal state, bounded by the
/// total drain budget (default 30min). Returns an error on timeout.
pub async fn drain_outstanding_work(timeout_ms: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while has_outstanding_work() {
        if std::time::Instant::now() >= deadline {
            return Err(
                "Timed out waiting for background subagent runs to finish before exit.".to_string(),
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

/// Session-shutdown harvest (ADR-0019 interactive branch): stop everything,
/// bounded wait, then the P0 shutdown sweep reaps the children.
pub async fn harvest_for_shutdown() {
    let handles: Vec<Arc<AsyncRunHandle>> = {
        let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        runs.values().cloned().collect()
    };
    for handle in &handles {
        handle.control.request_stop();
        crate::runner::foreground::request_stop_for_run(&handle.run_id);
        update_status(handle, |status| {
            if status["state"].as_str() == Some(STATE_RUNNING)
                || status["state"].as_str() == Some(STATE_QUEUED)
            {
                status["state"] = json!(STATE_STOPPED);
                status["stopped"] = json!(true);
                status["stoppedReason"] = json!("host-shutdown");
            }
        });
        append_event(
            &handle.run_dir,
            "control.stop",
            json!({ "runId": handle.run_id, "reason": "host-shutdown" }),
        );
        unregister_run(&handle.run_id);
    }
}

/// Stale-run reconciliation at plugin init (ADR-0019 crash branch): runs
/// stuck in queued/running whose owner host pid is dead get their recorded
/// children signalled (SIGTERM → bounded poll → SIGKILL, bootId-guarded
/// against pid reuse) and are then marked failed(stale).
pub fn reconcile_stale_runs() {
    let Ok(entries) = std::fs::read_dir(async_runs_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let run_dir = entry.path();
        let status_path = run_dir.join("status.json");
        let Ok(raw) = std::fs::read_to_string(&status_path) else {
            continue;
        };
        let Ok(mut status) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if !matches!(
            status["state"].as_str().unwrap_or(""),
            STATE_QUEUED | STATE_RUNNING
        ) {
            continue;
        }
        let owner_alive = status["ownerPid"]
            .as_u64()
            .map(|pid| pid_matches(pid, status["ownerBootId"].as_u64()))
            .unwrap_or(false);
        if owner_alive {
            // Another live host process owns it — do not touch.
            continue;
        }
        // Reap the orphaned children (ADR-0019: "signal and reap"): the
        // recorded pids survive the host crash in an independent process
        // group and would otherwise hang forever on the full stdout pipe.
        let mut reaped = Vec::new();
        for (pid, boot_id) in recorded_child_pids(&run_dir) {
            if !pid_matches(pid, boot_id) {
                continue;
            }
            reap_orphan_pid(pid);
            reaped.push(pid);
        }
        status["state"] = json!(STATE_FAILED);
        status["error"] = json!("stale: owning host process exited before the run finished");
        status["updatedAt"] = json!(iso8601(now_millis()));
        let _ = crate::artifacts::write_metadata(&status_path, &status);
        append_event(
            &run_dir,
            "run.reconciled",
            json!({ "reason": "owner-dead", "reapedChildPids": reaped }),
        );
    }
}

/// Signal ladder for one orphaned child pid, bounded (the reconciler runs on
/// the plugin init path): SIGTERM, poll up to 1s, then SIGKILL.
fn reap_orphan_pid(pid: u64) {
    #[cfg(unix)]
    {
        // Safety: kill(2) with a checked pid.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        while std::time::Instant::now() < deadline {
            if !pid_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Wait for runs to reach a terminal state (`subagent_wait` core,
/// subagent-wait.ts waitForSubagents subset): first-terminal (default) or
/// all-terminal, bounded by `timeout_ms`.
pub async fn wait_for_runs(
    id: Option<&str>,
    all: bool,
    timeout_ms: u64,
    on_update: Option<&(dyn Fn(&str) + Send + Sync)>,
    is_aborted: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms.max(1));
    let is_terminal = |state: &str| {
        matches!(
            state,
            STATE_COMPLETE | STATE_FAILED | STATE_STOPPED | STATE_PAUSED | STATE_REJECTED
        )
    };
    // subagent-wait.ts:552-566: the wait tracks the runs ACTIVE WHEN IT
    // STARTED (`initialAsyncIds`) — runs launched during the wait never
    // join, and runs already terminal before it are not waitable (the id
    // path picks from the active candidates only, :518-545).
    let initial_ids: Vec<String> = {
        let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        match id {
            Some(query) => {
                let resolve = || {
                    runs.values()
                        .find(|h| h.run_id == query)
                        .or_else(|| {
                            let mut prefix_matches: Vec<_> = runs
                                .values()
                                .filter(|h| h.run_id.starts_with(query))
                                .collect();
                            if prefix_matches.len() == 1 {
                                prefix_matches.pop()
                            } else {
                                None
                            }
                        })
                        .filter(|h| {
                            let status = h.status.read().unwrap_or_else(|e| e.into_inner());
                            !is_terminal(status["state"].as_str().unwrap_or(""))
                        })
                        .map(|h| h.run_id.clone())
                };
                match resolve() {
                    Some(run_id) => vec![run_id],
                    // Unknown or already-terminal id (subagent-wait.ts:549).
                    None => {
                        return Err(format!(
                            "No active run matched \"{query}\". Nothing to wait for."
                        ))
                    }
                }
            }
            None => runs
                .values()
                .filter(|h| {
                    let status = h.status.read().unwrap_or_else(|e| e.into_inner());
                    !is_terminal(status["state"].as_str().unwrap_or(""))
                })
                .map(|h| h.run_id.clone())
                .collect(),
        }
    };
    if initial_ids.is_empty() {
        // subagent-wait.ts:548-551 (no provider items in rpi).
        return Err(
            "No active async runs or registered provider work in this session. Nothing to wait for."
                .to_string(),
        );
    }
    loop {
        // User abort (extension-ABI abort-channel gap): the wait tool is a
        // synchronous dispatch the runtime cannot cancel, so it polls the
        // probe each cycle and returns promptly. The runs themselves keep
        // going — an aborted wait interrupts the *wait*, not the work
        // (upstream semantics: abort rejects the wait promise only).
        if let Some(is_aborted) = is_aborted {
            if is_aborted() {
                let snapshots: Vec<Value> = {
                    let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
                    initial_ids
                        .iter()
                        .filter_map(|run_id| runs.get(run_id.as_str()).map(|h| status_snapshot(h)))
                        .collect()
                };
                return Ok(json!({
                    "waited": 0,
                    "all": all,
                    "aborted": true,
                    "runs": snapshots,
                }));
            }
        }
        let snapshots: Vec<Value> = {
            // Single lock acquisition: `find_run` takes the same mutex and
            // std::sync::Mutex is not reentrant (deadlock otherwise).
            let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
            initial_ids
                .iter()
                .filter_map(|run_id| runs.get(run_id.as_str()).map(|h| status_snapshot(h)))
                .collect()
        };
        let terminal: Vec<&Value> = snapshots
            .iter()
            .filter(|s| is_terminal(s["state"].as_str().unwrap_or("")))
            .collect();
        if !terminal.is_empty() && (!all || terminal.len() == snapshots.len()) {
            return Ok(json!({
                "waited": terminal.len(),
                "all": all,
                "runs": terminal,
            }));
        }
        if let Some(on_update) = on_update {
            // Only the still-active subset of the initial set renders
            // (subagent-wait.ts:578-581 `activeInitialRuns`).
            let still_active: Vec<Value> = snapshots
                .iter()
                .filter(|s| !is_terminal(s["state"].as_str().unwrap_or("")))
                .cloned()
                .collect();
            on_update(&async_wait_update(
                &still_active,
                0,
                started.elapsed().as_millis() as u64,
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(json!({
                "waited": 0,
                "all": all,
                "timedOut": true,
                "runs": snapshots,
            }));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `asyncWaitUpdate` (subagent-wait.ts:322-338) over rpi status snapshots:
/// the headline plus one activity line per active step, joined with " · ",
/// then the run list below. rpi has no background provider items, so
/// `provider_count` is always 0 (the template keeps the upstream shape).
pub fn async_wait_update(runs: &[Value], provider_count: usize, elapsed_ms: u64) -> String {
    let mut headline_parts = vec![format!(
        "Waiting {} for {} async run(s) and {} provider item(s).",
        crate::runner::display::format_duration(elapsed_ms),
        runs.len(),
        provider_count,
    )];
    for run in runs {
        let steps = run["steps"].as_array().cloned().unwrap_or_default();
        let active: Vec<&Value> = steps
            .iter()
            .filter(|step| matches!(step["status"].as_str(), Some("pending") | Some("running")))
            .collect();
        if active.is_empty() {
            headline_parts.push(format!(
                "{}: {}",
                run["runId"].as_str().unwrap_or_default(),
                run["state"].as_str().unwrap_or_default(),
            ));
            continue;
        }
        for step in active {
            let current = match step["currentTool"].as_str() {
                Some(tool) => tool.to_string(),
                None if step["status"].as_str() == Some("pending") => "queued".to_string(),
                None => "thinking…".to_string(),
            };
            let mut line = format!("{}: {current}", step["agent"].as_str().unwrap_or_default());
            if let Some(path) = step["currentPath"].as_str() {
                line.push_str(&format!(" {}", crate::runner::display::shorten_path(path)));
            }
            headline_parts.push(line);
        }
    }
    let headline = headline_parts.join(" · ");
    let run_list = if runs.is_empty() {
        String::new()
    } else {
        format_async_run_list(runs)
    };
    [headline, run_list]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `formatActivityFacts` subset (async-status.ts:489-503) over a status
/// step: tool + running duration, currentPath, turns, tool count. Absent
/// fields drop (upstream filters undefined).
fn format_step_activity(step: &Value, now_ms: u64) -> Option<String> {
    let mut facts: Vec<String> = Vec::new();
    if let Some(tool) = step["currentTool"].as_str() {
        let duration = step["currentToolStartedAt"]
            .as_u64()
            .map(|started| crate::runner::display::format_duration(now_ms.saturating_sub(started)));
        match duration {
            Some(duration) => facts.push(format!("tool {tool} {duration}")),
            None => facts.push(format!("tool {tool}")),
        }
    }
    if let Some(path) = step["currentPath"].as_str() {
        facts.push(crate::runner::display::shorten_path(path));
    }
    if let Some(turns) = step["turnCount"].as_u64() {
        facts.push(format!("{turns} turns"));
    }
    if let Some(tools) = step["toolCount"].as_u64() {
        facts.push(format!("{tools} tools"));
    }
    (!facts.is_empty()).then(|| facts.join(" | "))
}

/// `formatAsyncRunList` (async-status.ts:554-576) over the rpi status field
/// subset: run header line + per-step lines; the upstream fields rpi status
/// does not carry (cwd, context label, parallel groups, nested children,
/// output files) drop out of their segments. The `steps {n}` label follows
/// the `currentStep === undefined` branch (rpi status has no currentStep).
pub fn format_async_run_list(runs: &[Value]) -> String {
    if runs.is_empty() {
        return "No active async runs.".to_string();
    }
    let now_ms = crate::artifacts::now_millis();
    let mut lines = vec![format!("Active async runs: {}", runs.len()), String::new()];
    for run in runs {
        let steps = run["steps"].as_array().cloned().unwrap_or_default();
        lines.push(format!(
            "- {} | {} | {} | steps {}",
            run["runId"].as_str().unwrap_or_default(),
            run["state"].as_str().unwrap_or_default(),
            run["mode"].as_str().unwrap_or_default(),
            steps.len(),
        ));
        for (index, step) in steps.iter().enumerate() {
            let mut parts = vec![
                format!(
                    "{}. {}",
                    index + 1,
                    step["agent"].as_str().unwrap_or_default()
                ),
                step["status"].as_str().unwrap_or_default().to_string(),
            ];
            if let Some(activity) = format_step_activity(step, now_ms) {
                parts.push(activity);
            }
            lines.push(format!("  {}", parts.join(" | ")));
        }
        if let Some(error) = run["error"].as_str() {
            lines.push(format!("  Error: {error}"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// ASYNC_RUNS is process-global; tests that register handles or wait
    /// over the "all runs" candidate set must not interleave (a concurrent
    /// running handle would join another test's wait set). Crate-visible:
    /// the TE11 fleet tests register scratch handles too and share this
    /// lock.
    pub(crate) static REGISTRY_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn spawn_budget_reserve_and_grant() {
        let session = format!("test-session-{}", std::process::id());
        let ledger = SpawnBudgetLedger::open(&session);
        let _ = std::fs::remove_file(&ledger.path);
        ledger.reserve(3, Some(5)).unwrap();
        assert_eq!(ledger.snapshot()["count"], json!(3));
        // Over the effective ceiling (configured + granted).
        assert!(ledger.reserve(3, Some(5)).is_err());
        // Grant lifts the ceiling; cumulative grant is capped at the original
        // limit (grantRemaining = 5 - 2 = 3).
        ledger.grant(2, Some(5)).unwrap();
        ledger.reserve(3, Some(5)).unwrap();
        assert_eq!(ledger.snapshot()["count"], json!(6));
        assert!(ledger.grant(4, Some(5)).is_err());
        assert!(ledger.grant(0, Some(5)).is_err());
        let _ = std::fs::remove_file(&ledger.path);
    }

    #[test]
    fn active_capacity_slots() {
        let session = format!("cap-session-{}", std::process::id());
        let capacity = ActiveAsyncCapacity::open(&session);
        let _ = std::fs::remove_dir_all(&capacity.dir);
        let a = capacity.acquire("run-a", 2).unwrap();
        let b = capacity.acquire("run-b", 2).unwrap();
        assert_ne!(a, b);
        assert!(capacity.acquire("run-c", 2).is_err());
        capacity.release(&a);
        // The freed slot is reusable.
        let c = capacity.acquire("run-c", 2).unwrap();
        capacity.release(&b);
        capacity.release(&c);
        let _ = std::fs::remove_dir_all(&capacity.dir);
    }

    #[tokio::test]
    async fn wait_for_unknown_run_errors() {
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(wait_for_runs(Some("nope"), false, 10, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn aborted_wait_returns_promptly_with_runs_kept_alive() {
        // The wait tool is a synchronous dispatch the runtime cannot cancel;
        // the abort probe is the cooperative substitute. An aborted wait must
        // return within a poll cycle and NOT touch the runs (they keep
        // running in the background — upstream aborts the wait promise only).
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handle = running_test_run("wait-aborted");
        let started = std::time::Instant::now();
        let result = wait_for_runs(Some("wait-aborted"), false, 60_000, None, Some(&|| true))
            .await
            .expect("run was active at wait start");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "aborted wait returns immediately, not after the 60s timeout"
        );
        assert_eq!(result["aborted"], json!(true));
        assert!(result.get("timedOut").is_none());
        assert_eq!(result["runs"][0]["state"], json!(STATE_RUNNING));
        // The run itself is untouched.
        let status = handle.status.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(status["state"], json!(STATE_RUNNING));
        unregister_run("wait-aborted");
    }

    #[tokio::test]
    async fn wait_returns_when_run_reaches_terminal_state() {
        // Regression: finish used to unregister the handle right after the
        // terminal transition, so the run vanished from the wait's snapshot
        // set and the wait burned its whole timeout. The terminal handle
        // must stay observable until the retention cap prunes it.
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handle = running_test_run("wait-completes");
        let waiter = tokio::spawn(async move {
            wait_for_runs(Some("wait-completes"), false, 5_000, None, None).await
        });
        // Let the wait take its initial snapshot (running), then finish the
        // run the way `finish_with_handoff` does: terminal state, handle
        // retained.
        tokio::time::sleep(Duration::from_millis(300)).await;
        {
            let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
            status["state"] = json!(STATE_COMPLETE);
        }
        let started = std::time::Instant::now();
        let result = waiter
            .await
            .expect("wait task panicked")
            .expect("run was active when the wait started");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "wait returned within a poll cycle of the terminal transition, not the 5s timeout"
        );
        assert_eq!(result["waited"], json!(1));
        assert!(result.get("timedOut").is_none());
        unregister_run("wait-completes");
    }

    #[tokio::test]
    async fn wait_all_requires_every_initial_run_terminal() {
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = running_test_run("wait-all-a");
        let second = running_test_run("wait-all-b");
        let waiter =
            tokio::spawn(async move { wait_for_runs(None, true, 2_000, None, None).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        for run_id in ["wait-all-a", "wait-all-b"] {
            let handle = find_run(run_id).unwrap();
            let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
            status["state"] = json!(STATE_COMPLETE);
        }
        let result = waiter
            .await
            .expect("wait task panicked")
            .expect("both runs were active at wait start");
        assert_eq!(result["waited"], json!(2), "{result}");
        assert!(result.get("timedOut").is_none());
        drop(first);
        drop(second);
        unregister_run("wait-all-a");
        unregister_run("wait-all-b");
    }

    #[tokio::test]
    async fn terminal_retention_is_bounded() {
        // finish keeps terminal handles for wait/status reads, but the
        // retention cap drops the oldest terminal entries.
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for index in 0..=MAX_RETAINED_TERMINAL_RUNS {
            let handle = running_test_run_started(&format!("prune-{index}"), index as u64);
            let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
            status["state"] = json!(STATE_COMPLETE);
        }
        prune_terminal_runs();
        let runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let terminal = runs
            .values()
            .filter(|h| {
                let status = h.status.read().unwrap_or_else(|e| e.into_inner());
                status["state"] == json!(STATE_COMPLETE)
            })
            .count();
        assert_eq!(
            terminal, MAX_RETAINED_TERMINAL_RUNS,
            "cap holds exactly MAX_RETAINED_TERMINAL_RUNS terminal entries"
        );
        assert!(
            runs.contains_key(&format!("prune-{MAX_RETAINED_TERMINAL_RUNS}")),
            "newest terminal run survives"
        );
        assert!(!runs.contains_key("prune-0"), "oldest terminal run pruned");
        drop(runs);
        for index in 0..=MAX_RETAINED_TERMINAL_RUNS {
            unregister_run(&format!("prune-{index}"));
        }
    }

    /// A registry handle whose status doc is in the running state.
    fn running_test_run(run_id: &str) -> Arc<AsyncRunHandle> {
        running_test_run_started(run_id, 0)
    }

    /// [`running_test_run`] with an explicit start timestamp (retention
    /// ordering tests).
    fn running_test_run_started(run_id: &str, started_ms: u64) -> Arc<AsyncRunHandle> {
        let handle = Arc::new(AsyncRunHandle {
            run_id: run_id.to_string(),
            status: Arc::new(RwLock::new(
                json!({ "runId": run_id, "state": STATE_RUNNING }),
            )),
            control: Arc::new(AsyncControl::default()),
            run_dir: std::env::temp_dir(),
            started_ms,
        });
        register_run(handle.clone());
        handle
    }

    #[tokio::test]
    async fn wait_without_active_runs_errors_instead_of_spinning() {
        // subagent-wait.ts:548-551: no active runs (a TERMINAL run left in
        // the registry does not count — :552 snapshots the active set only)
        // → the upstream "Nothing to wait for." message, not a poll loop
        // that burns the whole timeout.
        let _guard = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handle = terminal_test_run("wait-terminal");
        let error = wait_for_runs(None, false, 10, None, None)
            .await
            .expect_err("terminal-only registry has nothing to wait for");
        assert!(
            error.contains("Nothing to wait for"),
            "upstream message: {error}"
        );
        // An explicitly-requested id that is already terminal also fails
        // (the id path picks from ACTIVE candidates, :518-545).
        let error = wait_for_runs(Some("wait-terminal"), false, 10, None, None)
            .await
            .expect_err("terminal id is not waitable");
        assert!(error.contains("No active run matched"), "{error}");
        unregister_run(&handle.run_id);
    }

    /// A registry handle whose status doc is already terminal.
    fn terminal_test_run(run_id: &str) -> Arc<AsyncRunHandle> {
        let handle = Arc::new(AsyncRunHandle {
            run_id: run_id.to_string(),
            status: Arc::new(RwLock::new(
                json!({ "runId": run_id, "state": STATE_COMPLETE }),
            )),
            control: Arc::new(AsyncControl::default()),
            run_dir: std::env::temp_dir(),
            started_ms: 0,
        });
        register_run(handle.clone());
        handle
    }

    #[test]
    fn stale_reconciler_marks_dead_owner_runs() {
        let dir = async_runs_dir().join(format!("recon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::artifacts::write_metadata(
            &dir.join("status.json"),
            &json!({
                "runId": "deadrun",
                "state": STATE_RUNNING,
                "ownerPid": 4000000, // not a live pid on this system
            }),
        )
        .unwrap();
        reconcile_stale_runs();
        let status: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["state"], json!(STATE_FAILED));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stale_reconciler_reaps_orphaned_child_processes() {
        use std::process::Command;
        // A stand-in for a subagent child that outlived its crashed host:
        // independent process group, still running.
        let mut orphan = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = orphan.id() as u64;
        let dir = async_runs_dir().join(format!("recon-orphan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // children.jsonl first: status.json is the reconciler's entry
        // condition, and a concurrent test's reconcile sweep must not mark
        // the run terminal before the pid record exists.
        crate::artifacts::append_jsonl(
            &dir.join("children.jsonl"),
            &json!({ "pid": pid, "bootId": process_boot_id(pid) }).to_string(),
        );
        crate::artifacts::write_metadata(
            &dir.join("status.json"),
            &json!({
                "runId": "orphans",
                "state": STATE_RUNNING,
                "ownerPid": 4000000, // not a live pid on this system
            }),
        )
        .unwrap();
        reconcile_stale_runs();
        // The recorded pid was signalled through the ladder. The sleep child
        // is this test process's own child, so after SIGTERM it lingers as a
        // zombie until waited — poll try_wait (pid_alive stays true for
        // zombies, kill(pid,0) succeeds).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let exited = loop {
            match orphan.try_wait().expect("try_wait") {
                Some(_) => break true,
                None if std::time::Instant::now() >= deadline => break false,
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        assert!(exited, "orphan child should have been reaped");
        let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
        assert!(events.contains("run.reconciled"));
        assert!(events.contains(&format!("{pid}")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stale_reconciler_skips_reused_pids() {
        use std::process::Command;
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as u64;
        let dir = async_runs_dir().join(format!("recon-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // bootId deliberately wrong → the live pid must not be signalled.
        // Written before status.json for the same race reason as above.
        crate::artifacts::append_jsonl(
            &dir.join("children.jsonl"),
            &json!({ "pid": pid, "bootId": 1 }).to_string(),
        );
        crate::artifacts::write_metadata(
            &dir.join("status.json"),
            &json!({
                "runId": "reuse",
                "state": STATE_RUNNING,
                "ownerPid": 4000000,
            }),
        )
        .unwrap();
        reconcile_stale_runs();
        std::thread::sleep(Duration::from_millis(200));
        assert!(pid_alive(pid), "mismatched bootId must be left alone");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_update_headline_and_run_list() {
        // subagent-wait.ts:322-338: headline + per-step activity lines
        // joined with " · ", run list below.
        let running = json!({
            "runId": "ab12cd34",
            "state": "running",
            "mode": "single",
            "steps": [
                { "agent": "scout", "status": "running", "currentTool": "read", "currentPath": "/tmp/a.rs" },
            ],
        });
        let text = async_wait_update(std::slice::from_ref(&running), 0, 2_500);
        assert!(
            text.starts_with(
                "Waiting 2.5s for 1 async run(s) and 0 provider item(s). · scout: read /tmp/a.rs"
            ),
            "{text}"
        );
        assert!(text.contains("\nActive async runs: 1"));
        assert!(text.contains("- ab12cd34 | running | single | steps 1"));
        assert!(text.contains("  1. scout | running"));

        // Steps without currentTool project queued/thinking (pending →
        // "queued", running → "thinking…").
        let pending = json!({
            "runId": "ff001122",
            "state": "running",
            "mode": "chain",
            "steps": [
                { "agent": "scout", "status": "complete" },
                { "agent": "worker", "status": "pending" },
                { "agent": "worker", "status": "running" },
            ],
        });
        let text = async_wait_update(&[pending], 0, 90_000);
        assert!(text.contains("worker: queued"), "{text}");
        assert!(text.contains("worker: thinking…"), "{text}");
        assert!(text.contains("Waiting 1m30s for 1 async run(s)"), "{text}");

        // Empty input: headline only (no run list).
        let text = async_wait_update(&[], 0, 100);
        assert_eq!(
            text,
            "Waiting 100ms for 0 async run(s) and 0 provider item(s)."
        );
    }

    #[test]
    fn run_list_activity_facts() {
        // async-status.ts formatStepLine subset: status + activity facts
        // (tool duration, path, turns, tools).
        let run = json!({
            "runId": "ab12cd34",
            "state": "running",
            "mode": "single",
            "steps": [
                {
                    "agent": "scout",
                    "status": "running",
                    "currentTool": "read",
                    "currentToolStartedAt": crate::artifacts::now_millis().saturating_sub(2_500),
                    "currentPath": "/tmp/a.rs",
                    "turnCount": 3,
                    "toolCount": 7,
                },
            ],
        });
        let text = format_async_run_list(&[run]);
        assert!(
            text.contains("1. scout | running | tool read 2.5s | /tmp/a.rs | 3 turns | 7 tools"),
            "{text}"
        );
    }

    #[test]
    fn record_child_pid_writes_only_for_async_runs() {
        // The async-runs root holds no directory for this id → no file.
        record_child_pid("no-such-run-id-xyz", 4242);
        assert!(!async_runs_dir()
            .join("no-such-run-id-xyz")
            .join("children.jsonl")
            .exists());
    }
}
