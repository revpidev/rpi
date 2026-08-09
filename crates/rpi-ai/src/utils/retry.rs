//! Port of `packages/ai/src/utils/retry.ts` @ pi 0.82.1 (2efa728).
//!
//! Outer assistant-call retry: error classification regex tables + bounded
//! exponential backoff (`baseDelayMs * 2^(attempt-1)`).

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, StopReason};

fn build_provider_error_pattern(patterns: &[&str]) -> Regex {
    // invariant: pinned literal patterns ported from retry.ts; they compile
    // (verified by the tests below).
    Regex::new(&format!("(?i){}", patterns.join("|"))).expect("static retry pattern must compile")
}

fn non_retryable_provider_limit_error_pattern() -> &'static Regex {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        build_provider_error_pattern(&[
            // OpenCode Go/free-tier subscription/account limits.
            "GoUsageLimitError",
            "FreeUsageLimitError",
            "Monthly usage limit reached",
            "available balance",
            // Generic quota/budget/billing exhaustion.
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
        ])
    });
    &PATTERN
}

fn retryable_provider_error_pattern() -> &'static Regex {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        build_provider_error_pattern(&[
            "overloaded",
            "rate.?limit",
            "too many requests",
            "429",
            "500",
            "502",
            "503",
            "504",
            "524",
            "service.?unavailable",
            "server.?error",
            "internal.?error",
            "provider.?returned.?error",
            "network.?error",
            "connection.?error",
            "connection.?refused",
            "connection.?lost",
            "other side closed",
            "fetch failed",
            "getaddrinfo",
            "ENOTFOUND",
            "EAI_AGAIN",
            "upstream.?connect",
            "reset before headers",
            "socket hang up",
            "socket connection was closed",
            "timed? out",
            "timeout",
            "terminated",
            "websocket.?closed",
            "websocket.?error",
            "ended without",
            "stream ended before message_stop",
            "stream ended before a terminal response event",
            "http2 request did not get a response",
            "retry delay",
            "you can retry your request",
            "try your request again",
            "please retry your request",
            "ResourceExhausted",
        ])
    });
    &PATTERN
}

/// `RetryPolicy` — matches `settings.retry` in coding-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries). The initial call never counts.
    pub max_retries: u32,
    /// Base delay in ms. Per-attempt delay is `baseDelayMs * 2^(attempt-1)`.
    pub base_delay_ms: u64,
}

type RetryCallback<Args> = dyn Fn(Args) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

/// `onRetryScheduled` args: (attempt, max_attempts, delay_ms, error_message).
pub type RetryScheduledArgs = (u32, u32, u64, String);
/// `onRetryFinished` args: (success, attempt, final_error).
pub type RetryFinishedArgs = (bool, u32, Option<String>);

/// Optional callbacks emitted by [`retry_assistant_call`] around each retry.
#[derive(Default)]
pub struct RetryCallbacks {
    /// Before the backoff sleep of each attempt: (attempt, max_attempts,
    /// delay_ms, error_message).
    pub on_retry_scheduled: Option<Box<RetryCallback<RetryScheduledArgs>>>,
    /// After the backoff sleep, immediately before the retried call starts.
    pub on_retry_attempt_start: Option<Box<RetryCallback<()>>>,
    /// Once when the loop ends: (success, attempt, final_error).
    pub on_retry_finished: Option<Box<RetryCallback<RetryFinishedArgs>>>,
}

/// `retryAssistantCall`: runs a single assistant-producing call with bounded
/// retry on transient errors. Aborts are terminal and never retried; aborts
/// during the backoff sleep are normalized to an aborted `AssistantMessage`.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    signal: Option<&CancellationToken>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_attempts = match policy {
        Some(policy) if policy.enabled => policy.max_retries,
        _ => 0,
    };

    let mut attempt: u32 = 0;
    let mut last_retry: Option<(u32, String)> = None;
    loop {
        let response = produce().await;

        // Abort: terminal but not successful. Never retry an aborted message.
        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                    cb((false, attempt, None)).await;
                }
            }
            return response;
        }

        // Success: non-error, non-abort responses return as-is.
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                    cb((true, attempt, None)).await;
                }
            }
            return response;
        }

        // Non-retryable, or budget exhausted: return the final error message.
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry {
                if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                    cb((false, attempt, response.error_message.clone())).await;
                }
            }
            return response;
        }

        attempt += 1;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_owned());
        last_retry = Some((attempt, error_message.clone()));
        let base_delay_ms = policy.map(|p| p.base_delay_ms).unwrap_or(0);
        let delay_ms = base_delay_ms * 2u64.saturating_pow(attempt - 1);
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_scheduled.as_ref()) {
            cb((attempt, max_attempts, delay_ms, error_message.clone())).await;
        }

        // Normalize aborts during retry backoff to the same shape as provider
        // stream aborts.
        if !abortable_sleep(delay_ms, signal).await {
            if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                cb((false, attempt, Some(error_message))).await;
            }
            let mut aborted = response;
            aborted.stop_reason = StopReason::Aborted;
            aborted.error_message = None;
            return aborted;
        }
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_attempt_start.as_ref()) {
            cb(()).await;
        }
    }
}

/// Interruptible sleep; returns `false` when the signal fired first.
async fn abortable_sleep(ms: u64, signal: Option<&CancellationToken>) -> bool {
    match signal {
        None => {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            true
        }
        Some(token) => {
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(ms)) => true,
                () = token.cancelled() => false,
            }
        }
    }
}

/// `isRetryableAssistantError`: classifies whether a failed assistant message
/// looks like a transient provider/transport error.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if non_retryable_provider_limit_error_pattern().is_match(error_message) {
        return false;
    }
    retryable_provider_error_pattern().is_match(error_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ApiKind, AssistantRole, Usage};

    fn assistant(stop_reason: StopReason, error_message: Option<&str>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![],
            api: ApiKind::from("openai-completions"),
            provider: "openai".to_owned(),
            model: "m".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason,
            error_message: error_message.map(str::to_owned),
            timestamp: 0,
        }
    }

    #[test]
    fn test_is_retryable_assistant_error_classification() {
        assert!(is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("429 too many requests")
        )));
        assert!(is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("connection reset by peer: connection error")
        )));
        assert!(is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("Server requested 90s retry delay (max: 60s)")
        )));
        // Quota/billing exhaustion: not retryable.
        assert!(!is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("429 insufficient_quota")
        )));
        assert!(!is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("billing hard limit reached; 429")
        )));
        // Unclassified errors are not retryable.
        assert!(!is_retryable_assistant_error(&assistant(
            StopReason::Error,
            Some("authentication failed")
        )));
        assert!(!is_retryable_assistant_error(&assistant(
            StopReason::Stop,
            None
        )));
        assert!(!is_retryable_assistant_error(&assistant(
            StopReason::Error,
            None
        )));
    }

    #[tokio::test]
    async fn test_retry_assistant_call_success_first_try() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
        };
        let response = retry_assistant_call(
            || async { assistant(StopReason::Stop, None) },
            Some(&policy),
            None,
            None,
        )
        .await;
        assert_eq!(response.stop_reason, StopReason::Stop);
    }

    #[tokio::test]
    async fn test_retry_assistant_call_retries_then_succeeds() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 2,
            base_delay_ms: 1,
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls2 = calls.clone();
        let response = retry_assistant_call(
            move || {
                let calls = calls2.clone();
                async move {
                    let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 2 {
                        assistant(StopReason::Error, Some("503 service unavailable"))
                    } else {
                        assistant(StopReason::Stop, None)
                    }
                }
            },
            Some(&policy),
            None,
            None,
        )
        .await;
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_assistant_call_non_retryable_fails_fast() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls2 = calls.clone();
        let response = retry_assistant_call(
            move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assistant(StopReason::Error, Some("insufficient_quota"))
                }
            },
            Some(&policy),
            None,
            None,
        )
        .await;
        assert_eq!(response.stop_reason, StopReason::Error);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_assistant_call_abort_during_backoff() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 10_000,
        };
        let token = CancellationToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            token2.cancel();
        });
        let response = retry_assistant_call(
            || async { assistant(StopReason::Error, Some("503")) },
            Some(&policy),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.stop_reason, StopReason::Aborted);
        assert_eq!(response.error_message, None);
    }

    #[tokio::test]
    async fn test_retry_assistant_call_aborted_response_never_retried() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls2 = calls.clone();
        let response = retry_assistant_call(
            move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assistant(StopReason::Aborted, Some("Operation aborted"))
                }
            },
            Some(&policy),
            None,
            None,
        )
        .await;
        assert_eq!(response.stop_reason, StopReason::Aborted);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
