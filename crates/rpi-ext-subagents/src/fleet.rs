//! TE11 FR-C: the fleet status widget — the persistent "who is running"
//! strip below the editor (`ui.setWidget`, Component form, rpi-own design;
//! upstream `tui/fleet-status.ts` is the information reference only).
//!
//! Architecture (task doc FR-C, 2026-08-16 revision):
//! - **Process-wide singleton refresh task** on the plugin runtime: spawned
//!   at the async-run 0→1 boundary (`dispatch_async` →
//!   [`ensure_refresh_loop`]); ticks every 500 ms, snapshots `ASYNC_RUNS`,
//!   compares the dirty-key, and re-pushes the widget only on change.
//!   Empty snapshot (no active run, no in-window terminal run) removes the
//!   widget and the task exits — the next 0→1 boundary respawns it.
//! - **Lingering terminal runs**: recently finished runs stay visible for
//!   the linger window (≤ `MAX_TERMINAL_ROWS`) — the window is tracked
//!   as "when this loop first saw the run terminal", not parsed from the
//!   status document's ISO timestamps.
//! - **Flicker safety** is the host's differential renderer; the dirty-key
//!   is a quiet-period cost optimization (during activity the duration
//!   fields change every tick and the key intentionally never matches).
//! - Rows are single-column concatenations with `truncate` (TE11 FR-E.2) —
//!   no width awareness, no resize subscription (TE11 Out).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::runner::background::{
    AsyncRunHandle, ASYNC_RUNS, STATE_PAUSED, STATE_QUEUED, STATE_RUNNING,
};
use crate::{config, host_call_static, AsyncHostCalls, PluginRuntime};

/// The widget key (upstream `FLEET_STATUS_WIDGET_KEY`; the host namespaces
/// it per-extension — TE11 FR-E.1).
pub const FLEET_WIDGET_KEY: &str = "subagent-fleet-status";

/// Refresh cadence (upstream `REFRESH_MS`).
const REFRESH_MS: u64 = 500;
/// How long a terminal run stays on the strip after this loop first sees
/// it terminal (task doc: ≤3 runs, 60 s). Resolution order: the atomic
/// test seam, then env (`RPI_SUBAGENT_FLEET_LINGER_MS`), then the default —
/// the atomics exist because env updates race the loop's worker thread.
static LINGER_MS_TEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

fn terminal_linger_ms() -> u64 {
    const DEFAULT: u64 = 60_000;
    let forced = LINGER_MS_TEST.load(Ordering::SeqCst);
    if forced != u64::MAX {
        return forced;
    }
    std::env::var("RPI_SUBAGENT_FLEET_LINGER_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT)
}

/// Test seam: force the linger window (`0` = expire terminal rows
/// immediately; `u64::MAX` restores env/default resolution).
#[doc(hidden)]
pub fn set_linger_for_test(ms: u64) {
    LINGER_MS_TEST.store(ms, Ordering::SeqCst);
}

const MAX_TERMINAL_ROWS: usize = 3;
/// Expanded tree row cap (upstream `MAX_AGENT_ROWS`).
const MAX_TREE_ROWS: usize = 6;

/// Singleton guard: true while a refresh task is alive (the loop clears it
/// on exit so the next 0→1 boundary can respawn).
static FLEET_LOOP_RUNNING: AtomicBool = AtomicBool::new(false);

/// One run's strip row.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FleetRow {
    pub run_id: String,
    /// single/parallel/chain (the status document's `mode`).
    pub mode: String,
    /// Child count behind this run (steps length, floor 1) — the collapsed
    /// summary counts AGENTS across runs, not runs (2026-08-16 live-session
    /// fix: a tasks[] run with two researchers reads "2 agents").
    pub agents: usize,
    /// Display label: the single agent, or `N × agents` for composites.
    pub agent_label: String,
    pub state: String,
    pub duration_ms: u64,
    /// The live child activity (`⎿ grep …`) while running, if any.
    pub current_tool: Option<String>,
}

/// Everything one push renders from.
#[derive(Clone, Debug, Default)]
pub struct FleetSnapshot {
    pub active: Vec<FleetRow>,
    /// Recently-terminal rows still inside the linger window.
    pub lingering: Vec<FleetRow>,
    /// Configured `maxActiveAsyncRunsPerSession` (`None` = unlimited).
    pub limit: Option<u64>,
}

impl FleetSnapshot {
    pub fn is_empty(&self) -> bool {
        self.active.is_empty() && self.lingering.is_empty()
    }

    /// Aggregate row count for the summary line.
    pub fn run_count(&self) -> usize {
        self.active.len() + self.lingering.len()
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Capture the current fleet state from `ASYNC_RUNS`. `linger` is the
/// loop-owned map of run id → (when this loop first saw it terminal, the
/// run's duration frozen at that moment); it is updated in place
/// (transitions recorded, expired ids dropped) so the window is measured
/// from observation, not from ISO timestamp parsing. Freezing the terminal
/// duration keeps the quiet period quiet: the row text — and with it the
/// dirty-key — stops changing once every run has settled.
pub fn capture_fleet(now_ms: u64, linger: &mut HashMap<String, (u64, u64)>) -> FleetSnapshot {
    let linger_ms = terminal_linger_ms();
    let runs: Vec<Arc<AsyncRunHandle>> = {
        let registry = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        registry.values().cloned().collect()
    };
    let mut active = Vec::new();
    let mut terminal: Vec<(u64, FleetRow)> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    for handle in runs {
        let status = handle
            .status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let state = status
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let mut row = fleet_row(&handle.run_id, &status, handle.started_ms, now_ms);
        seen_ids.push(handle.run_id.clone());
        match state.as_str() {
            STATE_QUEUED | STATE_RUNNING => active.push(row),
            _ => {
                let entry = linger
                    .entry(handle.run_id.clone())
                    .or_insert((now_ms, row.duration_ms));
                if now_ms.saturating_sub(entry.0) <= linger_ms {
                    row.duration_ms = entry.1;
                    terminal.push((entry.0, row));
                }
            }
        }
    }
    // Drop linger entries only for runs that left the registry (pruned).
    // Expiry is a visibility condition above, NOT a map-removal: removing
    // expired entries here would let the next tick re-insert them as
    // "first sight" and oscillate visible/invisible forever (with multiple
    // runs interleaving phases, the empty snapshot — and the widget
    // removal — would never be reached).
    linger.retain(|run_id, _| seen_ids.contains(run_id));
    // Newest terminal runs first, capped.
    terminal.sort_by_key(|&(seen, _)| std::cmp::Reverse(seen));
    let lingering: Vec<FleetRow> = terminal
        .into_iter()
        .take(MAX_TERMINAL_ROWS)
        .map(|(_, row)| row)
        .collect();
    FleetSnapshot {
        active,
        lingering,
        limit: config::load_config().max_active_async_runs_per_session(),
    }
}

/// Project one run's status document onto a strip row.
fn fleet_row(run_id: &str, status: &Value, started_ms: u64, now_ms: u64) -> FleetRow {
    let mode = status
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("single")
        .to_string();
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let steps = status
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agents: Vec<String> = steps
        .iter()
        .filter_map(|step| step.get("agent").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let agents_count = steps.len().max(1);
    let agent_label = match agents.len() {
        0 => "subagent".to_string(),
        1 => agents[0].clone(),
        n => format!("{n} agents"),
    };
    // The live child activity: the first running step's currentTool.
    let current_tool = steps.iter().find_map(|step| {
        let step_state = step.get("status").and_then(Value::as_str)?;
        if step_state != "running" && step_state != STATE_RUNNING {
            return None;
        }
        step.get("currentTool")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    FleetRow {
        run_id: run_id.to_string(),
        mode,
        agents: agents_count,
        agent_label,
        state,
        duration_ms: now_ms.saturating_sub(started_ms),
        current_tool,
    }
}

/// The dirty-key: every displayed field serialized (durations included —
/// during activity the key never matches, by design; the quiet period is
/// where it saves the re-push).
pub fn dirty_key(snapshot: &FleetSnapshot) -> String {
    serde_json::to_string(&(&snapshot.active, &snapshot.lingering, snapshot.limit))
        .unwrap_or_default()
}

/// `Async runs {used}/{limit}` (unlimited renders as `∞`).
fn capacity_text(snapshot: &FleetSnapshot) -> String {
    let used = snapshot.active.len();
    match snapshot.limit {
        Some(limit) => format!("Async runs {used}/{limit}"),
        None => format!("Async runs {used}/∞"),
    }
}

/// State glyph + tone for a row.
fn row_glyph(state: &str, duration_ms: u64) -> (String, &'static str) {
    match state {
        STATE_QUEUED => ("·".to_string(), "muted"),
        STATE_RUNNING => (
            crate::render::spinner_frame(duration_ms).to_string(),
            "accent",
        ),
        STATE_PAUSED => ("■".to_string(), "warning"),
        "complete" => ("✓".to_string(), "success"),
        "failed" | "rejected" => ("✗".to_string(), "error"),
        // stopped + user-aborted family (the abort path settles as stopped).
        _ => ("■".to_string(), "warning"),
    }
}

/// The widget ComponentTree. Collapsed: one summary line. Expanded: up to
/// `MAX_TREE_ROWS` per-run lines (active first, then lingering) plus the
/// capacity footer. Every line truncates (single-column concatenation,
/// stats at line end — no width awareness by design).
pub fn fleet_tree(snapshot: &FleetSnapshot, expanded: bool) -> Value {
    if !expanded {
        // Collapsed: `{N} agents · {longest active duration} · Async runs
        // {used}/{limit}` — the strip's whole state in one line.
        let longest = snapshot
            .active
            .iter()
            .map(|row| row.duration_ms)
            .max()
            .unwrap_or(0);
        // Count AGENTS (children across runs), not runs — a tasks[] run
        // with two researchers reads "2 agents" (2026-08-16 live-session
        // fix: the collapsed line used run_count and read "1 agents").
        let agent_count: usize = snapshot
            .active
            .iter()
            .chain(snapshot.lingering.iter())
            .map(|row| row.agents)
            .sum::<usize>()
            .max(snapshot.run_count());
        let line = if snapshot.active.is_empty() {
            format!("{} agents · {}", agent_count, capacity_text(snapshot))
        } else {
            format!(
                "{} agents · {} · {} running",
                agent_count,
                crate::render::format_duration(longest),
                capacity_text(snapshot)
            )
        };
        return json!({
            "type": "column",
            "props": {},
            "children": [json!({
                "type": "text",
                "props": { "text": line, "fg": "muted", "truncate": true },
            })],
        });
    }
    let mut children: Vec<Value> = Vec::new();
    let rows: Vec<&FleetRow> = snapshot
        .active
        .iter()
        .chain(snapshot.lingering.iter())
        .take(MAX_TREE_ROWS)
        .collect();
    for row in &rows {
        let (glyph, fg) = row_glyph(&row.state, row.duration_ms);
        let mut text = format!(
            "{glyph} {} · {} · {}",
            row.agent_label,
            row.state,
            crate::render::format_duration(row.duration_ms)
        );
        if let Some(tool) = &row.current_tool {
            text.push_str(&format!(" · ⎿ {tool}"));
        }
        children.push(json!({
            "type": "text",
            "props": { "text": text, "fg": fg, "truncate": true },
        }));
    }
    if snapshot.run_count() > rows.len() {
        children.push(json!({
            "type": "text",
            "props": {
                "text": format!("+{} more", snapshot.run_count() - rows.len()),
                "fg": "dim",
                "truncate": true,
            },
        }));
    }
    children.push(json!({
        "type": "text",
        "props": { "text": capacity_text(snapshot), "fg": "dim", "truncate": true },
    }));
    json!({ "type": "column", "props": {}, "children": children })
}

/// Spawn the singleton refresh task if none is alive (the async-run 0→1
/// boundary; `dispatch_async` calls this right after `start_run`).
pub fn ensure_refresh_loop(runtime: &PluginRuntime, calls: AsyncHostCalls) {
    if FLEET_LOOP_RUNNING.swap(true, Ordering::SeqCst) {
        return; // already ticking
    }
    runtime.spawn(async move {
        fleet_refresh_loop(calls).await;
    });
}

async fn fleet_refresh_loop(calls: AsyncHostCalls) {
    let mut linger: HashMap<String, (u64, u64)> = HashMap::new();
    let mut last_key = String::new();
    let mut mounted = false;
    let fleet_enabled = config::load_config().fleet_enabled();
    let fleet_expanded = config::load_config().fleet_expanded();
    loop {
        tokio::time::sleep(Duration::from_millis(REFRESH_MS)).await;
        let now = now_millis();
        let snapshot = capture_fleet(now, &mut linger);
        if snapshot.is_empty() {
            if mounted {
                let _ = set_widget(&calls, Value::Null);
            }
            break;
        }
        if !fleet_enabled {
            // Disabled at (re)start: remove any mounted widget and idle on.
            if mounted {
                let _ = set_widget(&calls, Value::Null);
                mounted = false;
            }
            continue;
        }
        let key = dirty_key(&snapshot);
        if key != last_key {
            let tree = fleet_tree(&snapshot, fleet_expanded);
            if set_widget(&calls, tree).is_none() {
                let _ = set_widget(&calls, Value::Null);
            }
            last_key = key;
            mounted = true;
        }
    }
    FLEET_LOOP_RUNNING.store(false, Ordering::SeqCst);
}

/// `ui.setWidget` push (Component form) or removal (`Value::Null`).
/// Returns `Some(())` on acceptance, `None` on an error envelope (the
/// caller treats a dead host as "remove and stop").
fn set_widget(calls: &AsyncHostCalls, content: Value) -> Option<()> {
    let response = host_call_static(
        calls,
        "ui.setWidget",
        json!({
            "key": FLEET_WIDGET_KEY,
            "content": content,
            "placement": "belowEditor",
        }),
    );
    if response.get("error").is_some() {
        tracing::warn!("fleet: ui.setWidget rejected; removing the widget");
        return None;
    }
    Some(())
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    /// The linger seam and `ASYNC_RUNS` are process-global; fleet tests
    /// pin both and register scratch handles, so they serialize against
    /// each other AND the registry's wait tests (shared
    /// `REGISTRY_TEST_MUTEX`: a fleet scratch handle must not join another
    /// test's wait set, and vice versa).
    use crate::runner::background::tests::REGISTRY_TEST_MUTEX as TEST_LOCK;

    fn row(run_id: &str, agent: &str, state: &str, duration_ms: u64) -> FleetRow {
        FleetRow {
            run_id: run_id.to_string(),
            mode: "single".to_string(),
            agents: 1,
            agent_label: agent.to_string(),
            state: state.to_string(),
            duration_ms,
            current_tool: None,
        }
    }

    #[test]
    fn collapsed_line_counts_runs_and_capacity() {
        let snapshot = FleetSnapshot {
            active: vec![row("a", "researcher", STATE_RUNNING, 5_000)],
            lingering: vec![row("b", "scout", "complete", 90_000)],
            limit: Some(4),
        };
        let tree = fleet_tree(&snapshot, false);
        let text = tree["children"][0]["props"]["text"].as_str().unwrap();
        assert!(text.contains("2 agents"), "{text}");
        assert!(text.contains("Async runs 1/4"), "{text}");
    }

    #[test]
    fn expanded_tree_lists_rows_and_footer() {
        let snapshot = FleetSnapshot {
            active: vec![
                row("a", "researcher", STATE_RUNNING, 12_500),
                row("b", "mapper", STATE_QUEUED, 0),
            ],
            lingering: vec![row("c", "scout", "complete", 40_000)],
            limit: None,
        };
        let tree = fleet_tree(&snapshot, true);
        let texts: Vec<&str> = tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["props"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts.len(), 4, "{texts:?}"); // 3 rows + footer
        assert!(
            texts[0].contains("researcher · running · 12.5s"),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.ends_with("Async runs 2/∞")),
            "{texts:?}"
        );
        // Every row truncates (single line at any width).
        for child in tree["children"].as_array().unwrap() {
            assert_eq!(child["props"]["truncate"], json!(true));
        }
    }

    #[test]
    fn expanded_tree_caps_at_six_rows_with_overflow_note() {
        let active: Vec<FleetRow> = (0..8)
            .map(|i| row(&format!("r{i}"), "worker", STATE_RUNNING, 1_000))
            .collect();
        let snapshot = FleetSnapshot {
            active,
            lingering: vec![],
            limit: None,
        };
        let tree = fleet_tree(&snapshot, true);
        let children = tree["children"].as_array().unwrap();
        assert_eq!(children.len(), 6 + 1 + 1); // cap + "+N more" + footer
        assert!(children[6]["props"]["text"]
            .as_str()
            .unwrap()
            .contains("+2 more"));
    }

    #[test]
    fn dirty_key_tracks_durations() {
        let base = FleetSnapshot {
            active: vec![row("a", "researcher", STATE_RUNNING, 5_000)],
            lingering: vec![],
            limit: None,
        };
        let mut advanced = base.clone();
        advanced.active[0].duration_ms = 5_500;
        assert_ne!(dirty_key(&base), dirty_key(&advanced));
        assert_eq!(dirty_key(&base), dirty_key(&base.clone()));
    }

    #[test]
    fn capture_partitions_active_and_lingering() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Registry-based; uses the test-native reset seam (ASYNC_RUNS is
        // process-global, tests serialize through it). Pin the window
        // explicitly — the atomics are process-global too, and parallel
        // tests may otherwise leave a 0 window in place.
        set_linger_for_test(60_000);
        let mut runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        runs.clear();
        drop(runs);
        let status_running = json!({
            "mode": "single",
            "state": STATE_RUNNING,
            "steps": [{"agent": "researcher", "status": "running", "currentTool": "grep"}],
        });
        let handle_running = spawn_test_handle("fleet-t1", status_running, 10_000);
        let status_done = json!({
            "mode": "parallel",
            "state": "complete",
            "steps": [
                {"agent": "scout", "status": "complete"},
                {"agent": "mapper", "status": "complete"},
            ],
        });
        let handle_done = spawn_test_handle("fleet-t2", status_done, 200_000);
        let mut linger = HashMap::new();
        let snapshot = capture_fleet(300_000, &mut linger);
        assert_eq!(snapshot.active.len(), 1);
        assert_eq!(snapshot.active[0].agent_label, "researcher");
        assert_eq!(snapshot.active[0].current_tool.as_deref(), Some("grep"));
        assert_eq!(snapshot.active[0].duration_ms, 290_000);
        assert_eq!(snapshot.lingering.len(), 1);
        assert_eq!(snapshot.lingering[0].agent_label, "2 agents");
        // Terminal durations freeze at first sight: the row text (and the
        // dirty-key) stays constant across ticks.
        assert_eq!(snapshot.lingering[0].duration_ms, 100_000); // 300k - 200k
        let snapshot = capture_fleet(300_500, &mut linger);
        assert_eq!(snapshot.lingering[0].duration_ms, 100_000);
        // Linger window: past first sight + the window the terminal row
        // drops (forced window via the test seam for determinism).
        set_linger_for_test(1_000);
        let mut linger = HashMap::new();
        let _ = capture_fleet(300_000, &mut linger); // first sight at t=300s
        let snapshot = capture_fleet(300_000 + 1_000 + 1, &mut linger);
        set_linger_for_test(u64::MAX);
        assert!(
            snapshot.lingering.is_empty(),
            "expired terminal row dropped"
        );
        assert!(
            snapshot.lingering.is_empty(),
            "expired terminal row dropped"
        );
        // Cleanup.
        let mut runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        runs.remove(&handle_running.run_id);
        runs.remove(&handle_done.run_id);
    }

    fn spawn_test_handle(run_id: &str, status: Value, started_ms: u64) -> Arc<AsyncRunHandle> {
        use std::sync::RwLock;
        let handle = Arc::new(AsyncRunHandle {
            run_id: run_id.to_string(),
            status: Arc::new(RwLock::new(status)),
            control: Arc::new(crate::runner::background::AsyncControl::default()),
            run_dir: std::path::PathBuf::from("/tmp/fleet-test"),
            started_ms,
        });
        ASYNC_RUNS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id.to_string(), handle.clone());
        handle
    }

    /// End-to-end probe of the refresh loop itself: a stub host channel
    /// records `ui.setWidget` calls; a running run pushes the strip, its
    /// terminal transition (linger 0) removes it and exits the loop.
    #[test]
    fn refresh_loop_pushes_then_removes_on_empty() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::Mutex;
        static CALLS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
        extern "C" fn stub_call(
            _cookie: rpi_ext_host::native::PluginCookie,
            request: abi_stable::std_types::RVec<u8>,
        ) -> abi_stable::std_types::RVec<u8> {
            let parsed: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
            if parsed["call"] == json!("ui.setWidget") {
                CALLS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(parsed["args"].clone());
            }
            abi_stable::std_types::RVec::from(
                serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
            )
        }

        {
            let mut runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
            runs.clear();
        }
        set_linger_for_test(0);
        let runtime = crate::PluginRuntime::new().expect("plugin runtime");
        let calls = crate::AsyncHostCalls {
            call: stub_call,
            cookie: 0,
        };
        let running_status = json!({
            "mode": "single",
            "state": STATE_RUNNING,
            "steps": [{"agent": "researcher", "status": "running"}],
        });
        let handle = spawn_test_handle("fleet-loop-t1", running_status, now_millis());
        ensure_refresh_loop(&runtime, calls);

        // Within a few ticks the strip pushes with content.
        let mut pushed = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(200));
            let calls_seen = CALLS.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if calls_seen.iter().any(|call| !call["content"].is_null()) {
                pushed = true;
                break;
            }
        }
        assert!(pushed, "strip pushed while the run was active");

        // Terminal transition (linger 0): the strip removes itself.
        {
            let mut status = handle.status.write().unwrap_or_else(|e| e.into_inner());
            status["state"] = json!("complete");
        }
        let mut removed = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(200));
            let calls_seen = CALLS.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if calls_seen
                .iter()
                .any(|call| call["content"].is_null() && call["key"] == json!(FLEET_WIDGET_KEY))
            {
                removed = true;
                break;
            }
        }
        assert!(
            removed,
            "strip removed on the empty snapshot; calls: {:?}",
            CALLS.lock().unwrap_or_else(|e| e.into_inner())
        );
        set_linger_for_test(u64::MAX);
        {
            let mut runs = ASYNC_RUNS.lock().unwrap_or_else(|e| e.into_inner());
            runs.clear();
        }
    }
}
