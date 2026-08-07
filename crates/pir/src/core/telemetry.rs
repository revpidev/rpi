//! Port of `packages/coding-agent/src/core/telemetry.ts` @ pi 0.82.1
//! (2efa728) plus the install-telemetry ping `reportInstallTelemetry`
//! (`modes/interactive/interactive-mode.ts:1017-1036`) and its
//! changelog-driven trigger (`getChangelogForDisplay`,
//! interactive-mode.ts:991-1014).
//!
//! Intentional differences (D-046):
//! - `PI_TELEMETRY` → `PIR_TELEMETRY` (ADR-0001); the env layer itself lives
//!   in [`crate::core::environment::telemetry_enabled_override`].
//! - T14-W6a (ADR-0002 §8): the ping endpoint is configurable via the
//!   `PIR_TELEMETRY_URL` env var or the `telemetryUrl` setting
//!   ([`report_install_endpoint`], pir-specific — upstream hardcodes
//!   [`DEFAULT_REPORT_INSTALL_URL`]); the literal `off` disables the ping.
//! - HTTP goes through the injectable [`ReportInstallTransport`] trait
//!   (tests use a scripted transport; no real network in tests).
//! - The trigger's "new changelog entries" condition is approximated by
//!   version inequality until the changelog asset lands (T15): any version
//!   change implies new entries upstream. The fresh-install branch and the
//!   resumed-session skip are exact.

use std::time::Duration;

use futures::future::BoxFuture;

use crate::core::remote_catalog_provider::encode_uri_component;
use crate::core::settings_manager::SettingsManager;
use crate::core::version_check::pir_user_agent;

/// The install-telemetry endpoint (interactive-mode.ts:1028). Default;
/// override via [`report_install_endpoint`] (ADR-0002 §8).
pub const DEFAULT_REPORT_INSTALL_URL: &str = "https://pi.dev/api/report-install";

/// `AbortSignal.timeout(5000)` (interactive-mode.ts:1031).
pub const REPORT_INSTALL_TIMEOUT: Duration = Duration::from_millis(5_000);

/// `isInstallTelemetryEnabled` (telemetry.ts:8-12): a set `PIR_TELEMETRY`
/// (even empty) overrides the `enableInstallTelemetry` setting
/// (`1/true/yes` enable, everything else disables); the setting defaults to
/// **true**.
pub fn is_install_telemetry_enabled(settings: &SettingsManager) -> bool {
    install_telemetry_enabled(
        settings.get_enable_install_telemetry(),
        crate::core::environment::telemetry_enabled_override(),
    )
}

/// The composition of [`is_install_telemetry_enabled`] (telemetry.ts:12):
/// env override wins when set; otherwise the setting decides.
fn install_telemetry_enabled(setting: bool, env_override: Option<bool>) -> bool {
    env_override.unwrap_or(setting)
}

/// Resolve the install-telemetry endpoint (ADR-0002 §8):
/// `PIR_TELEMETRY_URL` env > `telemetryUrl` setting >
/// [`DEFAULT_REPORT_INSTALL_URL`]; the literal `off` disables the ping
/// (`None` — no request is made). Pir-specific; upstream hardcodes the URL.
pub fn report_install_endpoint(settings_url: Option<&str>) -> Option<String> {
    crate::config::endpoint_from_env(
        crate::config::ENV_TELEMETRY_URL,
        settings_url,
        DEFAULT_REPORT_INSTALL_URL,
    )
}

/// Injectable HTTP GET (upstream `fetch`). The response status and body are
/// irrelevant to the ping (interactive-mode.ts:1033-1034 resolves and
/// ignores both), so the transport reports only transport-level failures.
pub trait ReportInstallTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// Production transport: reqwest with rustls (D-005 precedent).
pub struct ReqwestReportInstallTransport;

impl ReportInstallTransport for ReqwestReportInstallTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| e.to_string())?;
            client
                .get(url)
                .header(reqwest::header::USER_AGENT, user_agent)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }
}

/// `reportInstallTelemetry` (interactive-mode.ts:1017-1036): a
/// fire-and-forget GET to `{endpoint}?version={version}` with the pir user
/// agent. Offline, a disabled setting, or a disabled endpoint each suppress
/// the ping entirely (no request); every failure is swallowed (upstream
/// `.then(() => undefined).catch(() => undefined)`). The payload carries
/// only the version — no identifiers, paths, or credentials.
pub async fn report_install(
    version: &str,
    enabled: bool,
    endpoint: Option<&str>,
    offline: bool,
    transport: &dyn ReportInstallTransport,
) {
    if offline || !enabled {
        return;
    }
    let Some(endpoint) = endpoint else {
        return;
    };
    let url = format!("{endpoint}?version={}", encode_uri_component(version));
    let _ = transport
        .get(&url, &pir_user_agent(version), REPORT_INSTALL_TIMEOUT)
        .await;
}

/// The `getChangelogForDisplay` side effects that gate the ping
/// (interactive-mode.ts:991-1014): resumed/continued sessions (messages
/// already present) never report; a fresh install or a version change
/// records the current version as `lastChangelogVersion` and reports.
/// Returns `Some((enabled, endpoint))` when the ping should fire; the
/// network stay gated inside [`report_install`].
///
/// Upstream compares changelog entries, not versions; until the changelog
/// asset lands (T15), version inequality stands in for "new entries"
/// (D-046). The settings write happens even when the ping is disabled,
/// matching upstream.
pub fn prepare_install_report(
    settings: &mut SettingsManager,
    version: &str,
    has_messages: bool,
) -> Option<(bool, Option<String>)> {
    if has_messages {
        return None;
    }
    if settings.get_last_changelog_version().as_deref() == Some(version) {
        return None;
    }
    settings.set_last_changelog_version(version);
    Some((
        is_install_telemetry_enabled(settings),
        report_install_endpoint(settings.get_telemetry_url().as_deref()),
    ))
}

#[cfg(test)]
mod tests {
    //! Port of the telemetry.ts env/setting composition and the
    //! `reportInstallTelemetry` gating (interactive-mode.ts:1017-1036),
    //! with a scripted transport instead of fetch stubs. No `PIR_*` variable
    //! is ever *written* here — process-env writers are confined to
    //! [`crate::core::environment`] tests, so these tests only read
    //! (nobody sets the endpoint vars in the test process); the
    //! env-override logic itself is covered by the pure
    //! [`crate::config::resolve_endpoint`] tests.

    use super::*;
    use crate::core::settings_manager::{Settings, SettingsManagerCreateOptions};
    use serde_json::json;
    use std::sync::Mutex;

    struct ScriptedTransport {
        calls: Mutex<Vec<(String, String, Duration)>>,
        response: Result<(), String>,
    }

    impl ScriptedTransport {
        fn ok() -> Self {
            ScriptedTransport {
                calls: Mutex::new(Vec::new()),
                response: Ok(()),
            }
        }

        fn fails() -> Self {
            ScriptedTransport {
                calls: Mutex::new(Vec::new()),
                response: Err("connection refused".to_string()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        fn calls(&self) -> Vec<(String, String, Duration)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl ReportInstallTransport for ScriptedTransport {
        fn get<'a>(
            &'a self,
            url: &'a str,
            user_agent: &'a str,
            timeout: Duration,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                url.to_string(),
                user_agent.to_string(),
                timeout,
            ));
            let response = self.response.clone();
            Box::pin(async move { response })
        }
    }

    fn manager(settings: serde_json::Value) -> SettingsManager {
        SettingsManager::in_memory(
            Settings::from_map(settings.as_object().expect("object").clone()),
            SettingsManagerCreateOptions::default(),
        )
    }

    /// telemetry.ts:12 — env override (when set) beats the setting.
    #[test]
    fn install_telemetry_enabled_composition() {
        assert!(install_telemetry_enabled(true, None));
        assert!(!install_telemetry_enabled(false, None));
        assert!(install_telemetry_enabled(false, Some(true)));
        assert!(!install_telemetry_enabled(true, Some(false)));
        // Empty PIR_TELEMETRY maps to Some(false) upstream (set-but-empty
        // disables) — covered by the environment.rs override tests.
    }

    #[test]
    fn report_install_endpoint_defaults_and_settings_override() {
        // Read-only env use: no test writes PIR_TELEMETRY_URL.
        assert_eq!(crate::config::ENV_TELEMETRY_URL, "PIR_TELEMETRY_URL");
        assert_eq!(
            report_install_endpoint(None).as_deref(),
            Some(DEFAULT_REPORT_INSTALL_URL)
        );
        assert_eq!(
            report_install_endpoint(Some("https://mirror.test/ping")).as_deref(),
            Some("https://mirror.test/ping")
        );
        // Settings `off` disables the endpoint.
        assert_eq!(report_install_endpoint(Some("off")), None);
    }

    #[tokio::test]
    async fn report_install_pings_version_with_user_agent() {
        let transport = ScriptedTransport::ok();
        report_install(
            "1.2.3",
            true,
            Some("https://telemetry.test/report"),
            false,
            &transport,
        )
        .await;
        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://telemetry.test/report?version=1.2.3");
        assert_eq!(calls[0].1, pir_user_agent("1.2.3"));
        assert_eq!(calls[0].2, REPORT_INSTALL_TIMEOUT);
    }

    #[tokio::test]
    async fn report_install_suppressed_makes_no_request() {
        // Zero-network anchors: each gate suppresses the ping entirely.
        for (enabled, endpoint, offline) in [
            (false, Some(DEFAULT_REPORT_INSTALL_URL), false), // setting off
            (true, None, false),                              // endpoint "off"
            (true, Some(DEFAULT_REPORT_INSTALL_URL), true),   // offline
        ] {
            let transport = ScriptedTransport::ok();
            report_install("1.2.3", enabled, endpoint, offline, &transport).await;
            assert_eq!(
                transport.call_count(),
                0,
                "enabled={enabled} endpoint={endpoint:?} offline={offline}"
            );
        }
    }

    #[tokio::test]
    async fn report_install_swallows_transport_failures() {
        let transport = ScriptedTransport::fails();
        report_install(
            "1.2.3",
            true,
            Some(DEFAULT_REPORT_INSTALL_URL),
            false,
            &transport,
        )
        .await;
        assert_eq!(transport.call_count(), 1);
    }

    #[test]
    fn prepare_install_report_fresh_install_and_version_change() {
        // Fresh install: records the version and reports.
        let mut settings = manager(json!({}));
        let report = prepare_install_report(&mut settings, "1.2.3", false);
        assert_eq!(
            settings.get_last_changelog_version().as_deref(),
            Some("1.2.3")
        );
        let (_enabled, endpoint) = report.expect("fresh install reports");
        assert_eq!(endpoint.as_deref(), Some(DEFAULT_REPORT_INSTALL_URL));

        // Same version: no report, no rewrite.
        assert!(prepare_install_report(&mut settings, "1.2.3", false).is_none());

        // Version change (stands in for new changelog entries, T15): reports.
        assert!(prepare_install_report(&mut settings, "1.3.0", false).is_some());
        assert_eq!(
            settings.get_last_changelog_version().as_deref(),
            Some("1.3.0")
        );

        // Resumed session (messages present): never reports, never records.
        let mut settings = manager(json!({}));
        assert!(prepare_install_report(&mut settings, "1.2.3", true).is_none());
        assert_eq!(settings.get_last_changelog_version(), None);
    }

    #[test]
    fn prepare_install_report_records_even_when_disabled() {
        // Upstream sets lastChangelogVersion unconditionally
        // (interactive-mode.ts:1004, 1010); the gates live in the ping.
        let mut settings = manager(json!({
            "enableInstallTelemetry": false,
            "telemetryUrl": "off",
        }));
        let (_enabled, endpoint) =
            prepare_install_report(&mut settings, "1.2.3", false).expect("trigger fires");
        assert_eq!(endpoint, None);
        assert_eq!(
            settings.get_last_changelog_version().as_deref(),
            Some("1.2.3")
        );
    }
}
