//! Port of `packages/ai/src/auth/oauth/oauth-page.ts` @ pi 0.82.1 (2efa728),
//! plus the localhost callback server half of `auth/oauth/anthropic.ts`
//! (`startCallbackServer`, there based on `node:http`; here `axum` per
//! coding-standards appendix A).
//!
//! Layering: this module renders the HTML verbatim and provides a generic
//! state-validating one-shot callback server; the provider-specific page copy
//! (upstream hardcodes Anthropic wording at the anthropic.ts call site) is
//! passed in as [`CallbackPageCopy`].
//!
//! Intentional differences: the upstream 500 catch-all has no counterpart —
//! the handler performs no fallible work beyond rendering; the bind host is
//! read from `RPI_OAUTH_CALLBACK_HOST` (ADR-0001 §2 `RPI_` prefix; upstream
//! reads `PI_OAUTH_CALLBACK_HOST` via `getProviderEnvValue`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::Html;
use axum::routing::{get, Router};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::super::resolve::{ModelsError, ModelsErrorCode};

/// `CALLBACK_PORT` (upstream anthropic.ts).
pub const CALLBACK_PORT: u16 = 53692;
/// `CALLBACK_PATH`.
pub const CALLBACK_PATH: &str = "/callback";
/// Default bind host; upstream: `getProviderEnvValue("PI_OAUTH_CALLBACK_HOST") || "127.0.0.1"`.
const DEFAULT_CALLBACK_HOST: &str = "127.0.0.1";
/// Env override for the bind host (ADR-0001 §2 `RPI_` prefix; corresponds to
/// upstream `PI_OAUTH_CALLBACK_HOST`).
const CALLBACK_HOST_ENV: &str = "RPI_OAUTH_CALLBACK_HOST";

/// Upstream: `getProviderEnvValue("PI_OAUTH_CALLBACK_HOST") || "127.0.0.1"`
/// (here with the `RPI_`-prefixed variable). Shared with the anthropic flow,
/// which binds the server itself for its test port seam.
pub(crate) fn default_callback_host() -> String {
    std::env::var(CALLBACK_HOST_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CALLBACK_HOST.to_owned())
}

/// `LOGO_SVG` (verbatim).
const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><path fill="#fff" fill-rule="evenodd" d="M165.29 165.29 H517.36 V400 H400 V517.36 H282.65 V634.72 H165.29 Z M282.65 282.65 V400 H400 V282.65 Z"/><path fill="#fff" d="M517.36 400 H634.72 V634.72 H517.36 Z"/></svg>"##;

/// `escapeHtml` — the same five entity replacements, in the same order.
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `renderPage` — verbatim markup; `__*__` tokens are substituted instead of
/// template interpolation so the CSS braces survive untouched.
const PAGE_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>__TITLE__</title>
  <style>
    :root {
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }
    * { box-sizing: border-box; }
    html { color-scheme: dark; }
    body {
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }
    main {
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }
    .logo {
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }
    h1 {
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }
    p {
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }
    .details {
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }
  </style>
</head>
<body>
  <main>
    <div class="logo">__LOGO_SVG__</div>
    <h1>__HEADING__</h1>
    <p>__MESSAGE__</p>
    __DETAILS__
  </main>
</body>
</html>"##;

/// `renderPage(options)`.
fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let details_block = match details {
        Some(details) => format!("<div class=\"details\">{}</div>", escape_html(details)),
        None => String::new(),
    };
    PAGE_TEMPLATE
        .replace("__TITLE__", &escape_html(title))
        .replace("__HEADING__", &escape_html(heading))
        .replace("__MESSAGE__", &escape_html(message))
        .replace("__DETAILS__", &details_block)
        .replace("__LOGO_SVG__", LOGO_SVG)
}

/// `oauthSuccessHtml`.
pub fn oauth_success_html(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

/// `oauthErrorHtml`.
pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}

/// Provider-specific page copy for the callback server (upstream hardcodes
/// Anthropic wording inside anthropic.ts's request handler).
#[derive(Debug, Clone)]
pub struct CallbackPageCopy {
    /// 200 page, e.g. "Anthropic authentication completed. You can close this window."
    pub success_message: String,
    /// 400 page for an `error` query param, e.g. "Anthropic authentication did not complete."
    pub failure_message: String,
}

/// Settled callback result (`{ code, state }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCode {
    pub code: String,
    pub state: String,
}

struct CallbackState {
    expected_state: String,
    copy: CallbackPageCopy,
    /// Settle-once channel: outer `None` = still waiting; `Some(None)` =
    /// cancelled (`cancelWait`); `Some(Some(_))` = code received.
    settle: watch::Sender<Option<Option<CallbackCode>>>,
    settled: AtomicBool,
}

impl CallbackState {
    fn settle(&self, value: Option<CallbackCode>) {
        if self
            .settled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.settle.send_replace(Some(value));
        }
    }
}

/// First occurrence of each query parameter (mirrors `URLSearchParams.get`).
fn query_params(uri: &Uri) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    params
}

async fn handle_callback(
    State(state): State<Arc<CallbackState>>,
    uri: Uri,
) -> (StatusCode, Html<String>) {
    let params = query_params(&uri);
    let code = params.get("code").filter(|value| !value.is_empty());
    let state_param = params.get("state").filter(|value| !value.is_empty());
    let error = params.get("error").filter(|value| !value.is_empty());

    if let Some(error) = error {
        let details = format!("Error: {error}");
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html(
                &state.copy.failure_message,
                Some(&details),
            )),
        );
    }

    let (Some(code), Some(state_value)) = (code, state_param) else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html("Missing code or state parameter.", None)),
        );
    };

    if *state_value != state.expected_state {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html("State mismatch.", None)),
        );
    }

    state.settle(Some(CallbackCode {
        code: code.clone(),
        state: state_value.clone(),
    }));
    (
        StatusCode::OK,
        Html(oauth_success_html(&state.copy.success_message)),
    )
}

async fn handle_fallback() -> (StatusCode, Html<String>) {
    (
        StatusCode::NOT_FOUND,
        Html(oauth_error_html("Callback route not found.", None)),
    )
}

/// `startCallbackServer` — one-shot localhost callback server. Bind errors
/// (e.g. port in use) propagate.
pub struct OAuthCallbackServer {
    state: Arc<CallbackState>,
    local_addr: SocketAddr,
    shutdown: CancellationToken,
    serve: Option<tokio::task::JoinHandle<()>>,
}

impl OAuthCallbackServer {
    /// Bind `CALLBACK_PORT` on `RPI_OAUTH_CALLBACK_HOST` (default 127.0.0.1).
    pub async fn start(
        expected_state: impl Into<String>,
        copy: CallbackPageCopy,
    ) -> Result<Self, ModelsError> {
        Self::start_on(
            expected_state,
            copy,
            &default_callback_host(),
            CALLBACK_PORT,
        )
        .await
    }

    /// Bind an explicit address (tests use port 0 for an ephemeral port).
    pub async fn start_on(
        expected_state: impl Into<String>,
        copy: CallbackPageCopy,
        host: &str,
        port: u16,
    ) -> Result<Self, ModelsError> {
        let listener = tokio::net::TcpListener::bind((host, port))
            .await
            .map_err(|error| {
                ModelsError::new(
                    ModelsErrorCode::Oauth,
                    format!("OAuth callback server failed to bind {host}:{port}: {error}"),
                )
            })?;
        let local_addr = listener.local_addr().map_err(|error| {
            ModelsError::new(
                ModelsErrorCode::Oauth,
                format!("OAuth callback server failed to read its address: {error}"),
            )
        })?;

        let (settle, _) = watch::channel(None);
        let state = Arc::new(CallbackState {
            expected_state: expected_state.into(),
            copy,
            settle,
            settled: AtomicBool::new(false),
        });

        let app = Router::new()
            .route(CALLBACK_PATH, get(handle_callback))
            .fallback(handle_fallback)
            .with_state(state.clone());

        let shutdown = CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        let serve = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(serve_shutdown.cancelled_owned())
                .await;
            if let Err(error) = result {
                tracing::warn!(%error, "OAuth callback server terminated with an error");
            }
        });

        Ok(Self {
            state,
            local_addr,
            shutdown,
            serve: Some(serve),
        })
    }

    /// Actually bound address (relevant when started on port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// `waitForCode()` — resolves with the callback code/state, or `None`
    /// when [`cancel_wait`](Self::cancel_wait) settles it first.
    pub async fn wait_for_code(&self) -> Option<CallbackCode> {
        let mut rx = self.state.settle.subscribe();
        if let Some(value) = rx.borrow().clone() {
            return value;
        }
        loop {
            if rx.changed().await.is_err() {
                return None; // sender dropped (server closed)
            }
            if let Some(value) = rx.borrow_and_update().clone() {
                return value;
            }
        }
    }

    /// `cancelWait()` — settle any `wait_for_code` with `None`.
    pub fn cancel_wait(&self) {
        self.state.settle(None);
    }

    /// `server.close()` — stop accepting connections and shut down.
    pub async fn close(mut self) {
        self.shutdown.cancel();
        if let Some(serve) = self.serve.take() {
            let _ = serve.await;
        }
    }
}

impl Drop for OAuthCallbackServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_copy() -> CallbackPageCopy {
        CallbackPageCopy {
            success_message: "Anthropic authentication completed. You can close this window."
                .to_owned(),
            failure_message: "Anthropic authentication did not complete.".to_owned(),
        }
    }

    async fn start_test_server(expected_state: &str) -> (OAuthCallbackServer, String) {
        let server = OAuthCallbackServer::start_on(expected_state, test_copy(), "127.0.0.1", 0)
            .await
            .expect("bind");
        let base = format!("http://{}", server.local_addr());
        (server, base)
    }

    /// `escapeHtml`: all five entities, `&` replaced first.
    #[test]
    fn escape_html_replaces_five_entities() {
        assert_eq!(escape_html(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&#39;");
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
    }

    #[test]
    fn success_page_renders_verbatim_structure() {
        let html = oauth_success_html("done <b>");
        assert!(html.starts_with("<!doctype html>\n<html lang=\"en\">"));
        assert!(html.ends_with("</html>"));
        assert!(html.contains("<title>Authentication successful</title>"));
        assert!(html.contains("<h1>Authentication successful</h1>"));
        assert!(html.contains("<p>done &lt;b&gt;</p>"));
        assert!(html.contains(LOGO_SVG));
        // No details → the interpolation line keeps its 4-space indent.
        assert!(html.contains("\n    \n  </main>"));
    }

    #[test]
    fn error_page_renders_details_block() {
        let html = oauth_error_html("nope", Some("Error: access_denied"));
        assert!(html.contains("<title>Authentication failed</title>"));
        assert!(html.contains("<div class=\"details\">Error: access_denied</div>"));
    }

    #[tokio::test]
    async fn callback_success_settles_wait_for_code() {
        let (server, base) = start_test_server("expected-state").await;
        let response = reqwest::get(format!(
            "{base}{CALLBACK_PATH}?code=the-code&state=expected-state"
        ))
        .await
        .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("body");
        assert!(body.contains("Anthropic authentication completed. You can close this window."));
        assert_eq!(
            server.wait_for_code().await,
            Some(CallbackCode {
                code: "the-code".to_owned(),
                state: "expected-state".to_owned(),
            })
        );
        server.close().await;
    }

    #[tokio::test]
    async fn callback_error_param_returns_400_with_details() {
        let (server, base) = start_test_server("expected-state").await;
        let response = reqwest::get(format!("{base}{CALLBACK_PATH}?error=access_denied"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.text().await.expect("body");
        assert!(body.contains("Anthropic authentication did not complete."));
        assert!(body.contains("Error: access_denied"));
        server.close().await;
    }

    #[tokio::test]
    async fn callback_missing_code_or_state_returns_400() {
        let (server, base) = start_test_server("expected-state").await;
        let response = reqwest::get(format!("{base}{CALLBACK_PATH}?code=only-code"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response
            .text()
            .await
            .expect("body")
            .contains("Missing code or state parameter."));
        server.close().await;
    }

    #[tokio::test]
    async fn callback_state_mismatch_returns_400_and_does_not_settle() {
        let (server, base) = start_test_server("expected-state").await;
        let response = reqwest::get(format!("{base}{CALLBACK_PATH}?code=c&state=wrong"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response
            .text()
            .await
            .expect("body")
            .contains("State mismatch."));
        server.cancel_wait();
        assert_eq!(server.wait_for_code().await, None);
        server.close().await;
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let (server, base) = start_test_server("expected-state").await;
        let response = reqwest::get(format!("{base}/nope"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response
            .text()
            .await
            .expect("body")
            .contains("Callback route not found."));
        server.close().await;
    }

    #[tokio::test]
    async fn cancel_wait_settles_none() {
        let (server, _base) = start_test_server("expected-state").await;
        server.cancel_wait();
        assert_eq!(server.wait_for_code().await, None);
        server.close().await;
    }
}
