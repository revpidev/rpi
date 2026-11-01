//! Footer status line + refresh worker (TE44 FR-D/FR-E; TE53 FR-G).
//!
//! Script execution is slow (up to the framework timeout), so dispatches
//! never fetch inline: they enqueue a [`Job`] on a per-session worker thread
//! and return immediately (statusline precedent). The worker coalesces
//! bursts, enforces the `usage.refreshMs` throttle for `message_end`
//! triggers, keeps the last same-identity success on a transient failure,
//! and writes `ui.setStatus("rpi-usage", ...)`.
//!
//! TE53 rules:
//! - footer text comes from typed `windows[]`/`ledgers[]`/`balance[]`
//!   ([`crate::format::footer_text`]); a legacy third-party envelope keeps
//!   its `displayText` verbatim;
//! - the automatic path only fetches for the current registered provider,
//!   with `enabled`/`footer`/UI on and the provider not `ExplicitOnly`;
//! - offline and `ExplicitOnly` (xAI) paths read `ctx.usage.cached` only —
//!   zero scripts, zero network;
//! - a typed future `resetAt` schedules a local redraw every
//!   `min(60s, nextReset)` from the cached envelope (no request);
//!   `usage.resetCountdown=false` disables the tick and renders a static
//!   label;
//! - a command publishes the *same* fetch result ([`FooterAction`]) instead
//!   of triggering a trailing refresh.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rpi_ext_host::native::PluginCookie;
use rpi_ext_host::usage::{UsageEnvelope, UsageFetchResult};

use crate::{HostCall, NativeHostCall, config, format, host, providers};

/// `ui.setStatus` key (single line; other plugins' keys stay untouched).
pub const STATUS_KEY: &str = "rpi-usage";

/// Local countdown redraw cadence (TE53 §5.7).
pub const COUNTDOWN_TICK_MS: i64 = 60_000;

/// One unit of work for the refresh worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Refresh the footer; `throttled` applies the `usage.refreshMs` window.
    Refresh {
        /// Respect the `usage.refreshMs` window.
        throttled: bool,
    },
    /// Handle one `/usage` invocation (reports through
    /// `ui.emitStructured`).
    Command {
        /// Raw argument text after the command name.
        args: String,
    },
    /// `session_shutdown`: stop the worker.
    Shutdown,
}

/// Footer action carried by a command outcome (TE53 §7.2).
#[derive(Debug, Clone)]
pub enum FooterAction {
    /// Leave the footer alone (help/list/cached/illegal/local token report).
    None,
    /// Remove the status entry (disabled or a failure without stale data).
    Clear,
    /// Publish the same result the command used (no second request).
    Publish(Box<UsageFetchResult>),
}

/// The footer's last published state (one per worker/session).
#[derive(Debug, Default)]
pub struct FooterState {
    /// Provider key of the last published line.
    pub provider: Option<String>,
    /// Last published display text.
    pub text: Option<String>,
    /// Typed envelope behind the last published line (countdown source).
    pub envelope: Option<UsageEnvelope>,
    /// Instant of the last refresh attempt (success or failure).
    pub last_fetch: Option<Instant>,
    /// Epoch milliseconds of the last publish/tick render.
    pub last_render_ms: i64,
}

/// Result of one refresh/render (assertions and diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// A line is published (possibly unchanged).
    Updated {
        /// Provider key.
        provider: String,
        /// Published display text.
        text: String,
    },
    /// No usable data: the entry was removed.
    Cleared,
    /// Nothing to do (throttle, no UI, no typed countdown, unchanged state).
    Skipped,
}

/// One live worker per cookie. The generation makes a stale worker's exit
/// harmless: it removes the map entry only when the generation still
/// matches its own (a restarted session's worker must not be unregistered
/// by the previous session's shutdown — v0.1.6 review P1-6).
#[derive(Clone)]
struct WorkerEntry {
    generation: u64,
    sender: Sender<Job>,
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Start (or restart) the per-session worker; returns its job sender.
pub fn start_worker(cookie: PluginCookie, host: NativeHostCall) -> Sender<Job> {
    let (tx, rx) = std::sync::mpsc::channel();
    let key = cookie as usize;
    let generation = next_generation();
    let previous = workers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key,
            WorkerEntry {
                generation,
                sender: tx.clone(),
            },
        );
    if let Some(previous) = previous {
        let _ = previous.sender.send(Job::Shutdown);
    }
    let spawned = std::thread::Builder::new()
        .name("rpi-usage-refresh".to_owned())
        .spawn(move || worker_loop(key, generation, host, rx));
    if let Err(error) = spawned {
        tracing::warn!(%error, "rpi-usage: refresh worker could not start");
    }
    tx
}

/// The worker sender for a cookie (dispatch routing seam).
pub fn worker_for(cookie: PluginCookie) -> Option<Sender<Job>> {
    workers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(cookie as usize))
        .map(|entry| entry.sender.clone())
}

fn workers() -> &'static Mutex<HashMap<usize, WorkerEntry>> {
    static WORKERS: OnceLock<Mutex<HashMap<usize, WorkerEntry>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remove the cookie's worker entry only when it still belongs to
/// `generation` (see [`start_worker`]).
fn stop_worker_key(key: usize, generation: u64) {
    let mut workers = workers().lock().unwrap_or_else(|error| error.into_inner());
    if workers
        .get(&key)
        .is_some_and(|entry| entry.generation == generation)
    {
        workers.remove(&key);
    }
}

/// The worker loop: coalesce pending jobs, then run the last command or one
/// merged refresh. Between jobs it wakes for the local countdown tick.
fn worker_loop(
    key: usize,
    generation: u64,
    host: NativeHostCall,
    rx: std::sync::mpsc::Receiver<Job>,
) {
    let mut state = FooterState::default();
    loop {
        let now_ms = now_epoch_ms();
        let config = config::load();
        let wait = next_tick_ms(&state, &config, now_ms);
        let first = match wait {
            Some(deadline) => {
                let wait = Duration::from_millis((deadline - now_ms).max(1) as u64);
                match rx.recv_timeout(wait) {
                    Ok(job) => Some(job),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        tick(&host, &config, &mut state, now_epoch_ms());
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(job) => Some(job),
                Err(_) => break,
            },
        };
        let Some(first) = first else {
            break;
        };
        let mut command: Option<String> = None;
        let mut refresh_job: Option<bool> = None;
        let mut shutdown = false;
        let mut consume = |job: Job| match job {
            Job::Refresh { throttled } => match refresh_job {
                None => refresh_job = Some(throttled),
                Some(t) => refresh_job = Some(t && throttled),
            },
            Job::Command { args } => command = Some(args),
            Job::Shutdown => shutdown = true,
        };
        consume(first);
        while let Ok(job) = rx.try_recv() {
            consume(job);
        }
        if shutdown {
            break;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let now = Instant::now();
            let config = config::load();
            let now_ms = now_epoch_ms();
            if let Some(args) = command {
                if let Some(outcome) = crate::command::handle_now(&host, &config, &args, now_ms) {
                    if let Err(error) = host::emit_output(&host, &outcome.output) {
                        tracing::debug!(%error, "rpi-usage: ui.emitStructured rejected");
                    }
                    apply(&host, &config, &mut state, outcome.footer, now_ms);
                }
            } else if let Some(throttled) = refresh_job {
                refresh(&host, &config, &mut state, false, throttled, now, now_ms);
            }
        }));
        if let Err(_panic) = result {
            tracing::warn!("rpi-usage: refresh worker recovered from a panic");
        }
    }
    stop_worker_key(key, generation);
}

/// Current wall-clock epoch milliseconds (the typed reset base).
///
/// `RPI_USAGE_NOW_MS` is the internal test clock shared with the provider
/// scripts (TE50, unset in production); it keeps countdown goldens
/// deterministic without touching the host clock.
pub fn now_epoch_ms() -> i64 {
    if let Ok(raw) = std::env::var("RPI_USAGE_NOW_MS")
        && let Ok(now) = raw.parse::<i64>()
    {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// The refresh decision + pipeline (no threads; testable).
///
/// `force` bypasses the framework cache and the `refreshMs` throttle. The
/// automatic path passes `force=false`; commands fetch their own result and
/// use [`apply`] instead, so no second request is ever issued.
pub fn refresh(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    force: bool,
    throttled: bool,
    now: Instant,
    now_ms: i64,
) -> RefreshOutcome {
    if !config.enabled || !config.footer {
        return clear_status(host, state);
    }
    if !host::has_ui(host) {
        // Dormant (no UI / subagent child): never touch the host surface.
        return RefreshOutcome::Skipped;
    }
    if !force
        && throttled
        && let Some(last) = state.last_fetch
        && now.duration_since(last) < Duration::from_millis(config.refresh_ms)
    {
        return RefreshOutcome::Skipped;
    }
    let model = host::current_model(host);
    let model_provider = model
        .as_ref()
        .and_then(host::model_provider)
        .unwrap_or_default();
    let known = host::usage_list_providers(host);
    let Some(provider) = providers::current_provider(model_provider, &known) else {
        return clear_status(host, state);
    };
    state.last_fetch = Some(now);
    let result = if host::offline() || providers::is_explicit_only(&provider) {
        host::usage_cached(host, &provider)
    } else {
        host::usage_fetch_detailed(host, &provider, force)
    };
    match result.envelope {
        Some(envelope) => publish(host, config, state, &provider, &envelope, now_ms),
        // A failure with no same-identity cache (including an identity change
        // or a cache miss) clears the old account's line instead of keeping
        // it.
        None => clear_status(host, state),
    }
}

/// Apply a command outcome's footer action to the same result the command
/// used.
pub fn apply(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    action: FooterAction,
    now_ms: i64,
) -> RefreshOutcome {
    match action {
        FooterAction::None => RefreshOutcome::Skipped,
        FooterAction::Clear => clear_status(host, state),
        FooterAction::Publish(result) => {
            // The footer switch and UI availability still gate the status
            // surface; command output itself is not gated on hasUI.
            if !config.enabled || !config.footer || !host::has_ui(host) {
                return clear_status(host, state);
            }
            match result.envelope {
                Some(envelope) => {
                    let provider = envelope.provider.clone();
                    publish(host, config, state, &provider, &envelope, now_ms)
                }
                None => clear_status(host, state),
            }
        }
    }
}

/// Local countdown redraw from the cached typed envelope — never a script or
/// a network request.
pub fn tick(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    now_ms: i64,
) -> RefreshOutcome {
    if !config.enabled || !config.footer || !config.reset_countdown || !host::has_ui(host) {
        return RefreshOutcome::Skipped;
    }
    let (Some(provider), Some(envelope)) = (state.provider.clone(), state.envelope.clone()) else {
        return RefreshOutcome::Skipped;
    };
    if format::next_reset_ms(&envelope, now_ms).is_none() {
        return RefreshOutcome::Skipped;
    }
    publish(host, config, state, &provider, &envelope, now_ms)
}

/// The next local tick deadline, if a typed future reset is present and the
/// countdown is enabled: `min(now + 60s, earliest reset)`.
pub fn next_tick_ms(state: &FooterState, config: &config::UsageConfig, now_ms: i64) -> Option<i64> {
    if !config.enabled || !config.footer || !config.reset_countdown {
        return None;
    }
    let envelope = state.envelope.as_ref()?;
    let reset = format::next_reset_ms(envelope, now_ms)?;
    Some((now_ms + COUNTDOWN_TICK_MS).min(reset))
}

fn publish(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    provider: &str,
    envelope: &UsageEnvelope,
    now_ms: i64,
) -> RefreshOutcome {
    let Some(text) = format::footer_text(envelope, config, now_ms) else {
        return clear_status(host, state);
    };
    // Skip the host write when the line is already current (the countdown
    // tick may land inside the same display minute).
    if state.provider.as_deref() != Some(provider) || state.text.as_deref() != Some(text.as_str()) {
        host::set_status(host, STATUS_KEY, Some(&text));
    }
    state.provider = Some(provider.to_owned());
    state.text = Some(text.clone());
    state.envelope = Some(envelope.clone());
    state.last_render_ms = now_ms;
    RefreshOutcome::Updated {
        provider: provider.to_owned(),
        text,
    }
}

fn clear_status(host: &dyn HostCall, state: &mut FooterState) -> RefreshOutcome {
    let had_line = state.text.is_some() || state.provider.is_some();
    if had_line {
        host::set_status(host, STATUS_KEY, None);
        state.provider = None;
        state.text = None;
        state.envelope = None;
        return RefreshOutcome::Cleared;
    }
    RefreshOutcome::Skipped
}

/// Test seam: register a raw sender (no thread) under a cookie.
#[cfg(test)]
pub fn install_worker_for_test(cookie: usize, sender: Sender<Job>) {
    workers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            cookie,
            WorkerEntry {
                generation: next_generation(),
                sender,
            },
        );
}

/// Test seam: clear every worker entry.
#[cfg(test)]
pub fn clear_workers_for_test() {
    workers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_host::UsageFakeHost;
    use serde_json::{Value, json};

    fn config() -> config::UsageConfig {
        config::UsageConfig::default()
    }

    const NOW_MS: i64 = 1_800_000_000_000;
    const RESET_48M: &str = "2027-01-15T08:48:00Z";

    fn typed_envelope(provider: &str) -> Value {
        json!({
            "schemaVersion": 1,
            "provider": provider,
            "displayText": "legacy",
            "windows": [{
                "id": "5h",
                "label": "5h",
                "status": "ok",
                "usedPercent": 39.0,
                "remainingPercent": 61.0,
                "resetAt": RESET_48M
            }]
        })
    }

    fn host_with(provider: &str, envelope: Value) -> UsageFakeHost {
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "m", "provider": provider}));
        let usage = providers::alias_for(provider)
            .unwrap_or(provider)
            .to_owned();
        host.set_providers(vec![usage.clone()]);
        host.set_envelope(&usage, envelope);
        host
    }

    #[test]
    fn refresh_publishes_typed_text_and_keeps_stale_on_transient_failure() {
        let host = host_with("zai", typed_envelope("glm-coding-plan"));
        let mut state = FooterState::default();
        let now = Instant::now();
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, now, NOW_MS),
            RefreshOutcome::Updated {
                provider: "glm-coding-plan".to_owned(),
                text: "5h 39% (~48m)".to_owned(),
            }
        );
        assert_eq!(
            host.statuses(),
            vec![("rpi-usage".to_owned(), Some("5h 39% (~48m)".to_owned()))]
        );
        // Transient failure with same-identity stale data keeps the line.
        host.set_failure(
            "glm-coding-plan",
            json!({
                "envelope": typed_envelope("glm-coding-plan"),
                "source": "cache",
                "stale": true,
                "error": {"kind": "transport", "action": "Check the network and provider status, then retry."}
            }),
        );
        assert_eq!(
            refresh(&host, &config(), &mut state, true, false, now, NOW_MS),
            RefreshOutcome::Updated {
                provider: "glm-coding-plan".to_owned(),
                text: "5h 39% (~48m)".to_owned(),
            }
        );
        assert_eq!(host.statuses().len(), 1, "no flicker write");
        // A failure without a same-identity envelope clears (identity change).
        host.set_failure(
            "glm-coding-plan",
            json!({
                "stale": false,
                "error": {"kind": "auth-unavailable", "action": "Sign in to the provider again, then retry."}
            }),
        );
        assert_eq!(
            refresh(&host, &config(), &mut state, true, false, now, NOW_MS),
            RefreshOutcome::Cleared
        );
        assert_eq!(
            host.statuses().last(),
            Some(&("rpi-usage".to_owned(), None))
        );
    }

    #[test]
    fn throttle_disabled_no_ui_and_provider_switch_paths() {
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        let now = Instant::now();
        refresh(&host, &config(), &mut state, false, false, now, NOW_MS);
        assert_eq!(
            refresh(
                &host,
                &config(),
                &mut state,
                false,
                true,
                now + Duration::from_millis(10),
                NOW_MS
            ),
            RefreshOutcome::Skipped
        );
        assert_eq!(host.fetch_count("deepseek"), 1, "throttle skips the fetch");
        refresh(
            &host,
            &config(),
            &mut state,
            true,
            true,
            now + Duration::from_millis(20),
            NOW_MS,
        );
        assert_eq!(host.fetch_count("deepseek"), 2, "force bypasses throttle");
        let disabled = config::UsageConfig {
            enabled: false,
            ..config()
        };
        assert_eq!(
            refresh(
                &host,
                &disabled,
                &mut state,
                false,
                false,
                Instant::now(),
                NOW_MS
            ),
            RefreshOutcome::Cleared
        );
        let no_ui = host_with("deepseek", typed_envelope("deepseek"));
        no_ui.set_has_ui(false);
        let mut state = FooterState::default();
        assert_eq!(
            refresh(
                &no_ui,
                &config(),
                &mut state,
                false,
                false,
                Instant::now(),
                NOW_MS
            ),
            RefreshOutcome::Skipped
        );
        assert!(!no_ui.called("ctx.usage.fetchDetailed"));
        // Model switch to an unregistered provider clears the old line.
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        refresh(
            &host,
            &config(),
            &mut state,
            false,
            false,
            Instant::now(),
            NOW_MS,
        );
        host.set_model(json!({"id": "m", "provider": "dgx-spark"}));
        assert_eq!(
            refresh(
                &host,
                &config(),
                &mut state,
                false,
                false,
                Instant::now(),
                NOW_MS
            ),
            RefreshOutcome::Cleared
        );
    }

    #[test]
    fn xai_and_offline_refresh_use_cached_only() {
        let host = host_with("xai", typed_envelope("xai"));
        let mut state = FooterState::default();
        assert_eq!(
            refresh(
                &host,
                &config(),
                &mut state,
                false,
                false,
                Instant::now(),
                NOW_MS
            ),
            RefreshOutcome::Updated {
                provider: "xai".to_owned(),
                text: "5h 39% (~48m)".to_owned(),
            }
        );
        assert!(host.called("ctx.usage.cached"));
        assert!(
            !host.called("ctx.usage.fetchDetailed"),
            "xAI auto refresh never runs a script"
        );
        assert_eq!(host.fetch_calls(), Vec::<(String, bool)>::new());
        let cached_before = host.cached_calls();
        assert_eq!(cached_before.len(), 1);
        // Explicit command result publishes into the footer (no refresh).
        let outcome = crate::command::handle_now(&host, &config(), "xai", NOW_MS).expect("channel");
        assert_eq!(
            host.fetch_count("xai"),
            1,
            "the explicit command fetches once"
        );
        apply(&host, &config(), &mut state, outcome.footer, NOW_MS);
        assert_eq!(
            host.statuses().last(),
            Some(&("rpi-usage".to_owned(), Some("5h 39% (~48m)".to_owned())))
        );
        // A following auto refresh re-reads cache only, never the script.
        refresh(
            &host,
            &config(),
            &mut state,
            false,
            false,
            Instant::now(),
            NOW_MS,
        );
        assert_eq!(host.fetch_count("xai"), 1);
        assert_eq!(host.cached_calls().len(), cached_before.len() + 1);

        // Offline: every provider reads cached only.
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var("RPI_OFFLINE").ok();
        rpi_test_env::set_var("RPI_OFFLINE", "1");
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        refresh(
            &host,
            &config(),
            &mut state,
            false,
            false,
            Instant::now(),
            NOW_MS,
        );
        assert!(host.called("ctx.usage.cached"));
        assert!(!host.called("ctx.usage.fetchDetailed"));
        match previous {
            Some(value) => rpi_test_env::set_var("RPI_OFFLINE", value),
            None => rpi_test_env::remove_var("RPI_OFFLINE"),
        }
    }

    #[test]
    fn command_footer_action_publishes_or_clears_without_a_second_request() {
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        let outcome = crate::command::handle_now(&host, &config(), "", NOW_MS).expect("channel");
        assert_eq!(host.fetch_count("deepseek"), 1);
        apply(&host, &config(), &mut state, outcome.footer, NOW_MS);
        assert_eq!(host.fetch_count("deepseek"), 1, "no trailing refresh");
        // Help leaves the footer alone.
        let outcome =
            crate::command::handle_now(&host, &config(), "help", NOW_MS).expect("channel");
        assert_eq!(
            apply(&host, &config(), &mut state, outcome.footer, NOW_MS),
            RefreshOutcome::Skipped
        );
        // A failure without stale clears.
        host.set_failure(
            "deepseek",
            json!({
                "stale": false,
                "error": {"kind": "transport", "action": "Check the network and provider status, then retry."}
            }),
        );
        let outcome = crate::command::handle_now(&host, &config(), "", NOW_MS).expect("channel");
        assert_eq!(
            apply(&host, &config(), &mut state, outcome.footer, NOW_MS),
            RefreshOutcome::Cleared
        );
    }

    #[test]
    fn command_publish_respects_the_footer_switch_and_ui_availability() {
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        let outcome = crate::command::handle_now(&host, &config(), "", NOW_MS).expect("channel");
        let disabled_footer = config::UsageConfig {
            footer: false,
            ..config()
        };
        assert_eq!(
            apply(
                &host,
                &disabled_footer,
                &mut state,
                outcome.footer.clone(),
                NOW_MS
            ),
            RefreshOutcome::Skipped
        );
        assert!(host.statuses().is_empty(), "footer=false publishes nothing");

        // A previously published line is cleared when the switch turns off.
        apply(&host, &config(), &mut state, outcome.footer, NOW_MS);
        assert_eq!(host.statuses().len(), 1);
        let outcome = crate::command::handle_now(&host, &config(), "", NOW_MS).expect("channel");
        assert_eq!(
            apply(&host, &disabled_footer, &mut state, outcome.footer, NOW_MS),
            RefreshOutcome::Cleared
        );
        assert_eq!(
            host.statuses().last(),
            Some(&("rpi-usage".to_owned(), None))
        );

        // A non-UI bridge (print/json) never publishes a status row.
        let host = host_with("deepseek", typed_envelope("deepseek"));
        host.set_has_ui(false);
        let mut state = FooterState::default();
        let outcome = crate::command::handle_now(&host, &config(), "", NOW_MS).expect("channel");
        assert_eq!(
            apply(&host, &config(), &mut state, outcome.footer, NOW_MS),
            RefreshOutcome::Skipped
        );
        assert!(host.statuses().is_empty());
    }

    #[test]
    fn countdown_tick_redraws_without_requests_and_stops_for_static_or_legacy() {
        let host = host_with("deepseek", typed_envelope("deepseek"));
        let mut state = FooterState::default();
        refresh(
            &host,
            &config(),
            &mut state,
            false,
            false,
            Instant::now(),
            NOW_MS,
        );
        assert_eq!(
            next_tick_ms(&state, &config(), NOW_MS),
            Some(NOW_MS + COUNTDOWN_TICK_MS)
        );
        let later = NOW_MS + COUNTDOWN_TICK_MS;
        assert_eq!(
            tick(&host, &config(), &mut state, later),
            RefreshOutcome::Updated {
                provider: "deepseek".to_owned(),
                text: "5h 39% (~47m)".to_owned(),
            }
        );
        assert_eq!(host.fetch_count("deepseek"), 1, "tick never fetches");
        assert_eq!(host.cached_calls().len(), 0, "tick never calls cached");
        // Static reset disables the tick.
        let static_config = config::UsageConfig {
            reset_countdown: false,
            ..config()
        };
        assert_eq!(next_tick_ms(&state, &static_config, later), None);
        assert_eq!(
            tick(&host, &static_config, &mut state, later),
            RefreshOutcome::Skipped
        );
        // Legacy displayText has no typed reset -> no tick.
        let legacy = json!({"schemaVersion": 1, "provider": "x", "displayText": "5h 39%(~48m)"});
        let mut legacy_state = FooterState {
            provider: Some("x".to_owned()),
            text: Some("5h 39%(~48m)".to_owned()),
            envelope: Some(serde_json::from_value(legacy).expect("envelope")),
            last_fetch: None,
            last_render_ms: NOW_MS,
        };
        assert_eq!(next_tick_ms(&legacy_state, &config(), NOW_MS), None);
        legacy_state.envelope = None;
        assert_eq!(next_tick_ms(&legacy_state, &config(), NOW_MS), None);
    }

    #[test]
    fn tick_deadline_uses_an_earlier_reset() {
        let mut state = FooterState {
            provider: Some("x".to_owned()),
            envelope: Some(
                serde_json::from_value(json!({
                    "schemaVersion": 1,
                    "provider": "x",
                    "displayText": "x",
                    "windows": [{
                        "id": "5h", "label": "5h", "status": "ok",
                        "usedPercent": 1.0, "remainingPercent": 99.0,
                        "resetAt": "2026-01-15T00:00:30Z"
                    }]
                }))
                .expect("envelope"),
            ),
            last_fetch: None,
            text: Some("x".to_owned()),
            last_render_ms: 0,
        };
        let now = format::reset_epoch_ms("2026-01-15T00:00:00Z").expect("epoch");
        assert_eq!(next_tick_ms(&state, &config(), now), Some(now + 30_000));
        state.envelope = None;
        assert_eq!(next_tick_ms(&state, &config(), now), None);
    }

    #[test]
    fn typed_footer_clears_when_every_window_is_unknown() {
        let host = host_with(
            "deepseek",
            json!({
                "schemaVersion": 1,
                "provider": "deepseek",
                "displayText": "legacy",
                "windows": [{"id": "x", "label": "x", "status": "unknown"}]
            }),
        );
        let mut state = FooterState::default();
        assert_eq!(
            refresh(
                &host,
                &config(),
                &mut state,
                false,
                false,
                Instant::now(),
                NOW_MS
            ),
            RefreshOutcome::Skipped
        );
        assert!(host.statuses().is_empty());
    }

    #[test]
    fn stale_worker_stop_does_not_remove_the_new_worker() {
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        clear_workers_for_test();
        let cookie = 7usize;
        let (old_tx, _old_rx) = std::sync::mpsc::channel();
        install_worker_for_test(cookie, old_tx);
        let old_generation = workers()
            .lock()
            .unwrap()
            .get(&cookie)
            .expect("old worker")
            .generation;
        let (new_tx, _new_rx) = std::sync::mpsc::channel();
        install_worker_for_test(cookie, new_tx);
        let new_generation = workers()
            .lock()
            .unwrap()
            .get(&cookie)
            .expect("new worker")
            .generation;
        assert_ne!(old_generation, new_generation);
        stop_worker_key(cookie, old_generation);
        assert!(
            worker_for(cookie as PluginCookie).is_some(),
            "the new session's worker must survive the stale shutdown"
        );
        stop_worker_key(cookie, new_generation);
        assert!(worker_for(cookie as PluginCookie).is_none());
        clear_workers_for_test();
    }
}
