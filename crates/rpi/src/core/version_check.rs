//! Port of `packages/coding-agent/src/utils/version-check.ts` @ pi 0.82.1
//! (2efa728) — the "latest release" probe behind `rpi update --self`.
//!
//! Intentional differences:
//! - `PI_OFFLINE` → `RPI_OFFLINE` (ADR-0001); the offline check reuses the
//!   package-manager interpretation
//!   ([`crate::core::package_manager::is_offline_mode_enabled`]).
//! - HTTP goes through the injectable [`LatestVersionTransport`] trait
//!   (tests use a scripted transport; no real network in tests). The
//!   default transport is reqwest + rustls (D-005 precedent).
//! - `getPiUserAgent` carries the rpi naming and a `rust` runtime marker
//!   (ADR-0001 / D-038 precedent).
//! - T14-W6a (ADR-0002 §8): the endpoint URL is configurable via the
//!   `RPI_VERSION_CHECK_URL` env var or the `versionCheckUrl` setting
//!   ([`version_check_endpoint`], rpi-specific — upstream hardcodes
//!   [`LATEST_VERSION_URL`]); the literal `off` disables the endpoint.

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

/// `LATEST_VERSION_URL` (version-check.ts:4). Default endpoint; override via
/// [`version_check_endpoint`] (ADR-0002 §8).
pub const LATEST_VERSION_URL: &str = "https://revpi.dev/api/latest-version";

/// `DEFAULT_VERSION_CHECK_TIMEOUT_MS` (version-check.ts:5).
pub const DEFAULT_VERSION_CHECK_TIMEOUT: Duration = Duration::from_millis(10_000);

/// `LatestPiRelease` (version-check.ts:7-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRpiRelease {
    pub version: String,
    pub package_name: Option<String>,
    pub note: Option<String>,
}

/// `getPiUserAgent` (utils/pi-user-agent.ts): `rpi/{version} ({platform};
/// rust; {arch})`.
pub fn rpi_user_agent(version: &str) -> String {
    format!(
        "rpi/{version} ({}; rust; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// `comparePackageVersions` (version-check.ts:13-20): strict semver
/// comparison; `None` when either side is not a valid version. The `semver`
/// crate does not accept node-semver's leading `v`/`=` prefixes (same
/// boundary as the D-040 range translation layer), so they are stripped
/// before parsing (T14 review L-2: without the strip, a `v1.0.0` release
/// string would fall back to string inequality and misreport a same-version
/// release as newer).
pub fn compare_package_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left = semver::Version::parse(strip_version_prefix(left.trim())).ok()?;
    let right = semver::Version::parse(strip_version_prefix(right.trim())).ok()?;
    Some(left.cmp(&right))
}

/// Strip one leading `v`/`V`/`=` from a version string (node-semver's
/// `semver.valid` accepts them).
fn strip_version_prefix(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .or_else(|| version.strip_prefix('='))
        .unwrap_or(version)
}

/// `isNewerPackageVersion` (version-check.ts:22-28): unparseable versions
/// fall back to string inequality.
pub fn is_newer_package_version(candidate: &str, current: &str) -> bool {
    match compare_package_versions(candidate, current) {
        Some(ordering) => ordering == std::cmp::Ordering::Greater,
        None => candidate.trim() != current.trim(),
    }
}

/// Injectable HTTP GET (upstream `fetch`). `Ok(None)` maps to upstream's
/// `!response.ok` → `undefined`; transport failures are `Err`.
pub trait LatestVersionTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;
}

/// Production transport: reqwest with rustls (no proxy code of its own —
/// reqwest's env proxy support covers `HTTP_PROXY`/`HTTPS_PROXY`).
pub struct ReqwestLatestVersionTransport;

impl LatestVersionTransport for ReqwestLatestVersionTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| e.to_string())?;
            let response = client
                .get(url)
                .header(reqwest::header::USER_AGENT, user_agent)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Ok(None);
            }
            response.text().await.map(Some).map_err(|e| e.to_string())
        })
    }
}

/// `getLatestPiRelease` (version-check.ts:30-61) with the default URL and
/// timeout; `RPI_OFFLINE` short-circuits to `None`.
pub async fn get_latest_rpi_release(
    current_version: &str,
    transport: &dyn LatestVersionTransport,
) -> Result<Option<LatestRpiRelease>, String> {
    get_latest_rpi_release_with(
        current_version,
        transport,
        LATEST_VERSION_URL,
        DEFAULT_VERSION_CHECK_TIMEOUT,
        crate::core::package_manager::is_offline_mode_enabled(),
    )
    .await
}

/// [`get_latest_rpi_release`] with explicit URL / timeout / offline flag
/// (test seam; also the W6a endpoint-override call site).
pub async fn get_latest_rpi_release_with(
    current_version: &str,
    transport: &dyn LatestVersionTransport,
    url: &str,
    timeout: Duration,
    offline: bool,
) -> Result<Option<LatestRpiRelease>, String> {
    if offline {
        return Ok(None);
    }
    let Some(body) = transport
        .get(url, &rpi_user_agent(current_version), timeout)
        .await?
    else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let trimmed_non_empty = |key: &str| {
        parsed
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let Some(version) = trimmed_non_empty("version") else {
        return Ok(None);
    };
    Ok(Some(LatestRpiRelease {
        version,
        package_name: trimmed_non_empty("packageName"),
        note: trimmed_non_empty("note"),
    }))
}

// ===== T14-W6a: configurable endpoint (ADR-0002 §8) =====

/// Resolve the version-check endpoint: `RPI_VERSION_CHECK_URL` env >
/// `versionCheckUrl` setting > [`LATEST_VERSION_URL`]; the literal `off`
/// disables the check (`None` — callers must not probe). Rpi-specific
/// (ADR-0002 §8); upstream hardcodes the URL.
pub fn version_check_endpoint(settings_url: Option<&str>) -> Option<String> {
    crate::config::endpoint_from_env(
        crate::config::ENV_VERSION_CHECK_URL,
        settings_url,
        LATEST_VERSION_URL,
    )
}

/// The startup-check gates of `checkForNewPiVersion` +
/// `getLatestPiRelease` (version-check.ts:34, 71) composed with the
/// endpoint: `RPI_SKIP_VERSION_CHECK` or offline or a disabled endpoint all
/// yield `None` (no probe). Pure — the caller reads the env flags.
pub fn startup_probe_url(skip: bool, offline: bool, endpoint: Option<String>) -> Option<String> {
    if skip || offline {
        return None;
    }
    endpoint
}

/// [`startup_probe_url`] reading the process env gates
/// (`RPI_SKIP_VERSION_CHECK` / `RPI_OFFLINE`).
pub fn startup_version_check_url(settings_url: Option<&str>) -> Option<String> {
    startup_probe_url(
        crate::core::environment::skip_version_check(),
        crate::core::environment::is_offline(),
        version_check_endpoint(settings_url),
    )
}

/// `checkForNewPiVersion` (version-check.ts:70-81): `None` unless the probe
/// reports a strictly newer version; every transport/parse failure is
/// swallowed (upstream `try/catch`). `url` is the resolved startup probe
/// URL — `None` disables the check with zero network traffic.
pub async fn check_for_new_rpi_release(
    current_version: &str,
    transport: &dyn LatestVersionTransport,
    url: Option<&str>,
) -> Option<LatestRpiRelease> {
    let url = url?;
    let release = get_latest_rpi_release_with(
        current_version,
        transport,
        url,
        DEFAULT_VERSION_CHECK_TIMEOUT,
        false,
    )
    .await
    .ok()??;
    if is_newer_package_version(&release.version, current_version) {
        Some(release)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! Port of the version-check intent of
    //! `packages/coding-agent/test/version-check.test.ts` (parse/compare
    //! rules and the release-probe response handling), with a scripted
    //! transport instead of fetch stubs.

    use super::*;
    use std::sync::Mutex;

    struct ScriptedTransport {
        calls: Mutex<Vec<(String, String)>>,
        response: Result<Option<String>, String>,
    }

    impl ScriptedTransport {
        fn responds(response: Result<Option<String>, String>) -> Self {
            ScriptedTransport {
                calls: Mutex::new(Vec::new()),
                response,
            }
        }
    }

    impl LatestVersionTransport for ScriptedTransport {
        fn get<'a>(
            &'a self,
            url: &'a str,
            user_agent: &'a str,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((url.to_string(), user_agent.to_string()));
            let response = self.response.clone();
            Box::pin(async move { response })
        }
    }

    #[test]
    fn compare_versions_strict_semver() {
        assert_eq!(
            compare_package_versions("1.2.3", "1.2.3"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_package_versions("1.3.0", "1.2.9"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_package_versions(" 1.2.3 ", "1.2.10"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(compare_package_versions("abc", "1.2.3"), None);
    }

    #[test]
    fn is_newer_falls_back_to_string_inequality() {
        assert!(is_newer_package_version("1.3.0", "1.2.9"));
        assert!(!is_newer_package_version("1.2.3", "1.2.3"));
        assert!(is_newer_package_version("abc", "1.2.3"));
        assert!(!is_newer_package_version(" same ", "same"));
    }

    #[test]
    fn node_semver_prefixes_do_not_misreport_same_version() {
        // T14 review L-2: node-semver accepts leading `v`/`=`, Rust
        // `semver` does not — without the strip, `v1.0.0` vs `1.0.0` would
        // fall back to string inequality and read as a newer release.
        assert_eq!(
            compare_package_versions("v1.0.0", "1.0.0"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_package_versions("=1.0.0", "v1.0.0"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_package_versions("V1.0.1", "1.0.0"),
            Some(std::cmp::Ordering::Greater)
        );
        assert!(!is_newer_package_version("v1.0.0", "1.0.0"));
        // An unprefixable non-version still falls back to inequality.
        assert!(is_newer_package_version("vabc", "1.0.0"));
    }

    #[tokio::test]
    async fn offline_skips_the_fetch() {
        let transport = ScriptedTransport::responds(Ok(Some("{}".to_string())));
        let release = get_latest_rpi_release_with(
            "1.0.0",
            &transport,
            LATEST_VERSION_URL,
            DEFAULT_VERSION_CHECK_TIMEOUT,
            true,
        )
        .await
        .expect("offline result");
        assert_eq!(release, None);
        assert!(transport
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[tokio::test]
    async fn non_ok_response_yields_none() {
        let transport = ScriptedTransport::responds(Ok(None));
        let release = get_latest_rpi_release("1.0.0", &transport)
            .await
            .expect("non-ok result");
        assert_eq!(release, None);
    }

    #[tokio::test]
    async fn parses_version_package_name_and_note() {
        let transport = ScriptedTransport::responds(Ok(Some(
            r#"{"version": " 1.2.3 ", "packageName": "rpi-next", "note": "  hi  "}"#.to_string(),
        )));
        let release = get_latest_rpi_release("1.0.0", &transport)
            .await
            .expect("release")
            .expect("some release");
        assert_eq!(release.version, "1.2.3");
        assert_eq!(release.package_name.as_deref(), Some("rpi-next"));
        assert_eq!(release.note.as_deref(), Some("hi"));

        let calls = transport.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls[0].0, LATEST_VERSION_URL);
        assert_eq!(calls[0].1, rpi_user_agent("1.0.0"));
    }

    #[tokio::test]
    async fn missing_or_empty_version_yields_none() {
        for body in [
            r#"{"packageName": "rpi"}"#,
            r#"{"version": "  "}"#,
            r#"{"version": 3}"#,
        ] {
            let transport = ScriptedTransport::responds(Ok(Some(body.to_string())));
            let release = get_latest_rpi_release("1.0.0", &transport)
                .await
                .expect("release");
            assert_eq!(release, None, "{body}");
        }
    }

    #[tokio::test]
    async fn blank_optional_fields_become_none() {
        let transport = ScriptedTransport::responds(Ok(Some(
            r#"{"version": "1.2.3", "packageName": " ", "note": 42}"#.to_string(),
        )));
        let release = get_latest_rpi_release("1.0.0", &transport)
            .await
            .expect("release")
            .expect("some release");
        assert_eq!(release.package_name, None);
        assert_eq!(release.note, None);
    }

    #[tokio::test]
    async fn transport_and_parse_errors_propagate() {
        let transport = ScriptedTransport::responds(Err("connection refused".to_string()));
        let error = get_latest_rpi_release("1.0.0", &transport)
            .await
            .expect_err("transport error");
        assert_eq!(error, "connection refused");

        let transport = ScriptedTransport::responds(Ok(Some("not json".to_string())));
        assert!(get_latest_rpi_release("1.0.0", &transport).await.is_err());
    }

    // ---- T14-W6a: endpoint configuration + startup check (ADR-0002 §8) ----

    #[test]
    fn startup_probe_url_composes_the_gates() {
        let endpoint = Some("https://revpi.dev/api/latest-version".to_string());
        assert_eq!(
            startup_probe_url(false, false, endpoint.clone()).as_deref(),
            Some("https://revpi.dev/api/latest-version")
        );
        // Skip flag, offline, and a disabled endpoint each suppress the probe.
        assert_eq!(startup_probe_url(true, false, endpoint.clone()), None);
        assert_eq!(startup_probe_url(false, true, endpoint.clone()), None);
        assert_eq!(startup_probe_url(false, false, None), None);
    }

    /// Read-only env use: no test writes `RPI_VERSION_CHECK_URL` (the
    /// env-override logic is covered by the pure
    /// [`crate::config::resolve_endpoint`] tests).
    #[test]
    fn version_check_endpoint_defaults_and_settings_override() {
        assert_eq!(
            crate::config::ENV_VERSION_CHECK_URL,
            "RPI_VERSION_CHECK_URL"
        );
        assert_eq!(
            version_check_endpoint(None).as_deref(),
            Some(LATEST_VERSION_URL)
        );
        assert_eq!(
            version_check_endpoint(Some("https://mirror.test/v")).as_deref(),
            Some("https://mirror.test/v")
        );
        assert_eq!(version_check_endpoint(Some("off")), None);
    }

    #[tokio::test]
    async fn check_for_new_rpi_release_reports_only_newer_versions() {
        // Newer version → Some(release).
        let transport = ScriptedTransport::responds(Ok(Some(
            r#"{"version": "9.9.9", "note": "hi"}"#.to_string(),
        )));
        let release = check_for_new_rpi_release("1.0.0", &transport, Some(LATEST_VERSION_URL))
            .await
            .expect("newer release");
        assert_eq!(release.version, "9.9.9");
        // Same / older / unparseable-fetch → None (upstream try/catch swallows).
        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "1.0.0"}"#.to_string())));
        assert!(
            check_for_new_rpi_release("1.0.0", &transport, Some(LATEST_VERSION_URL))
                .await
                .is_none()
        );
        let transport = ScriptedTransport::responds(Err("boom".to_string()));
        assert!(
            check_for_new_rpi_release("1.0.0", &transport, Some(LATEST_VERSION_URL))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn check_for_new_rpi_release_disabled_endpoint_makes_no_request() {
        // Zero-network anchor: a disabled endpoint never touches the transport.
        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "9.9.9"}"#.to_string())));
        assert!(check_for_new_rpi_release("1.0.0", &transport, None)
            .await
            .is_none());
        assert!(transport
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }
}
