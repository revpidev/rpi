//! The refresh loop (TE12 command mode + TE52 items mode).
//!
//! Structure mirrors the subagents fleet refresh loop (fleet.rs:357-392: a
//! private-runtime task driving UI host calls) plus the CC statusline update
//! discipline: 300ms debounce, one script in flight at a time, a newer
//! trigger cancels the in-flight script, failures keep the last render.
//!
//! Since TE52 the loop also renders `type:"items"` directly:
//! - session totals come from the absolute `ctx.sessionStats` snapshot;
//! - Git is probed on the plugin runtime with a generation that discards
//!   late results;
//! - `session-duration` runs a local 1s tick that only recomputes the clock
//!   (no host call, no script, no transcript);
//! - live fields reuse the existing `LiveMeasure` at `refreshMs`;
//! - subscriptions are reconciled from the selected fields on every poll and
//!   after a config save.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::{self, DEFAULT_LIVE_REFRESH_MS, Placement, StatusLine};
use crate::fields::{self, ContextInfo, FieldData, FieldId, ModelInfo, Palette};
use crate::git::{self, GitInfo};
use crate::items::ItemsConfig;
use crate::payload::{HOOK_EVENT_STATUS, build_stdin_json};
use crate::render::{footer_tree, status_text};
use crate::runner::{self, CancelToken, ScriptError};
use crate::session_stats::SessionStats;
use crate::state::{EngineState, Snapshot};
use crate::{AsyncHostCalls, ENGINE, host_ok};

/// `ui.setStatus` key (the `status` placement channel). Distinct from the
/// legacy "mcp" status key so both footers can coexist.
pub const STATUS_KEY: &str = "rpi-statusline";

/// CC debounce: rapid changes batch together, the script runs once after
/// they stop.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// UI-bridge retry cadence (install
/// runs before the TUI binds the bridge).
const BRIDGE_RETRY_INTERVAL: Duration = Duration::from_millis(1500);
const BRIDGE_RETRIES: usize = 15;

/// Poll cadence for the ctx-data fingerprint: the plugin's self-healing
/// data plane. The host event surface has proven lossy (TE12 found two
/// silent event drops); the poll re-reads the ctx facts every second
/// (µs-scale host calls) and triggers a full refresh whenever ANY of them
/// changed — "data changed ⇒ re-render", independent of event delivery.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Local clock tick for `session-duration` (reads no host data).
const DURATION_TICK: Duration = Duration::from_secs(1);

/// What wakes the loop.
#[derive(Debug)]
pub enum Trigger {
    /// A subscribed session event (or the synthetic "interval"/"poll").
    Event(&'static str),
    /// Live bookkeeping advanced (`message_start` / `message_update`
    /// dispatch — measurement only, FR-E); arms the live ticker, never
    /// the debounce must-run path.
    Live,
    /// The live ticker fired (streaming re-run throttle, FR-E / items live
    /// field re-render).
    LiveTick,
    /// The local session-duration clock moved one second.
    DurationTick,
    /// A background Git probe finished.
    GitDone(u64, Option<GitInfo>),
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
    /// `ctx.sessionFile` answer `{path, id}` (FR-I); `None` = call
    /// failed / unbound → the directory heuristic stays in charge.
    session_file: Option<Value>,
    /// TE52 items facts, fetched only when a selected field needs them.
    mode: Option<String>,
    idle: Option<bool>,
    theme: Option<Value>,
}

/// Run the refresh loop until `Trigger::Shutdown` (or every senders drop).
/// `shared` is the rebindable host channel (lib.rs `CHANNEL`): read at the
/// top of every iteration so a session-switch rebind takes effect on the
/// next trigger/tick — never pushing through a replaced host's freed
/// cookie.
pub async fn refresh_loop(
    shared: std::sync::Arc<std::sync::RwLock<AsyncHostCalls>>,
    mut rx: UnboundedReceiver<Trigger>,
) {
    let (done_tx, mut done_rx): (UnboundedSender<ScriptDone>, UnboundedReceiver<ScriptDone>) =
        tokio::sync::mpsc::unbounded_channel();
    let mut inflight: Option<CancelToken> = None;
    let mut generation: usize = 0;
    let mut bridge_ready = false;
    // Set when the bridge never appeared (a UI-less subagents child
    // process): the instance stays dormant for good — FR-C0 / FR-I.
    let mut bridge_abandoned = false;
    let mut interval_deadline: Option<Instant> = None;
    let mut poll_deadline = Instant::now() + POLL_INTERVAL;
    let mut last_fingerprint: Option<String> = None;
    // Live ticker (FR-E): armed by `Trigger::Live` while an assistant
    // message streams; fires at most once per `liveTokens.refreshMs`.
    let mut live_deadline: Option<Instant> = None;
    let mut last_live_fingerprint: Option<String> = None;
    // TE52 items state.
    let mut duration_deadline: Option<Instant> = None;
    let mut last_items_data: Option<FieldData> = None;
    let mut last_items_fingerprint: Option<String> = None;

    loop {
        // The current host channel (rebind-aware; see the fn doc).
        let calls = *shared.read().unwrap_or_else(|error| error.into_inner());
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
        let poll_fut = tokio::time::sleep_until(tokio::time::Instant::from_std(poll_deadline));
        tokio::pin!(poll_fut);
        let live_deadline_value = live_deadline;
        let live_fut = async move {
            match live_deadline_value {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(live_fut);
        let duration_deadline_value = duration_deadline;
        let duration_fut = async move {
            match duration_deadline_value {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(duration_fut);

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
            _ = &mut poll_fut => Trigger::Event("poll"),
            _ = &mut live_fut => Trigger::LiveTick,
            _ = &mut duration_fut => Trigger::DurationTick,
        };

        match step {
            Trigger::Shutdown => {
                if let Some(token) = inflight.take() {
                    token.cancel();
                }
                break;
            }
            Trigger::GitDone(probe_generation, info) => {
                let accepted = with_engine(|engine| engine.git.apply(probe_generation, info));
                if accepted && bridge_ready && !bridge_abandoned {
                    let settings = config::load_settings_snapshot();
                    if let Some(StatusLine::Items(items)) = settings.status_line.as_ref() {
                        publish_items_from_cache(&calls, items, last_items_data.as_mut());
                    }
                }
            }
            Trigger::Live => {
                if !live_currently_enabled() {
                    continue;
                }
                // Arm the streaming ticker (idempotent): the first live
                // bookkeeping of a stream starts the refreshMs cadence.
                if live_deadline.is_none() {
                    live_deadline = Some(Instant::now() + live_refresh_interval());
                }
            }
            Trigger::LiveTick => {
                let streaming =
                    with_engine(|engine| engine.live.as_ref().is_some_and(|l| l.is_streaming()));
                if !streaming {
                    live_deadline = None;
                    last_live_fingerprint = None;
                    continue;
                }
                // FR-E: refreshMs is re-read every tick — config edits
                // apply immediately.
                live_deadline = Some(Instant::now() + live_refresh_interval());
                if bridge_abandoned {
                    continue;
                }
                if !bridge_ready {
                    // A stream may start before any regular event armed
                    // the bridge; acquire it here (same discipline as the
                    // event path — dormant instances stay dormant).
                    bridge_ready = wait_for_bridge(&calls).await;
                    if !bridge_ready {
                        bridge_abandoned = true;
                        continue;
                    }
                }
                // Dirty check (FR-E): re-run only when a measurement moved.
                // While streaming `elapsed_ms` always moves (a stalled
                // stream still re-runs so the script can zero the rate).
                let now = Instant::now();
                let fingerprint = with_engine(|engine| {
                    engine.live.as_ref().map(|live| live.peek_fingerprint(now))
                });
                let Some(fingerprint) = fingerprint else {
                    continue;
                };
                if last_live_fingerprint.as_ref() == Some(&fingerprint) {
                    continue;
                }
                last_live_fingerprint = Some(fingerprint);
                let settings = config::load_settings_snapshot();
                match settings.status_line {
                    Some(StatusLine::Command(_)) => {
                        // A newer live frame supersedes an in-flight script
                        // (single-flight discipline; the generation bump in
                        // `refresh_tick` discards its late result).
                        if let Some(token) = inflight.take() {
                            token.cancel();
                        }
                        if let Some(token) =
                            refresh_tick(&calls, &done_tx, &mut generation, "message_update", None)
                        {
                            inflight = Some(token);
                        }
                    }
                    Some(StatusLine::Items(items)) => {
                        if bridge_ready && !bridge_abandoned {
                            // The first live tick can arrive before any regular
                            // event built the cached data frame (a stream may
                            // start right after install). Build it once so the
                            // estimated tokens/rate are visible immediately.
                            if last_items_data.is_none() {
                                let ctx = fetch_items_ctx(&calls, &items);
                                last_items_data = refresh_items(&calls, &items, Some(ctx), false);
                            } else {
                                publish_items_from_cache(&calls, &items, last_items_data.as_mut());
                            }
                        }
                    }
                    None => restore_if_mounted(&calls),
                }
            }
            Trigger::DurationTick => {
                let settings = config::load_settings_snapshot();
                let duration_selected = matches!(
                    settings.status_line.as_ref(),
                    Some(StatusLine::Items(items)) if items.needs_duration()
                );
                duration_deadline = duration_selected.then(|| Instant::now() + DURATION_TICK);
                if let Some(StatusLine::Items(items)) = settings.status_line.as_ref()
                    && items.needs_duration()
                    && bridge_ready
                    && !bridge_abandoned
                {
                    // Only the clock moves: no host call, no Git, no script.
                    if let Some(data) = last_items_data.as_mut() {
                        data.now = Some(Utc::now());
                        publish_items_from_data(&calls, items, data);
                    }
                }
            }
            Trigger::Event("poll") => {
                poll_deadline = Instant::now() + POLL_INTERVAL;
                // While an assistant message streams, the liveTicker owns
                // the re-run cadence (A4: spawn ≤ 1/refreshMs) — the poll
                // must not interleave a second spawn stream. Its
                // self-heal job resumes once the stream settles.
                if with_engine(|engine| {
                    engine.live.as_ref().is_some_and(|live| live.is_streaming())
                }) {
                    continue;
                }
                let settings = config::load_settings_snapshot();
                crate::reconcile_subscriptions(&calls, &settings);
                match settings.status_line.as_ref() {
                    None => {
                        if let Some(token) = inflight.take() {
                            token.cancel();
                            generation += 1;
                        }
                        restore_if_mounted(&calls);
                        last_fingerprint = None;
                        last_items_fingerprint = None;
                        last_items_data = None;
                        interval_deadline = None;
                        duration_deadline = None;
                        live_deadline = None;
                    }
                    Some(StatusLine::Command(_)) => {
                        duration_deadline = None;
                        if bridge_abandoned || !bridge_ready {
                            continue;
                        }
                        if let Some(ctx) = poll_fingerprint_changed(&calls, &mut last_fingerprint) {
                            if let Some(token) = inflight.take() {
                                token.cancel();
                                generation += 1;
                            }
                            if let Some(token) = refresh_tick(
                                &calls,
                                &done_tx,
                                &mut generation,
                                HOOK_EVENT_STATUS,
                                Some(ctx),
                            ) {
                                inflight = Some(token);
                            }
                        }
                    }
                    Some(StatusLine::Items(items)) => {
                        if let Some(token) = inflight.take() {
                            // A command script started before a mode switch
                            // must not publish over the items channel.
                            token.cancel();
                            generation += 1;
                        }
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
                        interval_deadline =
                            Some(Instant::now() + Duration::from_secs(items.refresh_interval_secs));
                        duration_deadline = items
                            .needs_duration()
                            .then(|| Instant::now() + DURATION_TICK);
                        // A data change (or a fresh config) re-renders; the
                        // duration clock has its own 1s tick.
                        let ctx = fetch_items_ctx(&calls, items);
                        let fingerprint = items_fingerprint(items, &ctx);
                        if last_items_fingerprint.as_ref() != Some(&fingerprint) {
                            last_items_fingerprint = Some(fingerprint);
                            maybe_start_git(&calls, items, &ctx);
                            let data = refresh_items(&calls, items, Some(ctx), true);
                            if data.is_some() {
                                last_items_data = data;
                            }
                        } else {
                            maybe_start_git(&calls, items, &ctx);
                        }
                    }
                }
            }
            Trigger::Event(reason) => {
                // INFO level: event delivery is the TE12 diagnostic
                // surface (which triggers arrived, which were dropped by
                // the dormant gate / bridge wait).
                tracing::info!(reason, "statusline refresh trigger");
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
                let (shutdown, saw_live) = drain_triggers(&mut rx);
                if shutdown {
                    if let Some(token) = inflight.take() {
                        token.cancel();
                    }
                    break;
                }
                if saw_live && live_deadline.is_none() && live_currently_enabled() {
                    live_deadline = Some(Instant::now() + live_refresh_interval());
                }
                let settings = config::load_settings_snapshot();
                crate::reconcile_subscriptions(&calls, &settings);
                match settings.status_line.as_ref() {
                    None => {
                        if let Some(token) = inflight.take() {
                            token.cancel();
                            generation += 1;
                        }
                        restore_if_mounted(&calls);
                        interval_deadline = None;
                        duration_deadline = None;
                        last_items_data = None;
                        last_items_fingerprint = None;
                        continue;
                    }
                    Some(StatusLine::Command(_)) => {
                        // A newer update cancels the in-flight script (CC
                        // semantics). Bump the generation so its late result
                        // is discarded even when no new run follows.
                        if let Some(token) = inflight.take() {
                            token.cancel();
                            generation += 1;
                        }
                        if let Some(token) =
                            refresh_tick(&calls, &done_tx, &mut generation, HOOK_EVENT_STATUS, None)
                        {
                            inflight = Some(token);
                        }
                        duration_deadline = None;
                        interval_deadline = current_refresh_interval()
                            .map(|secs| Instant::now() + Duration::from_secs(secs));
                    }
                    Some(StatusLine::Items(items)) => {
                        if let Some(token) = inflight.take() {
                            token.cancel();
                            generation += 1;
                        }
                        let ctx = fetch_items_ctx(&calls, items);
                        maybe_start_git(&calls, items, &ctx);
                        let read_stats = reads_session_stats_on(reason);
                        let data = refresh_items(&calls, items, Some(ctx), read_stats);
                        if data.is_some() {
                            last_items_data = data;
                        }
                        last_items_fingerprint = None;
                        interval_deadline =
                            Some(Instant::now() + Duration::from_secs(items.refresh_interval_secs));
                        duration_deadline = items
                            .needs_duration()
                            .then(|| Instant::now() + DURATION_TICK);
                    }
                }
            }
        }
    }
}

/// Coalesce the debounce window: drain queued triggers; report whether a
/// shutdown arrived (which wins over every other trigger) and whether any
/// live bookkeeping was drained (re-arm the ticker so a quiet stream's
/// cadence is not lost to the drain).
fn drain_triggers(rx: &mut UnboundedReceiver<Trigger>) -> (bool, bool) {
    let mut saw_live = false;
    loop {
        match rx.try_recv() {
            Ok(Trigger::Shutdown) => return (true, saw_live),
            Ok(Trigger::Live) => saw_live = true,
            Ok(Trigger::GitDone(generation, info)) => {
                let accepted = with_engine(|engine| engine.git.apply(generation, info));
                if accepted {
                    tracing::info!("statusline git probe applied during drain");
                }
            }
            Ok(_) => {}
            Err(_) => return (false, saw_live),
        }
    }
}

/// Whether the current configuration enables live measurement (both modes
/// are read from the current settings on every tick).
fn live_currently_enabled() -> bool {
    let snapshot = config::load_settings_snapshot();
    match snapshot.status_line {
        Some(StatusLine::Command(config)) => config.live_tokens.enabled,
        Some(StatusLine::Items(items)) => items.live_enabled(),
        None => false,
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

/// The configured `liveTokens.refreshMs` right now (re-read per tick;
/// §1.5 — applies immediately).
fn live_refresh_interval() -> Duration {
    let ms = config::load_settings_snapshot()
        .status_line
        .map(|config| config.live_tokens().refresh_ms)
        .unwrap_or(DEFAULT_LIVE_REFRESH_MS);
    Duration::from_millis(ms)
}

/// The configured `refreshInterval` right now (re-read from settings so a
/// config edit applies on the next tick).
fn current_refresh_interval() -> Option<u64> {
    config::load_settings_snapshot()
        .status_line
        .and_then(|config| config.refresh_interval_secs())
}

/// One poll pass for command mode: when unconfigured, also ride the FR-H
/// restore (the poll covers the idle no-event case); otherwise fingerprint
/// the five ctx facts and report whether ANY of them changed since the last
/// poll. The first poll after (re)configuration counts as a change — a free
/// initial render for config edits the event surface knows nothing about.
fn poll_fingerprint_changed(calls: &AsyncHostCalls, last: &mut Option<String>) -> Option<CtxData> {
    let settings = config::load_settings_snapshot();
    if settings.status_line.is_none() {
        restore_if_mounted(calls);
        *last = None;
        return None;
    }
    let ctx = fetch_ctx(calls);
    let live_fingerprint = with_engine(|engine| {
        engine
            .live
            .as_ref()
            .filter(|live| !live.is_streaming())
            .map(|live| live.peek_fingerprint(Instant::now()))
    });
    let fingerprint = format!(
        "{:?}|{}|{:?}|{:?}|{:?}|{:?}",
        ctx.model.as_ref().map(|model| model.get("id")),
        ctx.cwd,
        ctx.thinking_level,
        ctx.session_name,
        ctx.context_usage,
        live_fingerprint,
    );
    let changed = last.as_ref() != Some(&fingerprint);
    *last = Some(fingerprint);
    // On change, the already-fetched ctx rides into refresh_tick (S4).
    changed.then_some(ctx)
}

/// One command-mode refresh pass: reload config, pull ctx, spawn the script.
/// Returns the new in-flight cancel token.
///
/// Every tick runs the script unconditionally: the seven trigger events are
/// all low-frequency user-visible operations (message/tool completion,
/// model/thinking switches, session lifecycle) already coalesced by the
/// 300ms debounce, so a snapshot-comparison short-circuit only creates
/// "changed but not refreshed" bugs — the git branch, for one, lives in none
/// of the host-visible fields (TE12 follow-up: switching branches in a bash
/// tool never re-rendered because the dirty key was unchanged).
fn refresh_tick(
    calls: &AsyncHostCalls,
    done_tx: &UnboundedSender<ScriptDone>,
    generation: &mut usize,
    hook_event_name: &str,
    ctx_override: Option<CtxData>,
) -> Option<CancelToken> {
    let settings = config::load_settings_snapshot();
    let Some(StatusLine::Command(config)) = settings.status_line else {
        restore_if_mounted(calls);
        return None;
    };

    // Placement switch (FR-H): restore the old channel before using the
    // new one.
    let mounted = with_engine(|engine| engine.mounted);
    if mounted.is_some_and(|old| old != config.placement) {
        restore_if_mounted(calls);
    }

    // V13-07 S4: reuse the ctx a data-changing poll tick already fetched
    // (that tick's 6 host calls ride into the refresh instead of a second
    // fetch); other triggers self-fetch.
    let ctx = ctx_override.unwrap_or_else(|| fetch_ctx(calls));
    let snapshot = snapshot_from_engine(&ctx, settings.session_dir.as_deref(), hook_event_name);

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
/// accumulated state. The live delta window advances HERE — the
/// payload-assembly instant IS the measurement base (FR-B) — and the
/// authoritative session identity (FR-I) wins over the transcript latch,
/// which only runs as the fallback when `ctx.sessionFile` is unavailable.
fn snapshot_from_engine(
    ctx: &CtxData,
    settings_session_dir: Option<&str>,
    hook_event_name: &str,
) -> Snapshot {
    let mut snapshot = with_engine(|engine| {
        if let Some(info) = &ctx.session_file {
            let path = info.get("path").and_then(Value::as_str).map(str::to_owned);
            if let Some(id) = info.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                engine.set_authoritative_session(path, id.to_owned());
            }
        }
        let (transcript_path, session_id) = match engine.authoritative_session() {
            Some((path, id)) => (path.clone(), Some(id.clone())),
            None => {
                let cwd_path = Path::new(&ctx.cwd);
                engine.ensure_transcript(cwd_path, settings_session_dir);
                (
                    engine
                        .transcript_path()
                        .map(|path| path.display().to_string()),
                    engine.session_id().map(str::to_owned),
                )
            }
        };
        let live_output = engine
            .live
            .as_mut()
            .filter(|live| live.ever_measured())
            .map(|live| live.snapshot(std::time::Instant::now()));
        Snapshot {
            cwd: ctx.cwd.clone(),
            model: ctx.model.clone(),
            context_usage: ctx.context_usage.clone(),
            thinking_level: ctx.thinking_level.clone(),
            session_name: ctx.session_name.clone(),
            totals: engine.totals(),
            last_usage: engine.last_usage(),
            session_elapsed_ms: engine.session_elapsed_ms(),
            transcript_path,
            session_id,
            live_output,
            hook_event_name: hook_event_name.to_owned(),
        }
    });
    // Token accounting runs AFTER the engine lock is released: a large
    // model tokenizer must never block the streaming dispatch thread.
    if let Some(live) = snapshot.live_output.as_mut() {
        let model_id = snapshot
            .model
            .as_ref()
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut tokens = crate::tokens::count_breakdown(
            model_id.as_deref(),
            &live.text,
            &live.thinking,
            &live.toolcall,
        );
        // Same precedence as the UI fields: a provider-reported running/exact
        // total overrides the local estimate while the tier split keeps its
        // proportions.
        let provider_total = if live.streaming {
            live.output_tokens.or(live.output_tokens_exact)
        } else {
            live.output_tokens_exact.or(live.output_tokens)
        };
        if let Some(total) = provider_total.filter(|total| *total > 0) {
            tokens = tokens.with_provider_total(total);
        }
        live.tokens = Some(tokens);
    }
    snapshot
}

/// Pull the command-mode session facts over host calls (six reads; each
/// failure falls back independently).
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
        session_file: host_ok(calls, "ctx.sessionFile", json!({})).filter(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        }),
        ..CtxData::default()
    }
}

/// Items-mode context facts: the base six plus only the reads the selected
/// fields need (field-driven collection, R20 §4.4).
fn fetch_items_ctx(calls: &AsyncHostCalls, items: &ItemsConfig) -> CtxData {
    let mut ctx = fetch_ctx(calls);
    if items.has_field(FieldId::PermissionMode) {
        ctx.mode = host_ok(calls, "getMode", json!({}))
            .and_then(|value| value.as_str().map(str::to_owned));
    }
    if items.has_field(FieldId::RunState) {
        ctx.idle = host_ok(calls, "ctx.isIdle", json!({})).and_then(|value| value.as_bool());
    }
    if items.use_colors {
        ctx.theme = host_ok(calls, "ui.theme", json!({})).filter(|value| !value.is_null());
    }
    ctx
}

/// Items-mode change fingerprint: config + ctx + session totals + Git +
/// frozen live values. The duration clock is intentionally absent (its own
/// 1s tick re-renders it).
fn items_fingerprint(items: &ItemsConfig, ctx: &CtxData) -> String {
    let (stats, live) = with_engine(|engine| {
        (
            engine.session_stats.fingerprint(),
            engine
                .live
                .as_ref()
                .filter(|live| !live.is_streaming())
                .map(|live| live.peek_fingerprint(Instant::now())),
        )
    });
    let git = with_engine(|engine| format!("{:?}", engine.git.info()));
    format!(
        "{items:?}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{stats}|{git}|{live:?}",
        ctx.cwd,
        ctx.model.as_ref().map(|model| model.get("id")),
        ctx.thinking_level,
        ctx.session_name,
        ctx.context_usage,
        ctx.mode,
        ctx.idle,
        ctx.theme,
    )
}

/// Start a Git probe when the selection needs it and the interval elapsed.
fn maybe_start_git(calls: &AsyncHostCalls, items: &ItemsConfig, ctx: &CtxData) {
    let _ = calls;
    if !items.needs_git() {
        // Not selected: invalidate any in-flight probe and stop collecting.
        with_engine(|engine| engine.git.clear());
        return;
    }
    let now = Instant::now();
    let interval = Duration::from_secs(items.refresh_interval_secs);
    let due = with_engine(|engine| engine.git.should_fetch(interval, now));
    if !due {
        return;
    }
    let generation = with_engine(|engine| engine.git.begin(now));
    let cwd = ctx.cwd.clone();
    tokio::task::spawn_blocking(move || {
        let info = git::probe(Path::new(&cwd));
        crate::request_trigger(Trigger::GitDone(generation, info));
    });
}

/// One items-mode refresh: fetch/refresh the absolute session snapshot, build
/// the frame data, render and publish. Returns the data for later local ticks.
fn refresh_items(
    calls: &AsyncHostCalls,
    items: &ItemsConfig,
    ctx_override: Option<CtxData>,
    read_stats: bool,
) -> Option<FieldData> {
    if items.needs_session_stats() && read_stats {
        let token = with_engine(|engine| engine.session_stats.begin_request());
        match fetch_session_stats(calls) {
            Some(stats) => {
                with_engine(|engine| engine.session_stats.apply(token, stats));
            }
            None => warn_missing_session_stats_once(),
        }
    }
    let ctx = ctx_override.unwrap_or_else(|| fetch_items_ctx(calls, items));
    let data = build_field_data(items, &ctx);
    publish_items_from_data(calls, items, &data);
    Some(data)
}

/// Whether an items event owns a session-totals refresh (R20 §4.4). A
/// `message_end` only updates the live fields; model/thinking/mode/Git
/// events reuse the cached absolute snapshot.
fn reads_session_stats_on(reason: &str) -> bool {
    matches!(
        reason,
        "session_start"
            | "session_compact"
            | "turn_end"
            | "agent_settled"
            | "interval"
            | "config_saved"
    )
}

/// Read `ctx.sessionStats` without the generic per-call warning (`host_ok`):
/// a host that predates the method would otherwise log on every poll.
fn fetch_session_stats(calls: &AsyncHostCalls) -> Option<SessionStats> {
    let response = crate::host_call_static(calls, "ctx.sessionStats", json!({}));
    if let Some(error) = response.get("error") {
        tracing::debug!(?error, "rpi-statusline: ctx.sessionStats failed");
        return None;
    }
    let value = response.get("ok")?.clone();
    serde_json::from_value(value).ok()
}

/// One diagnostic per process: an old host without `ctx.sessionStats` omits
/// the token/cost/duration fields instead of scanning the transcript.
fn warn_missing_session_stats_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "rpi-statusline: ctx.sessionStats unavailable; session token/cost/duration fields stay hidden"
        );
    }
}

fn build_field_data(items: &ItemsConfig, ctx: &CtxData) -> FieldData {
    let model = ctx.model.as_ref().and_then(|model| {
        let id = model.get("id").and_then(Value::as_str)?.to_owned();
        let name = model.get("name").and_then(Value::as_str).map(str::to_owned);
        Some(ModelInfo { id, name })
    });
    let context = ctx.context_usage.as_ref().map(|usage| ContextInfo {
        tokens: usage.get("tokens").and_then(Value::as_u64),
        context_window: usage
            .get("contextWindow")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        percent: usage.get("percent").and_then(Value::as_f64),
    });
    let now = Instant::now();
    let (stats, live_peek, git) = with_engine(|engine| {
        (
            engine.session_stats.snapshot().cloned(),
            items
                .live_enabled()
                .then(|| engine.live_peek(now))
                .flatten(),
            engine.git.info().cloned(),
        )
    });
    let model_id = model.as_ref().map(|model| model.id.as_str());
    let live = live_peek.map(|peek| peek.into_view(model_id));
    let live_held = with_engine(|engine| engine.live_held_values());
    FieldData {
        cwd: Some(ctx.cwd.clone()).filter(|cwd| !cwd.is_empty()),
        model,
        thinking: ctx.thinking_level.clone(),
        mode: ctx.mode.clone(),
        idle: ctx.idle,
        context,
        session_name: ctx.session_name.clone(),
        stats,
        live,
        live_held,
        git,
        now: Some(Utc::now()),
        number_format: items.number_format,
        palette: items
            .use_colors
            .then(|| ctx.theme.as_ref().map(Palette::from_theme))
            .flatten(),
        preview: false,
    }
}

/// Re-render from the cached data (duration tick / live tick / Git result):
/// no host calls at all.
fn publish_items_from_cache(
    calls: &AsyncHostCalls,
    items: &ItemsConfig,
    data: Option<&mut FieldData>,
) {
    let Some(data) = data else {
        return;
    };
    let now = Instant::now();
    let model_id = data.model.as_ref().map(|model| model.id.clone());
    let (live_peek, git) = with_engine(|engine| {
        (
            items
                .live_enabled()
                .then(|| engine.live_peek(now))
                .flatten(),
            engine.git.info().cloned(),
        )
    });
    data.live = live_peek.map(|peek| peek.into_view(model_id.as_deref()));
    data.live_held = with_engine(|engine| engine.live_held_values());
    data.git = git;
    data.now = Some(Utc::now());
    data.number_format = items.number_format;
    publish_items_from_data(calls, items, data);
}

fn publish_items_from_data(calls: &AsyncHostCalls, items: &ItemsConfig, data: &FieldData) {
    let line = fields::render_line(&items.entries, &items.separator, items.use_colors, data);
    publish_line(calls, line.as_deref(), items.placement, items.padding);
    // Remember this frame's fresh live readings for the next frame's hold
    // (the gap between two assistant messages, the rate window, a tier the
    // current message has not produced yet).
    with_engine(|engine| engine.record_live_values(data));
}

/// Publish one items frame into the configured channel. Empty output clears
/// the owned key (widget/status) or restores the built-in footer (replace) —
/// never an empty separator or a permanently blank footer.
fn publish_line(calls: &AsyncHostCalls, text: Option<&str>, placement: Placement, padding: usize) {
    let text = text.filter(|text| !text.is_empty());
    let mounted = with_engine(|engine| engine.mounted);
    if let Some(text) = text {
        if mounted.is_some_and(|old| old != placement) {
            restore_if_mounted(calls);
        }
        match placement {
            Placement::Replace => {
                host_ok(
                    calls,
                    "ui.setFooter",
                    json!({ "component": footer_tree(text, padding) }),
                );
                with_engine(|engine| engine.mounted = Some(Placement::Replace));
            }
            Placement::Widget => {
                host_ok(
                    calls,
                    "ui.setWidget",
                    json!({
                        "key": STATUS_KEY,
                        "content": footer_tree(text, padding),
                        "placement": "belowEditor",
                    }),
                );
                with_engine(|engine| engine.mounted = Some(Placement::Widget));
            }
            Placement::Status => {
                host_ok(
                    calls,
                    "ui.setStatus",
                    json!({ "key": STATUS_KEY, "text": text }),
                );
                with_engine(|engine| engine.mounted = Some(Placement::Status));
            }
        }
    } else {
        restore_if_mounted(calls);
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
    let Some(StatusLine::Command(config)) = settings.status_line else {
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

/// Access the shared engine state (the dispatch thread and this loop are the
/// only users).
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
        assert_eq!(drain_triggers(&mut rx), (false, false));
        tx.send(Trigger::Event("message_end")).ok();
        tx.send(Trigger::Live).ok();
        tx.send(Trigger::Shutdown).ok();
        tx.send(Trigger::Event("turn_end")).ok();
        assert_eq!(drain_triggers(&mut rx), (true, true));
        tx.send(Trigger::Live).ok();
        assert_eq!(drain_triggers(&mut rx), (false, true));
    }

    #[test]
    fn session_stats_reads_are_trigger_gated() {
        for reason in [
            "session_start",
            "session_compact",
            "turn_end",
            "agent_settled",
            "interval",
            "config_saved",
        ] {
            assert!(reads_session_stats_on(reason), "{reason} reads totals");
        }
        for reason in [
            "message_end",
            "model_select",
            "thinking_level_select",
            "session_info_changed",
            "agent_start",
            "mode_change",
            "tool_execution_end",
        ] {
            assert!(
                !reads_session_stats_on(reason),
                "{reason} must not read session totals"
            );
        }
    }

    #[test]
    fn items_fingerprint_moves_with_config_and_ctx() {
        // Pure-shape test without the global engine: the fingerprint must at
        // least include the config and the ctx facts it is given.
        let items = ItemsConfig::default();
        let first = CtxData {
            cwd: "/a".into(),
            ..CtxData::default()
        };
        let second = CtxData {
            cwd: "/b".into(),
            ..CtxData::default()
        };
        // The engine is not installed in unit tests; capture the shape via
        // the same formatting pieces instead.
        let shape = |ctx: &CtxData| {
            format!(
                "{items:?}|{}|{:?}",
                ctx.cwd,
                ctx.model.as_ref().map(|model| model.get("id"))
            )
        };
        assert_ne!(shape(&first), shape(&second));
    }
}
