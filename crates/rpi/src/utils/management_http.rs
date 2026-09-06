//! Port of `packages/coding-agent/src/utils/management-http.ts` @ 4181f66
//! (commit 46b53b995): bounded immediate retry for idempotent management
//! HTTP requests.
//!
//! Intentional differences:
//! - The upstream wraps the global `fetch` and returns a `Response`; this
//!   port operates on `reqwest::RequestBuilder` and returns
//!   `reqwest::Response` (the project HTTP layer is reqwest + rustls).
//! - The timeout budget is implemented with `tokio::time::Instant` /
//!   `Duration` arithmetic instead of `AbortSignal.timeout`, which is
//!   semantically equivalent (a shared deadline across all attempts).
//! - Caller cancellation uses [`CancellationToken`] instead of `AbortSignal`;
//!   cancellation is terminal and never retried (same as upstream).

use std::future::Future;
use std::time::Duration;

use reqwest::RequestBuilder;
use tokio_util::sync::CancellationToken;

/// `RETRYABLE_STATUS_CODES` (management-http.ts:3): 408, 425, 429, 500, 502,
/// 503, 504.
pub const RETRYABLE_STATUS_CODES: &[u16] = &[408, 425, 429, 500, 502, 503, 504];

/// Options mirror [`FetchRetryOptions`](management-http.ts:6-12).
#[derive(Default)]
pub struct FetchRetryOptions {
    /// Number of additional attempts after the initial request. Defaults to 2
    /// when `None` (`maxRetries`, management-http.ts:30-33).
    pub max_retries: Option<usize>,
    /// Retry transient HTTP responses in addition to transport failures.
    /// Defaults to `true` (`retryOnStatus`, management-http.ts:34).
    pub retry_on_status: Option<bool>,
    /// Overall time budget shared by all attempts (`timeoutMs`,
    /// management-http.ts:35). A new deadline is computed per attempt from the
    /// remaining budget.
    pub timeout: Option<Duration>,
    /// Per-attempt timeout (`attemptTimeoutMs`, management-http.ts:16-17 @
    /// df018b602, #8198): a NEW timeout is created for every attempt, so a
    /// hung connection aborts only that attempt and the retry loop gets a
    /// fresh chance — unlike `timeout`, exhausting it is NOT terminal.
    pub attempt_timeout: Option<Duration>,
}

impl FetchRetryOptions {
    fn effective_max_retries(&self) -> usize {
        self.max_retries.unwrap_or(2)
    }

    fn effective_retry_on_status(&self) -> bool {
        self.retry_on_status.unwrap_or(true)
    }
}

/// Check whether an HTTP status code is retryable.
fn is_retryable_status(status: u16) -> bool {
    RETRYABLE_STATUS_CODES.contains(&status)
}

/// Fetch a management HTTP resource with a bounded immediate retry
/// (`fetchWithRetry`, management-http.ts:25-67).
///
/// This is a transport-level helper for idempotent management requests
/// (version checks, catalogs, and downloads). It must **not** be used for
/// agent/model operations: those can fail after the HTTP request starts and
/// are retried by their semantic caller instead.
///
/// `build` is called for each attempt to produce a fresh `RequestBuilder`
/// (the body may be consumed by a prior attempt). The caller-provided
/// `cancel_token` is terminal — cancellation is never retried.
pub async fn fetch_with_retry<F, Fut>(
    build: F,
    cancel_token: Option<&CancellationToken>,
    options: &FetchRetryOptions,
) -> Result<reqwest::Response, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = RequestBuilder>,
{
    let max_retries = options.effective_max_retries();
    let retry_on_status = options.effective_retry_on_status();

    // Overall deadline: when set, every attempt sees the *remaining* time as
    // its per-request timeout (AbortSignal.timeout is shared across attempts;
    // management-http.ts:36-42).
    let deadline = options.timeout.map(|t| tokio::time::Instant::now() + t);

    for attempt in 0..=max_retries {
        // Caller cancellation is terminal — check before every attempt
        // (signal.throwIfAborted, management-http.ts:45).
        if let Some(token) = cancel_token {
            if token.is_cancelled() {
                return Err("request cancelled".to_string());
            }
        }

        // Compute the per-attempt timeout: the binding constraint of the
        // remaining overall budget (if any) and `attempt_timeout` (which is
        // per-attempt and non-terminal, df018b602).
        let budget_remaining =
            deadline.map(|end| end.saturating_duration_since(tokio::time::Instant::now()));
        let attempt_timeout = match (budget_remaining, options.attempt_timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let mut request = build().await;
        if let Some(timeout) = attempt_timeout {
            request = request.timeout(timeout);
        }

        let send_result = send_with_cancellation(request, cancel_token).await;

        match send_result {
            Ok(response) => {
                let should_retry = retry_on_status
                    && is_retryable_status(response.status().as_u16())
                    && attempt < max_retries;
                if !should_retry {
                    return Ok(response);
                }
                // Retryable status: fall through to the next attempt
                // (upstream cancels the response body before retrying;
                // reqwest drops the connection when the response is dropped).
            }
            Err(error) => {
                // Transport failure: caller cancellation or budget exhaustion
                // is terminal; otherwise retry (management-http.ts:57-66).
                // An `attempt_timeout` expiry is NOT terminal — it aborts
                // only the current attempt (df018b602).
                let is_cancellation = cancel_token.is_some_and(|t| t.is_cancelled())
                    || deadline.is_some_and(|end| tokio::time::Instant::now() >= end);
                if is_cancellation || attempt >= max_retries {
                    return Err(error);
                }
            }
        }
    }

    // Unreachable: the loop always returns on the last attempt.
    Err("retry loop exhausted".to_string())
}

/// Send a request, racing against caller cancellation
/// (`AbortSignal.any([parentSignal, timeoutSignal])`, management-http.ts:38-42).
async fn send_with_cancellation(
    request: RequestBuilder,
    cancel_token: Option<&CancellationToken>,
) -> Result<reqwest::Response, String> {
    let send = request.send();
    match cancel_token {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => Err("request cancelled".to_string()),
                response = send => response.map_err(|e| e.to_string()),
            }
        }
        None => send.await.map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/management-http.test.ts` — the
    //! upstream `vi.spyOn(globalThis, "fetch")` becomes a scripted loopback
    //! HTTP server (no real network).

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    // ----- scripted loopback server (shared with remote-catalog-provider tests) -----

    #[derive(Clone)]
    struct ScriptedResponse {
        status: u16,
        body: String,
        delay: Duration,
    }

    impl ScriptedResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_owned(),
                delay: Duration::ZERO,
            }
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }
    }

    struct MockServer {
        url: String,
        call_count: Arc<AtomicUsize>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl MockServer {
        async fn start(responses: Vec<ScriptedResponse>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let responses: Arc<tokio::sync::Mutex<Vec<ScriptedResponse>>> =
                Arc::new(tokio::sync::Mutex::new(responses));
            let call_count = Arc::new(AtomicUsize::new(0));
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let handler_count = call_count.clone();
            tokio::spawn(async move {
                let shutdown = async move {
                    let _ = shutdown_rx.await;
                };
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        () = &mut shutdown => break,
                        accepted = listener.accept() => {
                            let (socket, _) = accepted.expect("accept");
                            let responses = responses.clone();
                            let count = handler_count.clone();
                            tokio::spawn(async move {
                                handle_connection(socket, responses, count).await;
                            });
                        }
                    }
                }
            });
            Self {
                url: format!("http://{addr}"),
                call_count,
                shutdown: Some(shutdown_tx),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    async fn handle_connection(
        mut socket: tokio::net::TcpStream,
        responses: Arc<tokio::sync::Mutex<Vec<ScriptedResponse>>>,
        call_count: Arc<AtomicUsize>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        call_count.fetch_add(1, Ordering::SeqCst);
        let response = {
            let mut queue = responses.lock().await;
            if queue.is_empty() {
                ScriptedResponse::json(500, "unexpected")
            } else {
                queue.remove(0)
            }
        };
        if !response.delay.is_zero() {
            tokio::time::sleep(response.delay).await;
        }
        let reason = match response.status {
            200 => "OK",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };
        let out = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            response.status,
            reason,
            response.body.len(),
            response.body
        );
        let _ = socket.write_all(out.as_bytes()).await;
        let _ = socket.flush().await;
    }

    // ----- ported test intents -----

    #[tokio::test]
    async fn retries_retryable_status_and_returns_success() {
        // Upstream: "retries transient HTTP responses and returns the
        // successful response" — 503 then 200.
        let server = MockServer::start(vec![
            ScriptedResponse::json(503, "busy"),
            ScriptedResponse::json(200, r#"{"ok":true}"#),
        ])
        .await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions::default(),
        )
        .await
        .expect("response");
        assert!(response.status().is_success());
        assert_eq!(server.calls(), 2);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_status() {
        // A 404 is not in RETRYABLE_STATUS_CODES, so it returns immediately.
        let server = MockServer::start(vec![ScriptedResponse::json(404, "not found")]).await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions::default(),
        )
        .await
        .expect("response");
        assert_eq!(response.status().as_u16(), 404);
        assert_eq!(server.calls(), 1);
    }

    #[tokio::test]
    async fn exhausts_retries_on_persistent_503() {
        // maxRetries defaults to 2: initial + 2 retries = 3 calls, then the
        // last 503 is returned.
        let server = MockServer::start(vec![
            ScriptedResponse::json(503, ""),
            ScriptedResponse::json(503, ""),
            ScriptedResponse::json(503, ""),
        ])
        .await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions::default(),
        )
        .await
        .expect("response");
        assert_eq!(response.status().as_u16(), 503);
        assert_eq!(server.calls(), 3);
    }

    #[tokio::test]
    async fn max_retries_zero_makes_single_attempt() {
        let server = MockServer::start(vec![ScriptedResponse::json(503, "")]).await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions {
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect("response");
        assert_eq!(response.status().as_u16(), 503);
        assert_eq!(server.calls(), 1);
    }

    #[tokio::test]
    async fn shares_timeout_budget_across_attempts() {
        // Upstream: "shares the timeout budget across attempts" — two
        // responses arrive within the budget, confirming the deadline spans
        // both.
        let server = MockServer::start(vec![
            ScriptedResponse::json(503, "").with_delay(Duration::from_millis(50)),
            ScriptedResponse::json(200, r#"{"ok":true}"#),
        ])
        .await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions {
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            },
        )
        .await
        .expect("response");
        assert!(response.status().is_success());
        assert_eq!(server.calls(), 2);
    }

    #[tokio::test]
    async fn caller_cancellation_is_terminal() {
        // Upstream: "does not retry caller cancellation" — a pre-cancelled
        // token means zero requests are sent.
        let server = MockServer::start(vec![ScriptedResponse::json(200, "")]).await;
        let token = CancellationToken::new();
        token.cancel();

        let url = server.url.clone();
        let result = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            Some(&token),
            &FetchRetryOptions::default(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(server.calls(), 0);
    }

    #[test]
    fn retryable_status_codes_table() {
        // Exact table from management-http.ts:3.
        for &code in &[408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(code), "{code} should be retryable");
        }
        for &code in &[200, 301, 400, 401, 403, 404, 499, 505] {
            assert!(!is_retryable_status(code), "{code} should NOT be retryable");
        }
    }

    // V14-13 FR-E R2 (df018b602, #8198): per-attempt timeout aborts only the
    // hung attempt; the loop retries with a fresh timeout.

    #[tokio::test]
    async fn attempt_timeout_retries_a_hung_request() {
        // First response hangs past the 50 ms attempt timeout; the second
        // answers immediately.
        let server = MockServer::start(vec![
            ScriptedResponse::json(200, r#"{"ok":true}"#).with_delay(Duration::from_millis(500)),
            ScriptedResponse::json(200, r#"{"ok":true}"#),
        ])
        .await;

        let url = server.url.clone();
        let response = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions {
                attempt_timeout: Some(Duration::from_millis(50)),
                ..Default::default()
            },
        )
        .await
        .expect("second attempt succeeds");
        assert!(response.status().is_success());
        assert_eq!(server.calls(), 2, "hung attempt aborted and retried");
    }

    #[tokio::test]
    async fn attempt_timeout_exhaustion_is_bounded_by_max_retries() {
        // Every response hangs: the default max_retries (2) bounds the loop
        // to 3 attempts, each with its own fresh timeout — the total wait is
        // attempts × attempt_timeout, not a single unbounded hang.
        let start = tokio::time::Instant::now();
        let server = MockServer::start(vec![
            ScriptedResponse::json(200, "x").with_delay(Duration::from_millis(500)),
            ScriptedResponse::json(200, "x").with_delay(Duration::from_millis(500)),
            ScriptedResponse::json(200, "x").with_delay(Duration::from_millis(500)),
        ])
        .await;

        let url = server.url.clone();
        let result = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions {
                attempt_timeout: Some(Duration::from_millis(40)),
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err(), "all attempts timed out");
        assert_eq!(server.calls(), 3, "initial + 2 retries");
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "per-attempt timeouts, not one long hang: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn overall_budget_still_terminal_across_attempt_timeouts() {
        // With BOTH budgets set, exhausting the overall budget is terminal
        // even though each attempt also has its own timeout.
        let server = MockServer::start(vec![
            ScriptedResponse::json(200, "x").with_delay(Duration::from_millis(300)),
            ScriptedResponse::json(200, "x").with_delay(Duration::from_millis(300)),
        ])
        .await;

        let url = server.url.clone();
        let result = fetch_with_retry(
            move || {
                let url = url.clone();
                Box::pin(async move { reqwest::Client::new().get(url) })
            },
            None,
            &FetchRetryOptions {
                timeout: Some(Duration::from_millis(100)),
                attempt_timeout: Some(Duration::from_millis(80)),
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err());
        assert!(server.calls() <= 2, "budget exhaustion is terminal");
    }
}
