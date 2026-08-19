//! The refresh loop (TE12 FR-C / FR-C0 / FR-D / FR-H / FR-J).
//!
//! Structure mirrors the subagents fleet refresh loop (fleet.rs:357-392:
//! a private-runtime task driving UI host calls) plus the CC statusline
//! update discipline: 300ms debounce, one script in flight at a time, a
//! newer trigger cancels the in-flight script, failures keep the last
//! render. Script runs are spawned (not awaited inline) so the loop stays
//! responsive; a generation counter discards results from cancelled runs.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::{self, Placement};
use crate::payload::build_stdin_json;
use crate::render::{footer_tree, status_text};
use crate::runner::{self, CancelToken, ScriptError};
use crate::state::{EngineState, Snapshot};
use crate::{host_ok, AsyncHostCalls, ENGINE};

/// `ui.setStatus` key (the `status` placement channel). Distinct from the
/// mcp-adapter's "mcp" key so both footers can coexist.
pub const STATUS_KEY: &str = "rpi-statusline";

/// CC debounce: rapid changes batch together, the script runs once after
/// they stop.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// UI-bridge retry cadence (mcp-adapter lib.rs:463-483 precedent: install
/// runs before the TUI binds the bridge).
const BRIDGE_RETRY_INTERVAL: Duration = Duration::from_millis(1500);
const BRIDGE_RETRIES: usize = 15;

/// What wakes the loop.
#[derive(Debug)]
pub enum Trigger {
    /// A subscribed session event (or the synthetic "interval").
    Event(&'static str),
    /// `session_shutdown` — cancel in-flight work and stop.
    Shutdown,
}

/// Script completion arriving from the spawned runner task.
type ScriptDone = (usize, Result<String, ScriptError>);

/// Session facts pulled over host calls at each tick.
#[derive(Debug, Default)]
struct CtxData {
    cwd: String,
    model: Option<Value>,
    context_usage: Option<Value>,
    thinking_level: Option<String>,
    session_name: Option<String>,
}

/// Run the refresh loop until `Trigger::Shutdown` (or every senders drop).
pub async fn refresh_loop(calls: AsyncHostCalls, mut rx: UnboundedReceiver<Trigger>) {
    let (done_tx, mut done_rx): (UnboundedSender<ScriptDone>, UnboundedReceiver<ScriptDone>) =
        tokio::sync::mpsc::unbounded_channel();
    let mut inflight: Option<CancelToken> = None;
    let mut generation: usize = 0;
    let mut bridge_ready = false;
    // Set when the bridge never appeared (a UI-less subagents child
    // process): the instance stays dormant for good — FR-C0 / FR-I.
    let mut bridge_abandoned = false;
    let mut interval_deadline: Option<Instant> = None;

    loop {
        // Copy the deadline into the future (Option<Instant> is Copy) so
        // the loop body may reassign `interval_deadline` while the pinned
        // future is still alive.
        let deadline = interval_deadline;
        let interval_fut = async move {
            match deadline {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(interval_fut);

        let step = tokio::select! {
            trigger = rx.recv() => match trigger {
                None => break,
                Some(trigger) => trigger,
            },
            done = done_rx.recv() => {
                let Some(done) = done else { break };
                match handle_script_done(&calls, generation, done) {
                    DoneOutcome::Stale => { /* cancelled run: ignore */ }
                    DoneOutcome::Handled => { inflight = None; }
                }
                continue;
            }
            _ = &mut interval_fut => Trigger::Event("interval"),
        };

        match step {
            Trigger::Shutdown => {
                if let Some(token) = inflight.take() {
                    token.cancel();
                }
                break;
            }
            Trigger::Event(reason) => {
                tracing::trace!(reason, "statusline refresh trigger");
                if bridge_abandoned {
                    continue;
                }
                if !bridge_ready {
                    bridge_ready = wait_for_bridge(&calls).await;
                    if !bridge_ready {
                        bridge_abandoned = true;
                        continue;
                    }
                }
                // CC debounce: wait out the burst, then coalesce.
                tokio::time::sleep(DEBOUNCE).await;
                let shutdown = drain_triggers(&mut rx);
                if shutdown {
                    if let Some(token) = inflight.take() {
                        token.cancel();
                    }
                    break;
                }
                // A newer update cancels the in-flight script (CC
                // semantics). Bump the generation so its late result is
                // discarded even when no new run follows.
                if let Some(token) = inflight.take() {
                    token.cancel();
                    generation += 1;
                }
                if let Some(token) = refresh_tick(&calls, &done_tx, &mut generation) {
                    inflight = Some(token);
                }
                let interval = current_refresh_interval();
                interval_deadline = interval.map(|secs| Instant::now() + Duration::from_secs(secs));
            }
        }
    }
}

/// Coalesce the debounce window: drain queued triggers; report if a
/// shutdown arrived (which wins over every other trigger).
fn drain_triggers(rx: &mut UnboundedReceiver<Trigger>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(Trigger::Shutdown) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// FR-C0: poll `ctx.hasUI` until the bridge appears (bounded). `false`
/// means this instance has no UI (dormant for good).
async fn wait_for_bridge(calls: &AsyncHostCalls) -> bool {
    for _ in 0..BRIDGE_RETRIES {
        if host_ok(calls, "ctx.hasUI", json!({})).as_ref() == Some(&Value::Bool(true)) {
            return true;
        }
        tokio::time::sleep(BRIDGE_RETRY_INTERVAL).await;
    }
    tracing::warn!("rpi-statusline: UI bridge never appeared; staying dormant");
    false
}

/// The configured `refreshInterval` right now (re-read from settings so a
/// config edit applies on the next tick).
fn current_refresh_interval() -> Option<u64> {
    config::load_settings_snapshot()
        .status_line
        .and_then(|config| config.refresh_interval_secs)
}

/// One refresh pass: reload config, pull ctx, spawn the script. Returns
/// the new in-flight cancel token.
///
/// Every tick runs the script unconditionally: the seven trigger events
/// are all low-frequency user-visible operations (message/tool completion,
/// model/thinking switches, session lifecycle) already coalesced by the
/// 300ms debounce, so a snapshot-comparison short-circuit only creates
/// "changed but not refreshed" bugs — the git branch, for one, lives in
/// none of the host-visible fields (TE12 follow-up: switching branches in
/// a bash tool never re-rendered because the dirty key was unchanged).
fn refresh_tick(
    calls: &AsyncHostCalls,
    done_tx: &UnboundedSender<ScriptDone>,
    generation: &mut usize,
) -> Option<CancelToken> {
    let settings = config::load_settings_snapshot();
    let Some(config) = settings.status_line else {
        restore_if_mounted(calls);
        return None;
    };

    // Placement switch (FR-H): restore the old channel before using the
    // new one.
    let mounted = with_engine(|engine| engine.mounted);
    if mounted.is_some_and(|old| old != config.placement) {
        restore_if_mounted(calls);
    }

    let ctx = fetch_ctx(calls);
    let snapshot = snapshot_from_engine(&ctx, settings.session_dir.as_deref());

    *generation += 1;
    let cancel = CancelToken::new();
    let command = config.command.clone();
    let cwd = ctx.cwd.clone();
    let timeout_ms = config.timeout_ms;
    let stdin_json = build_stdin_json(&snapshot);
    let done_tx = done_tx.clone();
    let generation_value = *generation;
    let run_cancel = cancel.clone();
    tokio::spawn(async move {
        let result = runner::run(run_cancel, &command, &stdin_json, &cwd, timeout_ms).await;
        let _ = done_tx.send((generation_value, result));
    });
    Some(cancel)
}

/// Assemble the payload snapshot: ctx facts plus the engine's
/// accumulated state (latching the transcript path on the way through).
fn snapshot_from_engine(ctx: &CtxData, settings_session_dir: Option<&str>) -> Snapshot {
    with_engine(|engine| {
        let cwd_path = Path::new(&ctx.cwd);
        engine.ensure_transcript(cwd_path, settings_session_dir);
        Snapshot {
            cwd: ctx.cwd.clone(),
            model: ctx.model.clone(),
            context_usage: ctx.context_usage.clone(),
            thinking_level: ctx.thinking_level.clone(),
            session_name: ctx.session_name.clone(),
            totals: engine.totals(),
            last_usage: engine.last_usage(),
            session_elapsed_ms: engine.session_elapsed_ms(),
            transcript_path: engine
                .transcript_path()
                .map(|path| path.display().to_string()),
            session_id: engine.session_id().map(str::to_owned),
        }
    })
}

/// Pull the session facts over host calls (five reads; each failure falls
/// back independently).
fn fetch_ctx(calls: &AsyncHostCalls) -> CtxData {
    CtxData {
        cwd: host_ok(calls, "ctx.cwd", json!({}))
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            }),
        model: host_ok(calls, "ctx.model", json!({})).filter(|value| !value.is_null()),
        context_usage: host_ok(calls, "ctx.getContextUsage", json!({}))
            .filter(|value| !value.is_null()),
        thinking_level: host_ok(calls, "getThinkingLevel", json!({}))
            .and_then(|value| value.as_str().map(str::to_owned)),
        session_name: host_ok(calls, "getSessionName", json!({}))
            .and_then(|value| value.as_str().map(str::to_owned)),
    }
}

enum DoneOutcome {
    /// Result of a cancelled run (generation mismatch) — ignore.
    Stale,
    /// Current run handled (success pushed / failure kept last render).
    Handled,
}

/// FR-F/FR-G: on success re-read the config (it may have changed while the
/// script ran) and push through the configured channel; on failure keep
/// the previous render and warn.
fn handle_script_done(calls: &AsyncHostCalls, generation: usize, done: ScriptDone) -> DoneOutcome {
    let (run_generation, result) = done;
    if run_generation != generation {
        return DoneOutcome::Stale;
    }
    let stdout = match result {
        Ok(stdout) => stdout,
        Err(error) => {
            tracing::warn!(%error, "rpi-statusline script failed; keeping last render");
            return DoneOutcome::Handled;
        }
    };
    let settings = config::load_settings_snapshot();
    let Some(config) = settings.status_line else {
        restore_if_mounted(calls);
        return DoneOutcome::Handled;
    };
    match config.placement {
        Placement::Replace => {
            let tree = footer_tree(&stdout, config.padding);
            host_ok(calls, "ui.setFooter", json!({ "component": tree }));
            with_engine(|engine| engine.mounted = Some(Placement::Replace));
        }
        Placement::Widget => {
            // Same ComponentTree rendering as Replace, mounted between the
            // editor and the untouched built-in footer (belowEditor).
            let tree = footer_tree(&stdout, config.padding);
            host_ok(
                calls,
                "ui.setWidget",
                json!({ "key": STATUS_KEY, "content": tree, "placement": "belowEditor" }),
            );
            with_engine(|engine| engine.mounted = Some(Placement::Widget));
        }
        Placement::Status => {
            if let Some(text) = status_text(&stdout) {
                host_ok(
                    calls,
                    "ui.setStatus",
                    json!({ "key": STATUS_KEY, "text": text }),
                );
                with_engine(|engine| engine.mounted = Some(Placement::Status));
            }
            // Empty stdout in status mode: nothing to publish this round
            // (FR-G); the mounted state is unchanged.
        }
    }
    DoneOutcome::Handled
}

/// FR-H: when the config disappeared (or switched placement), restore the
/// built-in footer through whichever channel we were occupying.
fn restore_if_mounted(calls: &AsyncHostCalls) {
    let mounted = with_engine(|engine| engine.mounted.take());
    match mounted {
        Some(Placement::Replace) => {
            host_ok(calls, "ui.setFooter", json!({ "component": null }));
        }
        Some(Placement::Widget) => {
            host_ok(
                calls,
                "ui.setWidget",
                json!({ "key": STATUS_KEY, "content": null }),
            );
        }
        Some(Placement::Status) => {
            host_ok(
                calls,
                "ui.setStatus",
                json!({ "key": STATUS_KEY, "text": null }),
            );
        }
        None => {}
    }
}

/// Access the shared engine state (the dispatch thread and this loop are
/// the only users).
fn with_engine<T>(update: impl FnOnce(&mut EngineState) -> T) -> T {
    let engine = ENGINE
        .get()
        .expect("engine state installed before the refresh loop starts");
    let mut engine = engine.lock().unwrap_or_else(|error| error.into_inner());
    update(&mut engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_triggers_reports_shutdown_and_coalesces() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Trigger::Event("message_end")).ok();
        tx.send(Trigger::Event("model_select")).ok();
        assert!(!drain_triggers(&mut rx));
        tx.send(Trigger::Event("message_end")).ok();
        tx.send(Trigger::Shutdown).ok();
        tx.send(Trigger::Event("turn_end")).ok();
        assert!(drain_triggers(&mut rx));
    }
}
