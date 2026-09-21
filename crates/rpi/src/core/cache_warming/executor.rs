//! 执行器 — one refresh cycle of the cache warmer
//! (cache-warmer.ts:279-345 @ c596d09d9, #9668).
//!
//! `schedule` arms the next refresh (aborting runs whose next refresh would
//! land beyond the phase safety window); `refresh` re-evaluates the
//! economics, dispatches the `cache_warming_decision` extension event,
//! replays the request with a one-token output cap (`maxRetries: 0`, own
//! abort signal), persists usage, fires `onWarmed`, and re-arms.

use std::sync::Arc;
use std::time::Duration;

use rpi_ai::types::StopReason;
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use rpi_ext_host::types::{CacheWarmingAction, CacheWarmingDecisionEvent};

use super::trigger::{lock, CacheWarmer, Phase};

/// Wall-clock epoch ms (`Date.now()`) for the `/session` display field.
fn wall_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl CacheWarmer {
    /// `schedule` (cache-warmer.ts:310-317). `self: &Arc<Self>` because the
    /// timer task outlives the caller.
    pub(super) fn schedule(self: &Arc<Self>, generation: u64) {
        let (beyond_deadline, sleep_until, phase_now) = {
            let mut state = lock(&self.state);
            let Some(run) = &mut state.run else {
                return;
            };
            if run.generation != generation {
                return;
            }
            run.extension_override = false;
            let now = tokio::time::Instant::now();
            let next = now + Duration::from_millis(run.delay_ms);
            run.next_warm_deadline = next;
            run.next_warm_at = wall_epoch_ms().saturating_add(run.delay_ms);
            run.refreshing = false;
            let deadline = run.deadline();
            (next > deadline || now >= deadline, next, run.phase)
        };
        if beyond_deadline {
            // Warm requests never extend the fixed safety windows
            // (cache-warmer.ts:282-284).
            let reason = match phase_now {
                Phase::Idle => "30-minute idle safety limit reached",
                Phase::Streaming => "one-hour safety limit reached",
            };
            self.stop(reason, None);
            return;
        }
        let warmer = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep_until(sleep_until).await;
            warmer.refresh(generation).await;
        });
    }

    /// `refresh` (cache-warmer.ts:291-345). Best-effort: every failure path
    /// either stops the run (recorded in `/session`) or re-arms the timer.
    pub(super) async fn refresh(self: &Arc<Self>, generation: u64) {
        // `run.timer = undefined` — the armed timer fired (cache-warmer.ts:292).
        {
            let mut state = lock(&self.state);
            let Some(run) = &mut state.run else {
                return;
            };
            if run.generation != generation {
                return;
            }
            run.refreshing = true;
        }
        if !self.validate_run(generation) {
            return;
        }
        let (decision, request, token) = {
            let state = lock(&self.state);
            let Some(run) = &state.run else {
                return;
            };
            if run.generation != generation {
                return;
            }
            (
                self.evaluate_locked(run),
                run.request.clone(),
                run.token.clone(),
            )
        };
        let event = CacheWarmingDecisionEvent {
            warm_cost: decision.warm_cost,
            miss_cost: decision.miss_cost,
            continuation_probability: decision.continuation_probability,
            action: decision.action,
        };
        // Extension failures fall back to pi's own decision
        // (cache-warmer.ts:296-301).
        let action = ((self.deps.decide)(event)).await;
        if !self.validate_run(generation) {
            return;
        }
        let extension_override = action != decision.action;
        if action == CacheWarmingAction::Stop {
            // cache-warmer.ts:310-317.
            let reason = if extension_override {
                "stopped by extension"
            } else if decision.economics_available {
                "expected savings below threshold"
            } else {
                "cache economics unavailable"
            };
            self.stop(reason, Some((decision, extension_override)));
            return;
        }
        {
            let mut state = lock(&self.state);
            let Some(run) = &mut state.run else {
                return;
            };
            if run.generation == generation {
                run.extension_override = extension_override;
            }
        }
        // Execute: replay the request with a one-token output cap
        // (cache-warmer.ts:322-328). Everything else about the request —
        // reasoning, session affinity headers, timeouts, env — is preserved
        // exactly as it was sent.
        let mut options = request.options;
        options.simple.stream.max_tokens = Some(1);
        options.simple.stream.max_retries = Some(0);
        options.simple.stream.request.signal = Some(token);
        let stream: AssistantMessageEventStream =
            self.deps
                .models
                .warming_stream_simple(&request.model, &request.context, Some(options));
        let message = stream.result().await;
        if !self.validate_run(generation) {
            return;
        }
        if let Some(message) = message {
            // Failed refreshes are not recorded (cache-warmer.ts:329-336).
            if message.stop_reason != StopReason::Error
                && message.stop_reason != StopReason::Aborted
            {
                let note = extension_override.then_some("extension override");
                let usage_model = message
                    .response_model
                    .clone()
                    .unwrap_or_else(|| message.model.clone());
                let entry = {
                    let mut session = lock(&self.deps.session);
                    session.append_usage(
                        "cache_warm",
                        &message.provider,
                        &usage_model,
                        message.usage.clone(),
                        note,
                    )
                };
                if let Ok(entry) = entry {
                    self.fire_on_warmed(&entry);
                }
            }
        }
        // `if (this.run === run) this.schedule(run)` (cache-warmer.ts:344).
        let still_active = {
            let state = lock(&self.state);
            state
                .run
                .as_ref()
                .map(|run| run.generation == generation)
                .unwrap_or(false)
        };
        if still_active {
            self.schedule(generation);
        }
    }
}
