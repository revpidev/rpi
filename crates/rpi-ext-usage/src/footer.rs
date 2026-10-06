//! Footer status line + refresh worker (TE44 FR-D/FR-E).
//!
//! Script execution is slow (up to the framework timeout), so dispatches
//! never fetch inline: they enqueue a [`Job`] on a per-session worker thread
//! and return immediately (statusline precedent). The worker coalesces
//! bursts, enforces the `usage.refreshMs` throttle for `message_end`
//! triggers, and keeps the last successful line when a refresh fails
//! (`ui.setStatus("rpi-usage", ...)`).
//!
//! Refresh rules (plugin 01 §5/§6):
//! - `session_start` / `model_select` / `/usage` refresh without the
//!   throttle; `message_end` is the throttled fallback cadence;
//! - `usage.enabled` / `usage.footer` off ⇒ the status entry is removed and
//!   no script runs;
//! - a session without a UI stays dormant (no host calls, no status);
//! - a provider without a matching script (or a failed fetch without a
//!   previous success) removes the entry instead of showing stale data.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rpi_ext_host::native::PluginCookie;

use crate::{HostCall, NativeHostCall, config, format, host, providers};

/// `ui.setStatus` key (single line; other plugins' keys stay untouched).
pub const STATUS_KEY: &str = "rpi-usage";

/// One unit of work for the refresh worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Refresh the footer; `throttled` applies the `usage.refreshMs` window.
    Refresh {
        /// Respect the `usage.refreshMs` window.
        throttled: bool,
    },
    /// Handle one `/usage` invocation (reports through `ui.notify`).
    Command {
        /// Raw argument text after the command name.
        args: String,
    },
    /// `session_shutdown`: stop the worker.
    Shutdown,
}

/// The footer's last published state (one per worker/session).
#[derive(Debug, Default)]
pub struct FooterState {
    /// Provider key of the last successful line.
    pub provider: Option<String>,
    /// Last successful display text.
    pub text: Option<String>,
    /// Instant of the last fetch attempt (success or failure).
    pub last_fetch: Option<Instant>,
}

/// Result of one refresh (assertions and diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// A fresh line was published.
    Updated {
        /// Provider key.
        provider: String,
        /// Published display text.
        text: String,
    },
    /// The fetch failed; the previous line for the same provider stays.
    KeptLast {
        /// Provider key.
        provider: String,
    },
    /// No usable data (or the feature is off): the entry was removed.
    Cleared,
    /// Nothing to do (throttle, no UI, no status to clear).
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
///
/// Every `session_start` starts a fresh worker: the previous session's
/// `session_shutdown` stopped the old one, and without this the plugin ran
/// jobs inline (blocking dispatch, no throttle, no last-good retention) for
/// the rest of the process (v0.1.6 review P1-6). A previous worker is told
/// to stop, and it removes only its own map entry (generation check), so a
/// stale shutdown can never unregister the new session's worker.
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
/// merged refresh.
fn worker_loop(
    key: usize,
    generation: u64,
    host: NativeHostCall,
    rx: std::sync::mpsc::Receiver<Job>,
) {
    let mut state = FooterState::default();
    while let Ok(first) = rx.recv() {
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
            let config = config::load();
            let now = Instant::now();
            if let Some(args) = command {
                crate::command::handle_now(&host, &config, &mut state, &args, now);
            } else if let Some(throttled) = refresh_job {
                refresh(&host, &config, &mut state, false, throttled, now);
            }
        }));
        if let Err(_panic) = result {
            tracing::warn!("rpi-usage: refresh worker recovered from a panic");
        }
    }
    stop_worker_key(key, generation);
}

/// The refresh decision + pipeline (pure of threads; testable).
pub fn refresh(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    force: bool,
    throttled: bool,
    now: Instant,
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
    let provider = model
        .as_ref()
        .and_then(host::model_provider)
        .and_then(providers::alias_for);
    let Some(provider) = provider else {
        return clear_status(host, state);
    };
    if !host::usage_list_providers(host)
        .iter()
        .any(|known| known == provider)
    {
        return clear_status(host, state);
    }
    state.last_fetch = Some(now);
    match host::usage_fetch(host, provider, force) {
        Some(envelope) => match format::display_text(&envelope) {
            Some(text) => publish(host, state, provider, text),
            None => keep_or_clear(host, state, provider),
        },
        None => keep_or_clear(host, state, provider),
    }
}

fn publish(
    host: &dyn HostCall,
    state: &mut FooterState,
    provider: &str,
    text: &str,
) -> RefreshOutcome {
    // Skip the host write when the line is already current (message_end keeps
    // refreshing from the cache; an unchanged envelope must not churn
    // `ui.setStatus`).
    if state.provider.as_deref() != Some(provider) || state.text.as_deref() != Some(text) {
        host::set_status(host, STATUS_KEY, Some(text));
        state.provider = Some(provider.to_owned());
        state.text = Some(text.to_owned());
    }
    RefreshOutcome::Updated {
        provider: provider.to_owned(),
        text: text.to_owned(),
    }
}

fn keep_or_clear(host: &dyn HostCall, state: &mut FooterState, provider: &str) -> RefreshOutcome {
    if state.provider.as_deref() == Some(provider) && state.text.is_some() {
        return RefreshOutcome::KeptLast {
            provider: provider.to_owned(),
        };
    }
    clear_status(host, state)
}

fn clear_status(host: &dyn HostCall, state: &mut FooterState) -> RefreshOutcome {
    if state.text.is_some() || state.provider.is_some() {
        host::set_status(host, STATUS_KEY, None);
        state.provider = None;
        state.text = None;
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

    fn envelope(provider: &str, text: &str) -> Value {
        json!({"schemaVersion": 1, "provider": provider, "displayText": text})
    }

    fn host_with(provider: &str, envelope: Value) -> UsageFakeHost {
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "m", "provider": provider}));
        host.set_providers(vec![providers::alias_for(provider).unwrap().to_owned()]);
        host.set_envelope(providers::alias_for(provider).unwrap(), envelope);
        host
    }

    #[test]
    fn refresh_publishes_and_keeps_the_line_on_failure() {
        let host = host_with(
            "zai",
            envelope("glm-coding-plan", "glm-coding-plan: 5h 1% used"),
        );
        let mut state = FooterState::default();
        let now = Instant::now();
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, now),
            RefreshOutcome::Updated {
                provider: "glm-coding-plan".to_owned(),
                text: "glm-coding-plan: 5h 1% used".to_owned(),
            }
        );
        assert_eq!(
            host.statuses(),
            vec![(
                "rpi-usage".to_owned(),
                Some("glm-coding-plan: 5h 1% used".to_owned())
            )]
        );
        // Failure for the same provider: the previous line stays (no second
        // status write).
        host.fail_fetch("glm-coding-plan");
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, now),
            RefreshOutcome::KeptLast {
                provider: "glm-coding-plan".to_owned(),
            }
        );
        assert_eq!(host.statuses().len(), 1, "no flicker write");
    }

    #[test]
    fn failure_without_a_previous_success_clears_and_provider_switch_clears() {
        let host = host_with("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        host.fail_fetch("deepseek");
        let mut state = FooterState::default();
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, Instant::now()),
            RefreshOutcome::Skipped
        );
        // A successful line, then a model switch to an unsupported provider
        // removes it (stale data from another provider must not remain).
        let host = host_with("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        let mut state = FooterState::default();
        refresh(&host, &config(), &mut state, false, false, Instant::now());
        host.set_model(json!({"id": "m", "provider": "dgx-spark"}));
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, Instant::now()),
            RefreshOutcome::Cleared
        );
        assert_eq!(
            host.statuses().last(),
            Some(&("rpi-usage".to_owned(), None))
        );
    }

    #[test]
    fn throttle_disabled_and_no_ui_paths_skip_fetches() {
        let host = host_with("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        let mut state = FooterState::default();
        let now = Instant::now();
        refresh(&host, &config(), &mut state, false, false, now);
        assert_eq!(
            refresh(
                &host,
                &config(),
                &mut state,
                false,
                true,
                now + Duration::from_millis(10)
            ),
            RefreshOutcome::Skipped
        );
        assert_eq!(host.fetch_count("deepseek"), 1, "throttle skips the fetch");
        // Force bypasses the throttle.
        refresh(&host, &config(), &mut state, true, true, now);
        assert_eq!(host.fetch_count("deepseek"), 2);
        // Feature switches clear the entry.
        let disabled = config::UsageConfig {
            enabled: false,
            ..config()
        };
        assert_eq!(
            refresh(&host, &disabled, &mut state, false, false, Instant::now()),
            RefreshOutcome::Cleared
        );
        let no_ui = host_with("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        no_ui.set_has_ui(false);
        let mut state = FooterState::default();
        assert_eq!(
            refresh(&no_ui, &config(), &mut state, false, false, Instant::now()),
            RefreshOutcome::Skipped
        );
        assert!(
            !no_ui.called("ctx.usage.fetch"),
            "dormant sessions never fetch"
        );
    }

    #[test]
    fn unknown_script_and_empty_display_text_degrade_cleanly() {
        // A provider alias exists but no script is registered.
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "m", "provider": "zai"}));
        let mut state = FooterState::default();
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, Instant::now()),
            RefreshOutcome::Skipped
        );
        assert!(!host.called("ctx.usage.fetch"));
        // An envelope without displayText is a failure, not an empty line.
        let host = host_with(
            "deepseek",
            json!({"schemaVersion": 1, "provider": "deepseek"}),
        );
        let mut state = FooterState::default();
        assert_eq!(
            refresh(&host, &config(), &mut state, false, false, Instant::now()),
            RefreshOutcome::Skipped
        );
        assert_eq!(host.statuses().len(), 0);
    }
    /// v0.1.6 review P1-6: a stale worker's exit must not unregister the
    /// worker a new session started. Round-2 review O3: this test touches
    /// the process-global WORKERS map, so it must hold TEST_LOCK (the
    /// lib.rs dispatch tests install/stop workers under the same lock).
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
