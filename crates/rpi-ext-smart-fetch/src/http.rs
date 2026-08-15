//! wreq client wrapper: TLS/HTTP2 browser fingerprinting, proxy, timeout and
//! transport-error classification (FR-P0-3 ~ FR-P0-6; design §3.1).
//!
//! Upstream rides wreq-js 2.3.1 (napi bindings on the wreq 6.0.0-rc engine
//! line) — the crates.io `wreq` crate is the same fingerprint stack, so the
//! profile semantics are shared by construction. Two wreq-js specifics are
//! reproduced by hand:
//! - `getProfiles()` / `validateBrowserProfile` / `validateOperatingSystem`
//!   (wreq-js.js:938-948): profile acceptance is pinned to the upstream
//!   2.3.1 profile SET (125 names, verified byte-for-byte including order
//!   against the pinned wreq-js 2.3.1 binary's `getProfiles()`) rather
//!   than the newer wreq-util VARIANTS,
//!   so a name upstream rejects is rejected here too, and the
//!   "Available profiles: …" message lists exactly the upstream set;
//! - `onRequestEvent` phase tracking does not exist in the crates.io engine —
//!   phases are marked by pipeline position instead (design §3.1), so the
//!   pre-response timeout phase is reported as `waiting` (declared deviation,
//!   see TE06 task file).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use wreq::Client;
use wreq_util::{Emulation, Platform, Profile};

use crate::types::FINGERPRINT_OS_VALUES;

/// Profile names accepted by the pinned upstream wreq-js 2.3.1
/// (`BrowserProfile` union in dist/wreq-js.d.ts, generated from the Rust
/// engine VARIANTS at build time). wreq-util 3.0.0-rc.14 additionally knows
/// chrome_148/149, firefox_150/151, safari_26.3/26.4, edge_148 and opera_131 —
/// upstream does not, so acceptance is filtered through this set.
pub const UPSTREAM_PROFILES: &[&str] = &[
    // chrome_100 ..= chrome_147 (upstream gap set: no 102/103/111/112/113/115/125)
    "chrome_100",
    "chrome_101",
    "chrome_104",
    "chrome_105",
    "chrome_106",
    "chrome_107",
    "chrome_108",
    "chrome_109",
    "chrome_110",
    "chrome_114",
    "chrome_116",
    "chrome_117",
    "chrome_118",
    "chrome_119",
    "chrome_120",
    "chrome_123",
    "chrome_124",
    "chrome_126",
    "chrome_127",
    "chrome_128",
    "chrome_129",
    "chrome_130",
    "chrome_131",
    "chrome_132",
    "chrome_133",
    "chrome_134",
    "chrome_135",
    "chrome_136",
    "chrome_137",
    "chrome_138",
    "chrome_139",
    "chrome_140",
    "chrome_141",
    "chrome_142",
    "chrome_143",
    "chrome_144",
    "chrome_145",
    "chrome_146",
    "chrome_147",
    // edge_101 ..= edge_147
    "edge_101",
    "edge_122",
    "edge_127",
    "edge_131",
    "edge_134",
    "edge_135",
    "edge_136",
    "edge_137",
    "edge_138",
    "edge_139",
    "edge_140",
    "edge_141",
    "edge_142",
    "edge_143",
    "edge_144",
    "edge_145",
    "edge_146",
    "edge_147",
    // opera_116 ..= opera_130
    "opera_116",
    "opera_117",
    "opera_118",
    "opera_119",
    "opera_120",
    "opera_121",
    "opera_122",
    "opera_123",
    "opera_124",
    "opera_125",
    "opera_126",
    "opera_127",
    "opera_128",
    "opera_129",
    "opera_130",
    // firefox set incl. private/android variants (upstream order)
    "firefox_109",
    "firefox_117",
    "firefox_128",
    "firefox_133",
    "firefox_135",
    "firefox_private_135",
    "firefox_android_135",
    "firefox_136",
    "firefox_private_136",
    "firefox_139",
    "firefox_142",
    "firefox_143",
    "firefox_144",
    "firefox_145",
    "firefox_146",
    "firefox_147",
    "firefox_148",
    "firefox_149",
    // safari set (upstream order: iOS/iPad variants interleaved)
    "safari_ios_17.2",
    "safari_ios_17.4.1",
    "safari_ios_16.5",
    "safari_15.3",
    "safari_15.5",
    "safari_15.6.1",
    "safari_16",
    "safari_16.5",
    "safari_17.0",
    "safari_17.2.1",
    "safari_17.4.1",
    "safari_17.5",
    "safari_17.6",
    "safari_18",
    "safari_ipad_18",
    "safari_18.2",
    "safari_ios_18.1.1",
    "safari_18.3",
    "safari_18.3.1",
    "safari_18.5",
    "safari_26",
    "safari_26.1",
    "safari_26.2",
    "safari_ipad_26",
    "safari_ipad_26.2",
    "safari_ios_26",
    "safari_ios_26.2",
    // okhttp set
    "okhttp_3.9",
    "okhttp_3.11",
    "okhttp_3.13",
    "okhttp_3.14",
    "okhttp_4.9",
    "okhttp_4.10",
    "okhttp_4.12",
    "okhttp_5",
];

/// name → Profile over the upstream set, in engine VARIANTS order (the order
/// `getProfiles().join(", ")` renders upstream).
static PROFILE_INDEX: LazyLock<Vec<(String, Profile)>> = LazyLock::new(|| {
    Profile::VARIANTS
        .iter()
        .filter_map(|profile| {
            let name = serde_json::to_string(profile).ok()?;
            let name = name.trim_matches('"').to_string();
            UPSTREAM_PROFILES
                .contains(&name.as_str())
                .then_some((name, *profile))
        })
        .collect()
});

/// `validateBrowserProfile` (wreq-js.js:938-941). `Ok` carries the resolved
/// profile; `Err` carries the exact upstream RequestError message.
pub fn resolve_profile(browser: &str) -> Result<Profile, String> {
    if browser.trim().is_empty() {
        return Err("Browser profile must not be empty".to_string());
    }
    for (name, profile) in PROFILE_INDEX.iter() {
        if name == browser {
            return Ok(*profile);
        }
    }
    let available = PROFILE_INDEX
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Invalid browser profile: {browser}. Available profiles: {available}"
    ))
}

/// `validateOperatingSystem` (wreq-js.js:943-946). The options list renders in
/// the `SUPPORTED_OSES` order (wreq-js.js:137-142).
pub fn resolve_platform(os: &str) -> Result<Platform, String> {
    if os.trim().is_empty() {
        return Err("Operating system must not be empty".to_string());
    }
    if !FINGERPRINT_OS_VALUES.contains(&os) {
        let available = FINGERPRINT_OS_VALUES.join(", ");
        return Err(format!(
            "Invalid operating system: {os}. Available options: {available}"
        ));
    }
    Ok(match os {
        "windows" => Platform::Windows,
        "macos" => Platform::MacOS,
        "linux" => Platform::Linux,
        "android" => Platform::Android,
        _ => Platform::IOS,
    })
}

/// One request, fully resolved by the pipeline (defaults applied).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub browser: String,
    pub os: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub proxy: Option<String>,
    pub timeout_ms: u64,
}

/// A response whose status/headers are lifted and whose body arrives as
/// either buffered bytes (mock transports, parity fixtures) or the live
/// engine stream (real wreq — the FR-P1-4 download path consumes it without
/// buffering, the text pipeline reads it to end).
#[derive(Debug)]
pub struct HttpResponse {
    pub final_url: String,
    pub status: u16,
    pub status_text: String,
    /// Lower-cased header name → joined values (multi-value joined with ",
    /// " like the fetch `Headers.get` contract).
    pub headers: HashMap<String, String>,
    pub body: ResponseBody,
}

/// Response body source: upstream's `FetchResponseLike` exposes `body`
/// (ReadableStream), `readable()` and `text()` — the pipeline picks per
/// branch (`response.text()` for text, `streamResponseToFile` for downloads).
pub enum ResponseBody {
    /// Fully-buffered bytes (scripted test transports; the upstream parity
    /// generator's `arrayBuffer`-shaped mocks).
    Full(Vec<u8>),
    /// The live wreq response: status/headers already lifted into
    /// [`HttpResponse`], the body streams via `bytes_stream()`. Boxed — the
    /// engine response dwarfs the buffered variant.
    Stream(Box<wreq::Response>),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseBody::Full(bytes) => f.debug_tuple("Full").field(&bytes.len()).finish(),
            ResponseBody::Stream(_) => f.debug_tuple("Stream").finish(),
        }
    }
}

impl ResponseBody {
    /// `await response.text()` — lossy UTF-8 (JS keeps invalid sequences as
    /// U+FFFD replacement chars, `String::from_utf8_lossy` matches).
    pub async fn read_all(self) -> Result<String, wreq::Error> {
        match self {
            ResponseBody::Full(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            ResponseBody::Stream(response) => {
                let bytes = response.bytes().await?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
    }
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(String::as_str)
    }

    /// A buffered response (mock transports) from plain parts.
    pub fn buffered(
        final_url: impl Into<String>,
        status: u16,
        status_text: impl Into<String>,
        headers: HashMap<String, String>,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        HttpResponse {
            final_url: final_url.into(),
            status,
            status_text: status_text.into(),
            headers,
            body: ResponseBody::Full(body.into()),
        }
    }
}

/// Transport failure classified the way upstream's message regexes do
/// (extract.ts:660-684), with the typed wreq predicates as reinforcement
/// (design §3.4).
#[derive(Debug, Clone)]
pub enum TransportFailure {
    Timeout,
    Dns,
    Connect,
    Tls,
    Other { message: String },
}

static RE_TIMEOUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)timed out|timeout|deadline exceeded|abort(?:ed)?").expect("valid regex")
});
static RE_DNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)dns error|failed to lookup address|nodename nor servname provided|name resolution failed")
        .expect("valid regex")
});
static RE_CONNECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)client error \\(connect\\)|connection refused|tcp connect error|connection reset|network unreachable|no route to host")
        .expect("valid regex")
});
static RE_TLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)ssl.*error|tls.*error|bad certificate|certificate.*invalid|unknown.*issuer")
        .expect("valid regex")
});

/// Message-level classification (upstream `isTimeoutError` / `isDnsError` /
/// `isConnectError` / `isTlsError`, extract.ts:660-684). Upstream classifies
/// inside `buildThrownFetchError` from `error.message` alone — the parity
/// harness replays the same messages through this entry so mocked and real
/// transports classify identically.
pub fn classify_message(message: &str) -> Option<TransportFailure> {
    if RE_TIMEOUT.is_match(message) {
        return Some(TransportFailure::Timeout);
    }
    if RE_DNS.is_match(message) {
        return Some(TransportFailure::Dns);
    }
    if RE_CONNECT.is_match(message) {
        return Some(TransportFailure::Connect);
    }
    if RE_TLS.is_match(message) {
        return Some(TransportFailure::Tls);
    }
    None
}

/// Walk the wreq error chain, concatenating the display messages, then apply
/// the upstream classification regexes; typed predicates (`is_timeout`,
/// `is_tls`) reinforce the regex verdicts.
pub fn classify_transport_error(error: &wreq::Error) -> TransportFailure {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }

    if error.is_timeout() || RE_TIMEOUT.is_match(&message) {
        return TransportFailure::Timeout;
    }
    if RE_DNS.is_match(&message) {
        return TransportFailure::Dns;
    }
    if error.is_connect() || RE_CONNECT.is_match(&message) {
        return TransportFailure::Connect;
    }
    if error.is_tls() || RE_TLS.is_match(&message) {
        return TransportFailure::Tls;
    }
    TransportFailure::Other {
        message: error.to_string(),
    }
}

/// Final URL of a failed request attempt, when the engine retained one
/// (upstream: `errorContext.finalUrl` from request events / response.url).
pub fn error_final_url(error: &wreq::Error) -> Option<String> {
    error.uri().map(|uri| uri.to_string())
}

/// Redact userinfo from a proxy URL before it enters an agent-facing error
/// message. The URL failed to parse (that is why it is being reported), so
/// this is string-level: within the authority (between `://` and the first
/// path/query/fragment delimiter) everything up to and including the LAST
/// `@` is userinfo under WHATWG parsing — it collapses to `***@`. URLs
/// without a scheme separator or without userinfo pass through unchanged.
fn redact_proxy_userinfo(proxy: &str) -> String {
    let Some(scheme_end) = proxy.find("://") else {
        return proxy.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = proxy[authority_start..]
        .find(['/', '?', '#'])
        .map(|offset| authority_start + offset)
        .unwrap_or(proxy.len());
    let authority = &proxy[authority_start..authority_end];
    match authority.rfind('@') {
        Some(at) => {
            let mut redacted = String::with_capacity(proxy.len());
            redacted.push_str(&proxy[..authority_start]);
            redacted.push_str("***@");
            redacted.push_str(&proxy[authority_start + at + 1..]);
            redacted
        }
        None => proxy.to_string(),
    }
}

/// Execute one fingerprinted request (FR-P0-3/4/5). The client is built per
/// request — emulation, proxy and timeout all vary per call, mirroring the
/// upstream wreq-js fetch options object.
pub async fn fetch(request: &HttpRequest) -> Result<HttpResponse, FetchFailure> {
    let profile = resolve_profile(&request.browser).map_err(FetchFailure::InvalidInput)?;
    let platform = resolve_platform(&request.os).map_err(FetchFailure::InvalidInput)?;

    let emulation = Emulation::builder()
        .profile(profile)
        .platform(platform)
        .build();

    let mut builder = Client::builder()
        .emulation(emulation)
        // upstream fetch option `redirect: "follow"` (extract.ts:1179) — the
        // engine default does NOT follow under emulation, so pin the policy
        // explicitly (reqwest-style default ceiling of 10).
        .redirect(wreq::redirect::Policy::limited(10))
        .timeout(Duration::from_millis(request.timeout_ms));
    if let Some(proxy) = &request.proxy {
        let proxy = wreq::Proxy::all(proxy).map_err(|error| {
            // rpi-only error face (upstream passes opts.proxy straight to the
            // engine and never echoes it) — redact userinfo so credentials
            // in the URL cannot leak into the agent-facing message.
            FetchFailure::InvalidInput(format!(
                "Invalid proxy URL: {} ({error})",
                redact_proxy_userinfo(proxy)
            ))
        })?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|error| FetchFailure::InvalidInput(format!("Failed to build client: {error}")))?;

    let mut request_builder = client.get(&request.url);
    for (name, value) in &request.headers {
        request_builder = request_builder.header(name.as_str(), value.as_str());
    }

    let response = request_builder
        .send()
        .await
        .map_err(|error| FetchFailure::Transport {
            failure: classify_transport_error(&error),
            final_url: error_final_url(&error),
        })?;

    let status = response.status();
    let final_url = response.uri().to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    for (name, value) in response.headers().iter() {
        let key = name.as_str().to_lowercase();
        let Ok(value) = value.to_str() else { continue };
        headers
            .entry(key)
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }

    Ok(HttpResponse {
        final_url,
        status: status.as_u16(),
        // JS `response.statusText`: the canonical reason phrase (wreq-js
        // exposes the engine's, which is the canonical set).
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        headers,
        // Body stays live: the pipeline decides per branch whether to read it
        // to end (text) or stream it to disk (FR-P1-4 download).
        body: ResponseBody::Stream(Box::new(response)),
    })
}

/// Request-shape failures (profile/proxy/client validation) vs transport
/// failures; both map into the FetchError model by the pipeline.
#[derive(Debug, Clone)]
pub enum FetchFailure {
    /// Message is the ready-made wreq-js RequestError text — wrapped into
    /// `network_error`/`connecting` by the pipeline like upstream does for a
    /// thrown RequestError.
    InvalidInput(String),
    Transport {
        failure: TransportFailure,
        final_url: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_profile_set_pins_the_engine_variants() {
        // The pinned default must resolve, and the newest wreq-util-only
        // profiles upstream does not know must be rejected with the exact
        // upstream message head.
        assert!(resolve_profile("chrome_145").is_ok());
        let err = resolve_profile("chrome_148").expect_err("not in upstream set");
        assert!(
            err.starts_with("Invalid browser profile: chrome_148. Available profiles: chrome_100"),
            "got: {err}"
        );
        let list = err.split("Available profiles: ").nth(1).expect("list");
        assert!(list.contains("chrome_145"));
        // The engine-only variants (chrome_148/149, firefox_150/151,
        // safari_26.3/26.4, edge_148, opera_131) must NOT be offered.
        for leaked in [
            "chrome_148",
            "chrome_149",
            "firefox_150",
            "firefox_151",
            "safari_26.3",
            "safari_26.4",
            "edge_148",
            "opera_131",
        ] {
            assert!(
                list.split(", ").all(|name| name != leaked),
                "engine-only variant leaked: {leaked}"
            );
        }
    }

    #[test]
    fn empty_profile_and_os_rejections_match_upstream() {
        assert_eq!(
            resolve_profile("  ").expect_err("empty rejected"),
            "Browser profile must not be empty"
        );
        assert_eq!(
            resolve_platform(""),
            Err("Operating system must not be empty".to_string())
        );
        assert_eq!(
            resolve_platform("Windows"),
            Err("Invalid operating system: Windows. Available options: windows, macos, linux, android, ios".to_string())
        );
        assert!(resolve_platform("ios").is_ok());
    }

    #[test]
    fn redact_proxy_userinfo_strips_credentials() {
        // credentials in the authority collapse to ***@ (WHATWG last-@)
        assert_eq!(
            redact_proxy_userinfo("http://user:secret@proxy.example:8080"),
            "http://***@proxy.example:8080"
        );
        // an unencoded @ in the password still redacts up to the last @
        assert_eq!(
            redact_proxy_userinfo("http://user:p@ss@proxy.example:1080"),
            "http://***@proxy.example:1080"
        );
        // path/query after the authority survive
        assert_eq!(
            redact_proxy_userinfo("socks5h://u:p@h:1/a?b=c"),
            "socks5h://***@h:1/a?b=c"
        );
        // no userinfo / no scheme separator pass through unchanged
        assert_eq!(
            redact_proxy_userinfo("socks5://proxy.local:1080"),
            "socks5://proxy.local:1080"
        );
        assert_eq!(redact_proxy_userinfo("not a url"), "not a url");
    }

    #[test]
    fn profile_index_is_complete_and_ordered() {
        assert_eq!(PROFILE_INDEX.len(), UPSTREAM_PROFILES.len());
        // The absolute count pins the documented set (upstream wreq-js 2.3.1
        // `getProfiles()` returns exactly 125 names) so a doc/code drift
        // fails here instead of surfacing in review.
        assert_eq!(PROFILE_INDEX.len(), 125);
        // VARIANTS order puts chrome first — the join order upstream renders.
        assert_eq!(PROFILE_INDEX[0].0, "chrome_100");
    }
}
