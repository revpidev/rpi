//! `configureHttpDispatcher` runtime seam — port of
//! `packages/coding-agent/src/core/http-dispatcher.ts` (T13, #59).
//!
//! Upstream swaps the process-global undici dispatcher whenever the HTTP
//! idle timeout changes (`configureHttpDispatcher`, http-dispatcher.ts:81-113;
//! called at startup, on session rebind — interactive-mode.ts:2063 — and from
//! the `/settings` arm — interactive-mode.ts:5000-5004). rpi's provider
//! requests do not go through a global dispatcher: each streamed request
//! carries its idle budget as `StreamOptions::timeout_ms`, consumed by
//! `rpi_ai::api::stream_timeouts`. This module carries the reconfigurable
//! slot that the session's stream function reads when a request does not
//! override the budget, so the settings arm reaches in-flight sessions
//! without a restart.
//!
//! Shape differences from upstream (startup-time reconfiguration only):
//! - The handle is owned by the session instead of a process-global; a
//!   session is the unit that owns the stream function here.
//! - `parseHttpIdleTimeoutMs` validation lives in
//!   [`crate::core::settings_manager`]; [`HttpDispatcher::configure`] takes
//!   an already-normalized `u64` (`0` = disabled).

use std::sync::atomic::{AtomicU64, Ordering};

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (http-dispatcher.ts:4): upstream's 5-minute
/// default, kept as the upstream reference ([`HttpDispatcher::default`]).
/// Production code seeds the slot from the settings manager
/// (`settings_manager::DEFAULT_HTTP_IDLE_TIMEOUT_MS`, disabled by default per
/// rpi#54 / D-103).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Shared, atomically reconfigurable HTTP idle-timeout slot.
///
/// Created once per session and captured by its stream function; the
/// settings chain reconfigures it through
/// [`crate::core::agent_session::AgentSession::configure_http_dispatcher`].
#[derive(Debug)]
pub struct HttpDispatcher {
    timeout_ms: AtomicU64,
}

impl HttpDispatcher {
    /// A dispatcher seeded with `timeout_ms` (`0` = disabled).
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout_ms: AtomicU64::new(timeout_ms),
        }
    }

    /// `configureHttpDispatcher(timeoutMs)` (http-dispatcher.ts:81): replace
    /// the idle budget for subsequent requests.
    pub fn configure(&self, timeout_ms: u64) {
        self.timeout_ms.store(timeout_ms, Ordering::Relaxed);
    }

    /// The current idle budget in milliseconds (`0` = disabled).
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.load(Ordering::Relaxed)
    }
}

impl Default for HttpDispatcher {
    fn default() -> Self {
        Self::new(DEFAULT_HTTP_IDLE_TIMEOUT_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_replaces_the_budget() {
        let dispatcher = HttpDispatcher::new(0);
        assert_eq!(dispatcher.timeout_ms(), 0);
        dispatcher.configure(600_000);
        assert_eq!(dispatcher.timeout_ms(), 600_000);
        dispatcher.configure(0);
        assert_eq!(dispatcher.timeout_ms(), 0);
    }

    #[test]
    fn default_uses_the_upstream_constant() {
        assert_eq!(
            HttpDispatcher::default().timeout_ms(),
            DEFAULT_HTTP_IDLE_TIMEOUT_MS
        );
    }
}
