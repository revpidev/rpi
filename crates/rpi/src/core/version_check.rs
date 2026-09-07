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

// ===== V14-19: RC 更新通道（rpi 自有，无上游对照）=====
//
// 通道只决定「候选版本从哪来」（本体端点 URL + registry 预发布过滤），
// 比较仍由 [`is_newer_package_version`] 的 semver 全序承担（预发布段天然
// 有序：`0.1.5-rc.1 < 0.1.5-rc.2 < 0.1.5`）——通道迁移四象限不写分支，
// 见 v0.1.4 需求基线 R6.1.3 / 设计基线 §8.1。

/// 更新通道（V14-19，rpi 自有）：命令行 `--rc` 选 [`PreRelease`]，缺省
/// [`Stable`]。通道是单次命令的属性，不持久化（R6.1.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateChannel {
    /// 缺省通道：最新正式版（现状行为，预发布被排除）。
    #[default]
    Stable,
    /// 预发布通道（`--rc`）：最新 RC 版本（`<stable>-rc.<N>` 形态，
    /// R6.1.1 版本号规范）。
    PreRelease,
}

/// RC 端点文件名（同目录推导目标，设计 §8.2）。
const RC_ENDPOINT_FILE: &str = "latest-rc-version.json";

/// 由已解析的 stable 端点 URL 推导 RC 端点（设计 §8.2）：同目录末段
/// 无条件替换为 `latest-rc-version.json`——镜像运营方无论把 stable
/// 端点命名为什么，只需同目录放同名 RC 文件即可；裸 host（无 `/`）
/// 回退官方默认。
///
/// `endpoint` 为 `None`（`off`/未配置）时 RC 探测同样禁用（R6.2.3：
/// 通道推导自 stable 端点配置，不新增 RC 专用配置项）。
pub fn rc_probe_url(endpoint: Option<&str>) -> Option<String> {
    let url = endpoint?;
    Some(match url.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{RC_ENDPOINT_FILE}"),
        // 裸 host（无路径）：根目录即「同目录」。
        None => format!("{url}/{RC_ENDPOINT_FILE}"),
    })
}

/// 通道 → 探测 URL：stable 透传已解析端点；PreRelease 用
/// [`rc_probe_url`] 推导。
pub fn channel_probe_url(channel: UpdateChannel, endpoint: Option<&str>) -> Option<String> {
    match channel {
        UpdateChannel::Stable => endpoint.map(str::to_string),
        UpdateChannel::PreRelease => rc_probe_url(endpoint),
    }
}

/// 版本串是否含 semver 预发布段（`0.1.5-rc.1` → `true`；非法版本按
/// 非 pre 处理）。横幅文案与启动通道判定共用。
pub fn is_prerelease_version(version: &str) -> bool {
    semver::Version::parse(strip_version_prefix(version.trim()))
        .map(|parsed| !parsed.pre.is_empty())
        .unwrap_or(false)
}

/// 版本串所属通道：含预发布段 → [`UpdateChannel::PreRelease`]，否则
/// [`UpdateChannel::Stable`]（非法版本按 stable）。启动检查（R6.4.1）
/// 与横幅文案共用。
pub fn channel_of_version(version: &str) -> UpdateChannel {
    if is_prerelease_version(version) {
        UpdateChannel::PreRelease
    } else {
        UpdateChannel::Stable
    }
}

/// 运行中构建所属的启动检查通道（R6.4.1）：预发布构建（`VERSION` 含
/// 预发布段）启动探测 RC 端点，stable 构建维持 stable 端点。
fn running_channel() -> UpdateChannel {
    channel_of_version(crate::config::VERSION)
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
/// `!response.ok` → `undefined`; transport failures are `Err`. `retry`
/// mirrors upstream `options.retry` (version-check.ts:97-108): `true` allows
/// two retries (the `--self` update path, package-manager-cli.ts:479),
/// `false` is a single attempt (the startup path passes no retry).
pub trait LatestVersionTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
        retry: bool,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;
}

/// Production transport: reqwest with rustls (no proxy code of its own —
/// reqwest's env proxy support covers `HTTP_PROXY`/`HTTPS_PROXY`).
///
/// Uses [`fetch_with_retry`] (46b53b995) for bounded immediate retry on
/// transient failures — this is the management-plane HTTP helper used by
/// version-check / catalog / managed-tool / package downloads. Retry count
/// follows the caller's `retry` flag (upstream `maxRetries: options.retry ?
/// 2 : 0`, version-check.ts:66).
pub struct ReqwestLatestVersionTransport;

impl LatestVersionTransport for ReqwestLatestVersionTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
        timeout: Duration,
        retry: bool,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .build()
                .map_err(|e| e.to_string())?;
            let url = url.to_owned();
            let ua = user_agent.to_owned();
            let response = crate::utils::management_http::fetch_with_retry(
                move || {
                    let client = client.clone();
                    let url = url.clone();
                    let ua = ua.clone();
                    Box::pin(async move {
                        client
                            .get(url)
                            .header(reqwest::header::USER_AGENT, ua)
                            .header(reqwest::header::ACCEPT, "application/json")
                    })
                },
                None,
                &crate::utils::management_http::FetchRetryOptions {
                    max_retries: Some(if retry { 2 } else { 0 }),
                    timeout: Some(timeout),
                    ..Default::default()
                },
            )
            .await?;
            if !response.status().is_success() {
                return Ok(None);
            }
            response.text().await.map(Some).map_err(|e| e.to_string())
        })
    }
}

/// `getLatestPiRelease` (version-check.ts:30-61) with the default URL and
/// timeout; `RPI_OFFLINE` short-circuits to `None`. Like the upstream
/// zero-options call, this does not retry (`maxRetries: 0`).
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
        false,
    )
    .await
}

/// [`get_latest_rpi_release`] with explicit URL / timeout / offline flag /
/// retry policy (test seam; also the W6a endpoint-override call site).
/// `retry` is upstream `options.retry` (version-check.ts:30): the `--self`
/// update path passes `true`, the startup path `false`.
pub async fn get_latest_rpi_release_with(
    current_version: &str,
    transport: &dyn LatestVersionTransport,
    url: &str,
    timeout: Duration,
    offline: bool,
    retry: bool,
) -> Result<Option<LatestRpiRelease>, String> {
    if offline {
        return Ok(None);
    }
    let Some(body) = transport
        .get(url, &rpi_user_agent(current_version), timeout, retry)
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
/// (`RPI_SKIP_VERSION_CHECK` / `RPI_OFFLINE`) and applying the running
/// build's channel (V14-19, R6.4.1): a pre-release build probes the RC
/// endpoint, a stable build the stable endpoint. The skip/offline/off
/// gates suppress both channels identically.
pub fn startup_version_check_url(settings_url: Option<&str>) -> Option<String> {
    startup_probe_url(
        crate::core::environment::skip_version_check(),
        crate::core::environment::is_offline(),
        channel_probe_url(
            running_channel(),
            version_check_endpoint(settings_url).as_deref(),
        ),
    )
}

/// `checkForNewPiVersion` (version-check.ts:70-81): `None` unless the probe
/// reports a strictly newer version; every transport/parse failure is
/// swallowed (upstream `try/catch`). `url` is the resolved startup probe
/// URL — `None` disables the check with zero network traffic. The startup
/// path never retries (upstream passes no `retry`, version-check.ts:101).
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
        calls: Mutex<Vec<(String, String, bool)>>,
        response: Result<Option<String>, String>,
    }

    impl ScriptedTransport {
        fn responds(response: Result<Option<String>, String>) -> Self {
            ScriptedTransport {
                calls: Mutex::new(Vec::new()),
                response,
            }
        }

        fn retry_flags(&self) -> Vec<bool> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(_, _, retry)| *retry)
                .collect()
        }
    }

    impl LatestVersionTransport for ScriptedTransport {
        fn get<'a>(
            &'a self,
            url: &'a str,
            user_agent: &'a str,
            _timeout: Duration,
            retry: bool,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                url.to_string(),
                user_agent.to_string(),
                retry,
            ));
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
        // V14-13 FR-C 三态补缺 (#8226/#8239): an OLDER candidate must not
        // read as a newer release (`semver.gt`, version-check.ts:43-49).
        assert!(!is_newer_package_version("1.2.9", "1.3.0"));
        assert!(!is_newer_package_version("1.2.3", "1.3.0"));
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
            false,
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

    // ---- V14-19: RC 更新通道（rpi 自有，无上游对照）----

    /// 通道迁移四象限（R6.1.3）：全部由 semver 全序自然导出，无通道分支。
    #[test]
    fn channel_transition_matrix_via_semver_total_order() {
        // stable 构建 + RC 通道：升级到更新 rc；同基线 rc 不降级。
        assert!(is_newer_package_version("0.1.6-rc.1", "0.1.5"));
        assert!(!is_newer_package_version("0.1.5-rc.1", "0.1.5"));
        // rc 构建 + RC 通道：rc 递增升级。
        assert!(is_newer_package_version("0.1.5-rc.2", "0.1.5-rc.1"));
        assert!(is_newer_package_version("0.1.5-rc.10", "0.1.5-rc.9"));
        // rc 构建 + stable 通道：同基线 stable 即通道切换；rc 新于最新
        // stable 时不降级。
        assert!(is_newer_package_version("0.1.5", "0.1.5-rc.2"));
        assert!(!is_newer_package_version("0.1.4", "0.1.6-rc.1"));
        // stable 构建 + stable 通道（现状）。
        assert!(is_newer_package_version("0.1.5", "0.1.4"));
        assert!(!is_newer_package_version("0.1.4", "0.1.4"));
    }

    /// RC 端点推导（R6.2.3）：同目录末段替换；`off`/未配置传播禁用。
    #[test]
    fn rc_probe_url_derives_same_directory_sibling() {
        // 官方默认端点。
        assert_eq!(
            rc_probe_url(Some(LATEST_VERSION_URL)).as_deref(),
            Some("https://revpi.dev/api/latest-rc-version.json")
        );
        // 自定义镜像：同目录同名文件（镜像运营方唯一规则）。
        assert_eq!(
            rc_probe_url(Some("https://mirror.test/api/latest-version")).as_deref(),
            Some("https://mirror.test/api/latest-rc-version.json")
        );
        // 裸 host（无路径）：根目录即「同目录」。
        assert_eq!(
            rc_probe_url(Some("mirror.test")).as_deref(),
            Some("mirror.test/latest-rc-version.json")
        );
        // `off`/未配置 → RC 探测同样禁用。
        assert_eq!(rc_probe_url(None), None);
    }

    /// 通道 → 探测 URL 组合（设计 §8.2）。
    #[test]
    fn channel_probe_url_selects_endpoint_per_channel() {
        let endpoint = Some("https://revpi.dev/api/latest-version");
        assert_eq!(
            channel_probe_url(UpdateChannel::Stable, endpoint).as_deref(),
            Some("https://revpi.dev/api/latest-version")
        );
        assert_eq!(
            channel_probe_url(UpdateChannel::PreRelease, endpoint).as_deref(),
            Some("https://revpi.dev/api/latest-rc-version.json")
        );
        assert_eq!(channel_probe_url(UpdateChannel::Stable, None), None);
        assert_eq!(channel_probe_url(UpdateChannel::PreRelease, None), None);
    }

    /// 预发布版本判定（R6.1.1）：`v` 前缀容忍；非法按 stable。
    #[test]
    fn channel_of_version_detects_prerelease_builds() {
        assert_eq!(channel_of_version("0.1.4"), UpdateChannel::Stable);
        assert_eq!(channel_of_version("0.1.5-rc.1"), UpdateChannel::PreRelease);
        assert_eq!(channel_of_version("v0.1.5-rc.2"), UpdateChannel::PreRelease);
        // 判定按 semver 预发布段（非 rc 形态也是预发布构建；站点端
        // 形态闸会拦非 rc tag，见 V14-19 FR-G R3）。
        assert_eq!(
            channel_of_version("0.2.0-beta.1"),
            UpdateChannel::PreRelease
        );
        assert_eq!(channel_of_version("not-a-version"), UpdateChannel::Stable);
        assert!(is_prerelease_version("0.1.5-rc.1"));
        assert!(!is_prerelease_version("0.1.5"));
    }

    /// 启动探测 URL 的通道组合（R6.4.1）：抑制门对两通道一致。
    #[test]
    fn startup_probe_channel_composition_respects_gates() {
        let endpoint = Some("https://revpi.dev/api/latest-version".to_string());
        // rc 构建 → RC 端点；stable 构建 → stable 端点。
        let rc_url = channel_probe_url(channel_of_version("0.1.5-rc.1"), endpoint.as_deref());
        assert_eq!(
            startup_probe_url(false, false, rc_url.clone()).as_deref(),
            Some("https://revpi.dev/api/latest-rc-version.json")
        );
        let stable_url = channel_probe_url(channel_of_version("0.1.4"), endpoint.as_deref());
        assert_eq!(
            startup_probe_url(false, false, stable_url).as_deref(),
            Some("https://revpi.dev/api/latest-version")
        );
        // skip / offline / off 对两通道同样禁用。
        assert_eq!(startup_probe_url(true, false, rc_url.clone()), None);
        assert_eq!(startup_probe_url(false, true, rc_url.clone()), None);
        assert_eq!(startup_probe_url(false, false, None), None);
    }

    /// RC 通道下本体探测命中推导端点（R6.2.1）：transport 记录的 URL
    /// 即 `latest-rc-version.json`。
    #[tokio::test]
    async fn pre_release_channel_probes_the_rc_endpoint() {
        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "9.9.9-rc.1"}"#.to_string())));
        let release = get_latest_rpi_release_with(
            "1.0.0",
            &transport,
            rc_probe_url(Some(LATEST_VERSION_URL)).as_deref().unwrap(),
            DEFAULT_VERSION_CHECK_TIMEOUT,
            false,
            true,
        )
        .await
        .expect("release")
        .expect("some release");
        assert_eq!(release.version, "9.9.9-rc.1");
        let calls = transport.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls[0].0, "https://revpi.dev/api/latest-rc-version.json");
        // 比较仍由 semver 全序承担：rc.1 > 1.0.0 才算新。
        assert!(is_newer_package_version(&release.version, "1.0.0"));
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

    /// T23.4: the startup probe never retries (upstream `checkForNewPiVersion`
    /// passes no `retry` → `maxRetries: 0`, version-check.ts:97-108); only
    /// callers that opt in (the `--self` update path) get retries.
    #[tokio::test]
    async fn startup_check_disables_retry_unless_the_caller_opts_in() {
        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "9.9.9"}"#.to_string())));
        check_for_new_rpi_release("1.0.0", &transport, Some(LATEST_VERSION_URL)).await;
        assert_eq!(transport.retry_flags(), vec![false]);

        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "9.9.9"}"#.to_string())));
        get_latest_rpi_release("1.0.0", &transport)
            .await
            .expect("release");
        assert_eq!(transport.retry_flags(), vec![false]);

        let transport =
            ScriptedTransport::responds(Ok(Some(r#"{"version": "9.9.9"}"#.to_string())));
        get_latest_rpi_release_with(
            "1.0.0",
            &transport,
            LATEST_VERSION_URL,
            DEFAULT_VERSION_CHECK_TIMEOUT,
            false,
            true,
        )
        .await
        .expect("release");
        assert_eq!(transport.retry_flags(), vec![true]);
    }
}
