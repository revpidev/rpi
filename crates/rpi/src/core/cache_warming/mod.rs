//! Prompt cache warming (#9668, upstream `c596d09d9`).
//!
//! Port of `packages/coding-agent/src/core/cache-warmer.ts` @ pi v0.86.1
//! (`19451accd`). Keeps one prompt cache entry alive by re-sending its
//! request with a one-token output cap before the entry expires.
//!
//! Module split (task file §3 four-file tree; the upstream single class is
//! divided by responsibility):
//! - [`decision`] — 决策器: cost-aware pure functions (eligibility, TTL,
//!   delay, economics, formatters).
//! - [`trigger`] — 触发器: lifecycle state machine (`start` on each session
//!   request, agent-run phase transitions, mode reconcile, scheduling).
//! - [`executor`] — 执行器: one refresh request (decision → extension
//!   override → `stream_simple` with `maxTokens: 1` / `maxRetries: 0` →
//!   `appendUsage` → `onWarmed` → reschedule).
//!
//! Upstream trigger anchoring note (task §3 pre-implementation wording
//! corrected during port): the trigger is NOT a tool-execution observer —
//! it is the session-request hook (`streamFn`, rpi `sdk.rs`) plus the agent
//! run lifecycle (`onAgentSettled`). "streaming" mode warms while the agent
//! run that sent the request is still active (long tool executions keep the
//! run active); "idle" mode continues after settle under a 30-minute
//! horizon. Compaction/summary requests never start warming (they carry
//! their own routing ids — `session_id` differs from the session manager
//! id; `sdk.ts:387-391`).
//!
//! Time: scheduling and safety windows use `tokio::time::Instant` so the
//! state machine is testable under `start_paused`; the `next_warm_at`
//! display field is wall-clock epoch ms (formatting only).

mod decision;
mod executor;
mod trigger;

pub use decision::{
    format_cache_warming_status, format_cache_warming_usage, get_cache_warming_delay_ms,
    get_prompt_cache_ttl_ms, is_replayable, last_prompt_tokens, CacheWarmingDecision,
    CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS, IDLE_CONTINUATION_PROBABILITY, MAX_IDLE_WARMING_AGE_MS,
    MAX_WARMING_AGE_MS,
};
pub use trigger::{
    CacheWarmRequest, CacheWarmer, CacheWarmerDeps, CacheWarmingDecide, DecideFuture, IsCurrent,
    Phase, WarmingModels,
};

pub use rpi_ext_host::types::CacheWarmingAction;

/// `CacheWarmingStatus.state` (cache-warmer.ts:100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmingState {
    /// Nothing scheduled (with `reason`).
    Inactive,
    /// A refresh timer is armed.
    Scheduled,
    /// A warm request is in flight.
    Refreshing,
}

/// `CacheWarmingStatus` (cache-warmer.ts:98-106) — `/session` view.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheWarmingStatus {
    pub state: WarmingState,
    pub reason: Option<String>,
    /// Wall-clock epoch ms of the next scheduled decision.
    pub next_warm_at: Option<u64>,
    /// The pending decision, or the decision that stopped warming.
    pub decision: Option<CacheWarmingDecision>,
    /// True when an extension changed `decision.action`.
    pub extension_override: bool,
}

impl CacheWarmingStatus {
    pub fn inactive(reason: &str) -> Self {
        CacheWarmingStatus {
            state: WarmingState::Inactive,
            reason: Some(reason.to_owned()),
            next_warm_at: None,
            decision: None,
            extension_override: false,
        }
    }
}
