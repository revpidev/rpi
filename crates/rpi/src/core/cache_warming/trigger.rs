//! Trigger — the cache-warmer lifecycle state machine
//! (cache-warmer.ts:141-385 @ c596d09d9, #9668).
//!
//! `start` replaces any previous run (aborting its in-flight refresh); the
//! scheduling deadline re-arms after each refresh. Warm requests never
//! extend the fixed safety windows (60 min streaming / 30 min idle).
//! Scheduling and the safety windows use `tokio::time::Instant` so the
//! whole cycle is testable under `#[tokio::test(start_paused = true)]`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rpi_agent::session::UsageEntry;
use rpi_ai::models::ModelsSimpleStreamOptions;
use rpi_ai::types::{Context, Model, ThinkingLevel};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use tokio_util::sync::CancellationToken;

use crate::core::settings_manager::CacheWarmingMode;

use super::decision::{
    evaluate_decision, get_cache_warming_delay_ms, get_prompt_cache_ttl_ms, is_replayable,
    last_prompt_tokens, CacheWarmingDecision, MAX_IDLE_WARMING_AGE_MS, MAX_WARMING_AGE_MS,
};
use super::{CacheWarmingStatus, WarmingState};

/// `Pick<ModelRuntime, "streamSimple">` (cache-warmer.ts:162) — the warmer's
/// entire view of the model runtime. `ModelRuntime` implements this at the
/// `sdk.rs` wiring; tests inject fakes.
pub trait WarmingModels: Send + Sync {
    fn warming_stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> AssistantMessageEventStream;
}

/// The decide hook's boxed future (async extension dispatch).
pub type DecideFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = rpi_ext_host::types::CacheWarmingAction> + Send>,
>;

/// `CacheWarmer`'s decision hook: receives the pending decision event,
/// returns the effective action (extension overrides land inside the
/// closure; failures fall back to pi's own action — cache-warmer.ts:166-168,
/// runner errors already reported by the runner).
pub type CacheWarmingDecide =
    Arc<dyn Fn(rpi_ext_host::types::CacheWarmingDecisionEvent) -> DecideFuture + Send + Sync>;

/// `isCurrent: () => boolean` (cache-warmer.ts:143) — false once the
/// session's model or messages no longer match the request.
pub type IsCurrent = Arc<dyn Fn() -> bool + Send + Sync>;

/// `CacheWarmRequest` (cache-warmer.ts:135-139): the request whose prompt
/// cache entry should be kept warm, exactly as it was sent (options already
/// carry the merged timeout/retry/header pipeline from the session's
/// stream function).
#[derive(Clone)]
pub struct CacheWarmRequest {
    pub model: Model,
    pub context: Context,
    pub options: ModelsSimpleStreamOptions,
}

/// `phase` (cache-warmer.ts:147).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// While the agent run that sent the request is still active.
    Streaming,
    /// After the agent run settled (idle mode only).
    Idle,
}

impl Phase {
    fn safety_window_ms(self) -> u64 {
        match self {
            Phase::Streaming => MAX_WARMING_AGE_MS,
            Phase::Idle => MAX_IDLE_WARMING_AGE_MS,
        }
    }
}

/// `ActiveRun` (cache-warmer.ts:141-157). Guarded by the state mutex;
/// `generation` identifies the run across spawned refresh tasks (the
/// upstream object-identity check `this.run === run`).
pub(super) struct ActiveRun {
    pub(super) generation: u64,
    pub(super) request: CacheWarmRequest,
    pub(super) is_current: IsCurrent,
    pub(super) delay_ms: u64,
    pub(super) started_at: tokio::time::Instant,
    pub(super) token: CancellationToken,
    pub(super) phase: Phase,
    /// Wall-clock epoch ms for `/session` display.
    pub(super) next_warm_at: u64,
    /// Pausable-clock deadline of the armed refresh (the enforcement copy;
    /// `next_warm_at` is display-only).
    pub(super) next_warm_deadline: tokio::time::Instant,
    /// `run.timer === undefined` ⇔ a refresh is in flight.
    pub(super) refreshing: bool,
    pub(super) extension_override: bool,
}

impl ActiveRun {
    /// `run.startedAt + (idle ? MAX_IDLE_WARMING_AGE_MS : MAX_WARMING_AGE_MS)`
    /// (cache-warmer.ts:282).
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.started_at + std::time::Duration::from_millis(self.phase.safety_window_ms())
    }
}

pub(super) struct WarmerState {
    pub(super) run: Option<ActiveRun>,
    pub(super) inactive: CacheWarmingStatus,
}

/// Constructor dependencies (cache-warmer.ts:162-168).
pub struct CacheWarmerDeps {
    pub models: Arc<dyn WarmingModels>,
    pub session: Arc<Mutex<crate::core::session_manager::SessionManager>>,
    pub get_mode: Arc<dyn Fn() -> CacheWarmingMode + Send + Sync>,
    pub decide: CacheWarmingDecide,
}

impl CacheWarmerDeps {
    /// The default decide hook (cache-warmer.ts:174-176): pi's own action.
    pub fn default_decide() -> CacheWarmingDecide {
        Arc::new(|event| Box::pin(async move { event.action }) as DecideFuture)
    }
}

/// `onWarmed?: (entry: UsageEntry) => void` (cache-warmer.ts:168).
pub type OnWarmed = Arc<dyn Fn(&UsageEntry) + Send + Sync>;

/// `CacheWarmer` (cache-warmer.ts:159-385).
pub struct CacheWarmer {
    pub(super) deps: CacheWarmerDeps,
    pub(super) state: Mutex<WarmerState>,
    on_warmed: RwLock<Option<OnWarmed>>,
    pub(super) next_generation: AtomicU64,
}

impl CacheWarmer {
    /// Constructor (cache-warmer.ts:170-181).
    pub fn new(deps: CacheWarmerDeps) -> Self {
        CacheWarmer {
            deps,
            state: Mutex::new(WarmerState {
                run: None,
                inactive: CacheWarmingStatus::inactive("waiting for first request"),
            }),
            on_warmed: RwLock::new(None),
            next_generation: AtomicU64::new(0),
        }
    }

    /// `onWarmed` callback (cache-warmer.ts:168): called with the persisted
    /// usage entry after each successful refresh; wired by `AgentSession`
    /// to re-emit `entry_appended` (agent-session.ts:403).
    pub fn set_on_warmed(&self, callback: Option<OnWarmed>) {
        *lock_write(&self.on_warmed) = callback;
    }

    pub(super) fn fire_on_warmed(&self, entry: &UsageEntry) {
        let callback = {
            let reader = self.on_warmed.read().unwrap_or_else(|e| e.into_inner());
            reader.clone()
        };
        if let Some(callback) = callback {
            callback(entry);
        }
    }

    /// `status` getter (cache-warmer.ts:183-200).
    pub fn status(&self) -> CacheWarmingStatus {
        if (self.deps.get_mode)() == CacheWarmingMode::Off {
            return CacheWarmingStatus::inactive("cache warming disabled");
        }
        let state = lock(&self.state);
        let Some(run) = &state.run else {
            return state.inactive.clone();
        };
        if !(run.is_current)() {
            return CacheWarmingStatus::inactive("conversation context changed");
        }
        let decision = self.evaluate_locked(run);
        if !decision.economics_available && !run.refreshing {
            return CacheWarmingStatus::inactive("cache economics unavailable");
        }
        CacheWarmingStatus {
            state: if run.refreshing {
                WarmingState::Refreshing
            } else {
                WarmingState::Scheduled
            },
            reason: None,
            next_warm_at: Some(run.next_warm_at),
            decision: Some(decision),
            extension_override: run.extension_override,
        }
    }

    /// `start` (cache-warmer.ts:202-238): keep the prompt cache entry
    /// written by `request` warm while `is_current` holds. Replaces any
    /// previous run. `self: &Arc<Self>` because scheduling spawns the
    /// refresh task.
    pub fn start(self: &Arc<Self>, request: CacheWarmRequest, is_current: IsCurrent) {
        self.clear_run();
        let mode = (self.deps.get_mode)();
        if mode == CacheWarmingMode::Off {
            self.stop("cache warming disabled", None);
            return;
        }
        // The captured `SimpleStreamOptions.reasoning` is the ai-side level:
        // `Some` ⇔ thinking requested ("off" is normalized away at the
        // stream_simple boundary — upstream `options?.reasoning` truthiness).
        let reasoning: Option<ThinkingLevel> = request.options.simple.reasoning;
        if !is_replayable(&request.model, reasoning.as_ref()) {
            self.stop("request cannot be replayed safely", None);
            return;
        }
        let ttl_ms = get_prompt_cache_ttl_ms(
            &request.model,
            request.options.simple.stream.cache_retention,
            request.options.simple.stream.env.as_ref(),
        );
        let Some(ttl_ms) = ttl_ms else {
            self.stop(
                if request.options.simple.stream.cache_retention
                    == Some(rpi_ai::types::CacheRetention::None)
                {
                    "request disabled prompt caching"
                } else {
                    "cache lifetime unavailable"
                },
                None,
            );
            return;
        };
        let Some(delay_ms) = get_cache_warming_delay_ms(ttl_ms) else {
            self.stop("cache lifetime unavailable", None);
            return;
        };
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut state = lock(&self.state);
            state.run = Some(ActiveRun {
                generation,
                request,
                is_current,
                delay_ms,
                started_at: tokio::time::Instant::now(),
                token: CancellationToken::new(),
                phase: Phase::Streaming,
                next_warm_at: 0,
                next_warm_deadline: tokio::time::Instant::now(),
                refreshing: false,
                extension_override: false,
            });
        }
        self.schedule(generation);
    }

    /// `onAgentSettled` (cache-warmer.ts:240-253): the agent run that sent
    /// the request finished. Streaming mode stops; idle mode continues
    /// under the shorter 30-minute horizon.
    pub fn on_agent_settled(&self) {
        let (stop_reason, expired) = {
            let mut state = lock(&self.state);
            let Some(run) = &mut state.run else {
                return;
            };
            if (self.deps.get_mode)() == CacheWarmingMode::Streaming {
                (Some("agent run settled".to_owned()), false)
            } else {
                run.phase = Phase::Idle;
                let deadline = run.deadline();
                let now = tokio::time::Instant::now();
                (None, run.next_warm_deadline > deadline || now >= deadline)
            }
        };
        if stop_reason.is_some() {
            self.stop("agent run settled", None);
        } else if expired {
            self.stop("30-minute idle safety limit reached", None);
        }
    }

    /// `onModeChanged` (cache-warmer.ts:255-260): reconcile an active run
    /// after the persisted warming mode changes.
    pub fn on_mode_changed(&self) {
        let reason = {
            let mode = (self.deps.get_mode)();
            let state = lock(&self.state);
            let Some(run) = &state.run else {
                return;
            };
            self.mode_stop_reason(mode, run.phase)
        };
        if let Some(reason) = reason {
            self.stop(&reason, None);
        }
    }

    /// `cancel` (cache-warmer.ts:262-264).
    pub fn cancel(&self) {
        self.stop("inactive", None);
    }

    // -- internals ----------------------------------------------------------

    /// `evaluate` (cache-warmer.ts:362-379) over a locked run.
    pub(super) fn evaluate_locked(&self, run: &ActiveRun) -> CacheWarmingDecision {
        let prompt_tokens = self.branch_prompt_tokens();
        evaluate_decision(&run.request.model, prompt_tokens, run.phase)
    }

    pub(super) fn branch_prompt_tokens(&self) -> u64 {
        let branch = {
            let session = lock(&self.deps.session);
            session.get_branch(None)
        };
        let known: Vec<_> = branch.iter().filter_map(|e| e.known()).cloned().collect();
        last_prompt_tokens(&known)
    }

    pub(super) fn clear_run(&self) {
        let mut state = lock(&self.state);
        if let Some(run) = state.run.take() {
            run.token.cancel();
        }
    }

    /// `stop` (cache-warmer.ts:274-277).
    pub(super) fn stop(&self, reason: &str, stopped: Option<(CacheWarmingDecision, bool)>) {
        self.clear_run();
        let mut state = lock(&self.state);
        state.inactive = CacheWarmingStatus {
            state: WarmingState::Inactive,
            reason: Some(reason.to_owned()),
            next_warm_at: None,
            decision: stopped.as_ref().map(|(decision, _)| *decision),
            extension_override: stopped.map(|(_, override_)| override_).unwrap_or(false),
        };
    }

    /// `getModeStopReason` (cache-warmer.ts:355-360).
    pub(super) fn mode_stop_reason(&self, mode: CacheWarmingMode, phase: Phase) -> Option<String> {
        if mode == CacheWarmingMode::Off {
            Some("cache warming disabled".to_owned())
        } else if mode == CacheWarmingMode::Streaming && phase == Phase::Idle {
            Some("agent run settled".to_owned())
        } else {
            None
        }
    }

    /// `validateRun` (cache-warmer.ts:347-353): true when `generation` is
    /// still the active run and nothing disqualifies it; a disqualifying
    /// run is stopped and reported as invalid.
    pub(super) fn validate_run(&self, generation: u64) -> bool {
        let reason = {
            let state = lock(&self.state);
            let Some(run) = &state.run else {
                return false;
            };
            if run.generation != generation {
                return false;
            }
            let mode = (self.deps.get_mode)();
            self.mode_stop_reason(mode, run.phase).or_else(|| {
                if !(run.is_current)() {
                    Some("conversation context changed".to_owned())
                } else {
                    None
                }
            })
        };
        match reason {
            None => true,
            Some(reason) => {
                self.stop(&reason, None);
                false
            }
        }
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_write<T>(rw: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rw.write().unwrap_or_else(|e| e.into_inner())
}

// `schedule` and `refresh` (the executor half of the cycle) are `CacheWarmer`
// methods in `executor.rs`; inherent impls resolve across sibling modules.

#[cfg(test)]
mod tests;
