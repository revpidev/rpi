//! Adapter client construction with explicit env-proxy takeover
//! (V14-08 FR-B; `utils/http_proxy` module docs explain the semantics and
//! the reqwest-vs-Node-agents transport note).
//!
//! Every provider-facing `reqwest::Client` in the adapter layer is built
//! through [`adapter_client_builder`]: reqwest's built-in env proxying is
//! disabled first (`.no_proxy()` — its suffix-matching `NO_PROXY` semantics
//! are exactly the pre-#8737 bug), then a proxy resolved for the target URL
//! with the upstream `shouldProxyHostname` semantics is injected, gated on
//! the target scheme (upstream picks `HttpProxyAgent` vs `HttpsProxyAgent`
//! by target protocol).

use crate::types::ProviderEnv;
use crate::utils::http_proxy::resolve_http_proxy_for_target;

/// Build a client builder with the env proxy decision taken over for
/// `target_url`. `Ok(builder)` always produces a usable builder — a direct
/// connection when no proxy resolves; `Err` mirrors the upstream
/// `resolveHttpProxyUrlForTarget` failures (invalid / SOCKS-PAC proxy URL).
pub fn adapter_client_builder(
    env: Option<&ProviderEnv>,
    target_url: &str,
) -> Result<reqwest::ClientBuilder, String> {
    // Disable reqwest's env/system proxy first: `no_proxy()` clears any
    // proxies and turns off auto detection; the explicit proxy below (if
    // any) is added afterwards and wins.
    let mut builder = reqwest::Client::builder().no_proxy();
    let target_is_http = url::Url::parse(target_url)
        .map(|parsed| parsed.scheme() == "http")
        .unwrap_or(false);
    if let Some(proxy_url) = resolve_http_proxy_for_target(target_url, env)? {
        let proxy = if target_is_http {
            reqwest::Proxy::http(proxy_url)
        } else {
            reqwest::Proxy::https(proxy_url)
        }
        .map_err(|error| format!("Invalid proxy URL: {error}"))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No env → no proxy entry → plain builder (direct); scheme-gated
    /// injection for a resolved proxy.
    #[test]
    fn builder_direct_without_env() {
        let env: ProviderEnv = [(
            "HTTPS_PROXY".to_owned(),
            "http://proxy.example:8080".to_owned(),
        )]
        .into_iter()
        .collect();
        // NO_PROXY exempts the target → builder builds fine (direct).
        let mut env_with_exempt = env.clone();
        env_with_exempt.insert("NO_PROXY".to_owned(), "api.example.com".to_owned());
        let builder = adapter_client_builder(Some(&env_with_exempt), "https://api.example.com")
            .expect("builder");
        builder.build().expect("direct client builds");

        // A SOCKS proxy for a covered target surfaces the upstream error.
        let socks_env: ProviderEnv = [(
            "HTTPS_PROXY".to_owned(),
            "socks5://proxy.example:1080".to_owned(),
        )]
        .into_iter()
        .collect();
        let error = adapter_client_builder(Some(&socks_env), "https://api.example.com")
            .expect_err("socks rejected");
        assert!(error.starts_with(crate::utils::http_proxy::UNSUPPORTED_PROXY_PROTOCOL_MESSAGE));
    }
}
