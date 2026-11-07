//! Typed usage-provider wire contract (V18-10 / ADR-0039).
//!
//! This module is the single Rust definition of the usage envelope, the
//! detailed fetch result, the fixed error vocabulary, and the command report
//! DTOs. The host (`crates/rpi`) re-exports these types; native extensions
//! (for example `rpi-ext-usage`) consume the same definitions through their
//! `rpi-ext-host` dependency, so the host, the wasm wire table and the
//! plugins cannot drift into parallel shapes.
//!
//! `UsageFetchResult.source` is only `network` / `cache`. The command report
//! adds `local` for the current-session token-only fallback (ADR-0039); that
//! path never enters `UsageFetchResult`.
//!
//! Unknown envelope fields are ignored on parse and never reach the typed
//! value, so secrets a script echoes are dropped before caching or reporting.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::SessionStatsSnapshot;

/// Pinned envelope schema version. `schemaVersion:1` stays; the additive
/// `windows` / `ledgers` fields do not bump it.
pub const USAGE_SCHEMA_VERSION: u64 = 1;

/// One multi-currency balance entry (`balance[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBalanceEntry {
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topped_up: Option<f64>,
}

/// Legacy plan-quota block (`quota`): used/total/remaining in an optional
/// unit. Kept as the primary fallback for third-party envelopes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// `windows[].status` — required on every window (a missing status is a
/// shape change, never an implicit `ok`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageWindowStatus {
    Ok,
    RateLimited,
    Unknown,
}

/// One typed usage window. Exactly one representation is allowed:
/// percentage (`usedPercent` required, `remainingPercent` derivable),
/// counted/allowance (`unit` plus at least one of used/total/remaining), or
/// unlimited (`unlimited: true`, no numbers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub id: String,
    pub label: String,
    pub status: UsageWindowStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlimited: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
}

/// `ledgers[].kind` — independent on-demand/prepaid/API-balance/quota
/// ledgers. Ledgers are never summed across kinds or currencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageLedgerKind {
    OnDemand,
    Prepaid,
    ApiBalance,
    Quota,
}

impl UsageLedgerKind {
    /// Monetary ledgers must carry a currency so they can never be summed
    /// with token/count quotas.
    pub fn is_monetary(self) -> bool {
        matches!(self, Self::OnDemand | Self::Prepaid | Self::ApiBalance)
    }
}

/// One typed ledger (`ledgers[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedger {
    pub id: String,
    pub label: String,
    pub kind: UsageLedgerKind,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
}

/// The pinned usage envelope. Required: `schemaVersion` / `provider` /
/// `displayText`; unknown fields are ignored on parse and never cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEnvelope {
    pub schema_version: u64,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<Vec<UsageBalanceEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<UsageWindow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledgers: Option<Vec<UsageLedger>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<UsageQuota>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    pub display_text: String,
}

impl UsageEnvelope {
    /// The envelope as the JSON handed to extensions (cache-normalized: only
    /// schema fields are present).
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Fixed error vocabulary (03 §5.5 / 04 §4.2). Serialized kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageErrorKind {
    AuthUnavailable,
    Unauthorized,
    RateLimited,
    Timeout,
    Transport,
    ShapeChanged,
    OriginMismatch,
    NoData,
    CacheMiss,
    Offline,
    RuntimeUnavailable,
    UnsupportedOutput,
}

impl UsageErrorKind {
    /// Stable wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthUnavailable => "auth-unavailable",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate-limited",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::ShapeChanged => "shape-changed",
            Self::OriginMismatch => "origin-mismatch",
            Self::NoData => "no-data",
            Self::CacheMiss => "cache-miss",
            Self::Offline => "offline",
            Self::RuntimeUnavailable => "runtime-unavailable",
            Self::UnsupportedOutput => "unsupported-output",
        }
    }

    /// Fixed, actionable next step. Never carries a raw response, stderr or
    /// credential value.
    pub const fn action(self) -> &'static str {
        match self {
            Self::AuthUnavailable => "Sign in to the provider again, then retry.",
            Self::Unauthorized => "Check the provider account and credentials, then retry.",
            Self::RateLimited => "Wait for the provider limit to reset, then retry.",
            Self::Timeout => "Retry later or raise usage.timeoutMs.",
            Self::Transport => "Check the network and provider status, then retry.",
            Self::ShapeChanged => {
                "The usage response is invalid or changed; update rpi or the provider script."
            }
            Self::OriginMismatch => {
                "Use the provider's official origin; a custom or proxy model URL cannot inherit \
                 subscription usage."
            }
            Self::NoData => "This provider has no usage data.",
            Self::CacheMiss => "Run /usage without --cached to fetch fresh data.",
            Self::Offline => "Offline mode only serves cached usage; unset RPI_OFFLINE to fetch.",
            Self::RuntimeUnavailable => "Install Python 3 (or the provider's runtime), then retry.",
            Self::UnsupportedOutput => "This mode has no output channel for usage reports.",
        }
    }
}

impl std::fmt::Display for UsageErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for UsageErrorKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "auth-unavailable" => Self::AuthUnavailable,
            "unauthorized" => Self::Unauthorized,
            "rate-limited" => Self::RateLimited,
            "timeout" => Self::Timeout,
            "transport" => Self::Transport,
            "shape-changed" => Self::ShapeChanged,
            "origin-mismatch" => Self::OriginMismatch,
            "no-data" => Self::NoData,
            "cache-miss" => Self::CacheMiss,
            "offline" => Self::Offline,
            "runtime-unavailable" => Self::RuntimeUnavailable,
            "unsupported-output" => Self::UnsupportedOutput,
            _ => return Err(()),
        })
    }
}

/// One classified usage failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageError {
    pub kind: UsageErrorKind,
    pub action: String,
}

impl UsageError {
    pub fn new(kind: UsageErrorKind) -> Self {
        Self {
            kind,
            action: kind.action().to_owned(),
        }
    }
}

/// Where a detailed fetch result came from. Only `network` / `cache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageSource {
    Network,
    Cache,
}

/// `ctx.usage.fetchDetailed` / `ctx.usage.cached` result. `source` is omitted
/// when there is no envelope; `stale` is always present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageFetchResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<UsageEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<UsageSource>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<UsageError>,
}

impl UsageFetchResult {
    pub fn network(envelope: UsageEnvelope) -> Self {
        Self {
            envelope: Some(envelope),
            source: Some(UsageSource::Network),
            stale: false,
            error: None,
        }
    }

    pub fn cache(envelope: UsageEnvelope, stale: bool) -> Self {
        Self {
            envelope: Some(envelope),
            source: Some(UsageSource::Cache),
            stale,
            error: None,
        }
    }

    /// A network success that failed to refresh an older identity-matched
    /// success: the stale value and the error are both reported.
    pub fn cache_with_error(envelope: UsageEnvelope, error: UsageError) -> Self {
        Self {
            envelope: Some(envelope),
            source: Some(UsageSource::Cache),
            stale: true,
            error: Some(error),
        }
    }

    pub fn failure(error: UsageError) -> Self {
        Self {
            envelope: None,
            source: None,
            stale: false,
            error: Some(error),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `source` of a command report: the detailed fetch sources plus the
/// current-session local token fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageReportSource {
    Network,
    Cache,
    Local,
}

impl From<UsageSource> for UsageReportSource {
    fn from(source: UsageSource) -> Self {
        match source {
            UsageSource::Network => Self::Network,
            UsageSource::Cache => Self::Cache,
        }
    }
}

/// Current-session token counts (ADR-0039 decision 5). `used` is
/// `input + cacheWrite + output`; `cacheRead` is reported but not added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub used: u64,
}

impl LocalTokenUsage {
    pub fn from_session_stats(stats: &SessionStatsSnapshot) -> Self {
        let input = stats.tokens.input;
        let output = stats.tokens.output;
        let cache_write = stats.tokens.cache_write;
        Self {
            input,
            output,
            cache_read: stats.tokens.cache_read,
            cache_write,
            used: input.saturating_add(cache_write).saturating_add(output),
        }
    }
}

/// Machine report for one provider or `all` (always `schemaVersion`,
/// `provider`, `stale`; optional `current`, `source`, `envelope`, `error`,
/// `localTokens`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub schema_version: u64,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<UsageReportSource>,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<UsageEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<UsageError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_tokens: Option<LocalTokenUsage>,
}

impl UsageReport {
    pub fn from_fetch(provider: &str, result: &UsageFetchResult, current: bool) -> Self {
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            provider: provider.to_owned(),
            current: current.then_some(true),
            source: result.source.map(UsageReportSource::from),
            stale: result.stale,
            envelope: result.envelope.clone(),
            error: result.error.clone(),
            local_tokens: None,
        }
    }

    /// Current-session local token report: no envelope, no quota fields, no
    /// error, `source:"local"`, `stale:false`.
    pub fn local(provider: &str, tokens: LocalTokenUsage) -> Self {
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            provider: provider.to_owned(),
            current: None,
            source: Some(UsageReportSource::Local),
            stale: false,
            envelope: None,
            error: None,
            local_tokens: Some(tokens),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope() -> UsageEnvelope {
        UsageEnvelope {
            schema_version: 1,
            provider: "x".to_owned(),
            plan: None,
            balance: None,
            windows: None,
            ledgers: None,
            quota: None,
            used: None,
            reset_at: None,
            display_text: "x: ok".to_owned(),
        }
    }

    #[test]
    fn error_kind_spellings_actions_and_round_trip() {
        let expected = [
            (UsageErrorKind::AuthUnavailable, "auth-unavailable"),
            (UsageErrorKind::Unauthorized, "unauthorized"),
            (UsageErrorKind::RateLimited, "rate-limited"),
            (UsageErrorKind::Timeout, "timeout"),
            (UsageErrorKind::Transport, "transport"),
            (UsageErrorKind::ShapeChanged, "shape-changed"),
            (UsageErrorKind::OriginMismatch, "origin-mismatch"),
            (UsageErrorKind::NoData, "no-data"),
            (UsageErrorKind::CacheMiss, "cache-miss"),
            (UsageErrorKind::Offline, "offline"),
            (UsageErrorKind::RuntimeUnavailable, "runtime-unavailable"),
            (UsageErrorKind::UnsupportedOutput, "unsupported-output"),
        ];
        for (kind, spelling) in expected {
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(spelling.parse::<UsageErrorKind>(), Ok(kind));
            assert!(!kind.action().is_empty());
            let error = UsageError::new(kind);
            assert_eq!(
                serde_json::to_value(&error).expect("error json"),
                json!({"kind": spelling, "action": kind.action()})
            );
        }
        assert!("unknown".parse::<UsageErrorKind>().is_err());
    }

    #[test]
    fn fetch_result_omits_absent_fields_and_keeps_stale() {
        let failure = UsageFetchResult::failure(UsageError::new(UsageErrorKind::CacheMiss));
        let json = failure.to_json();
        assert_eq!(json["stale"], json!(false));
        assert_eq!(json["error"]["kind"], json!("cache-miss"));
        assert!(json.get("envelope").is_none());
        assert!(json.get("source").is_none());

        let network = UsageFetchResult::network(envelope());
        let json = network.to_json();
        assert_eq!(json["source"], json!("network"));
        assert_eq!(json["stale"], json!(false));
        assert_eq!(json["envelope"]["displayText"], json!("x: ok"));

        let stale = UsageFetchResult::cache_with_error(
            envelope(),
            UsageError::new(UsageErrorKind::Transport),
        );
        let json = stale.to_json();
        assert_eq!(json["source"], json!("cache"));
        assert_eq!(json["stale"], json!(true));
        assert_eq!(json["error"]["kind"], json!("transport"));
    }

    #[test]
    fn reports_always_carry_the_required_fields() {
        let miss = UsageFetchResult::failure(UsageError::new(UsageErrorKind::CacheMiss));
        let report = UsageReport::from_fetch("x", &miss, false);
        let json = report.to_json();
        assert_eq!(json["schemaVersion"], json!(1));
        assert_eq!(json["provider"], json!("x"));
        assert_eq!(json["stale"], json!(false));
        assert_eq!(json["error"]["kind"], json!("cache-miss"));
        assert!(json.get("source").is_none());
        assert!(json.get("envelope").is_none());
        assert!(json.get("current").is_none());
        assert!(json.get("localTokens").is_none());

        let network = UsageFetchResult::network(envelope());
        let json = UsageReport::from_fetch("x", &network, true).to_json();
        assert_eq!(json["current"], json!(true));
        assert_eq!(json["source"], json!("network"));
        assert!(json.get("localTokens").is_none());
    }

    #[test]
    fn local_report_has_no_quota_surface() {
        let tokens = LocalTokenUsage {
            input: 10,
            output: 3,
            cache_read: 7,
            cache_write: 2,
            used: 15,
        };
        let report = UsageReport::local("openrouter", tokens).to_json();
        assert_eq!(report["schemaVersion"], json!(1));
        assert_eq!(report["provider"], json!("openrouter"));
        assert_eq!(report["stale"], json!(false));
        assert_eq!(report["source"], json!("local"));
        assert_eq!(
            report["localTokens"],
            json!({"input": 10, "output": 3, "cacheRead": 7, "cacheWrite": 2, "used": 15})
        );
        for absent in ["envelope", "error", "plan", "windows", "ledgers", "quota"] {
            assert!(report.get(absent).is_none(), "{absent} must be absent");
        }
    }

    #[test]
    fn local_tokens_use_input_plus_cache_write_plus_output() {
        let stats = SessionStatsSnapshot {
            session_id: "s".to_owned(),
            session_file: None,
            started_at: "2026-10-31T00:00:00Z".to_owned(),
            entry_count: 3,
            user_messages: 1,
            assistant_messages: 2,
            tokens: crate::types::SessionTokenStats {
                input: 10,
                output: 5,
                cache_read: 100,
                cache_write: 2,
            },
            cost: 0.0,
        };
        let tokens = LocalTokenUsage::from_session_stats(&stats);
        assert_eq!(tokens.input, 10);
        assert_eq!(tokens.output, 5);
        assert_eq!(tokens.cache_read, 100);
        assert_eq!(tokens.cache_write, 2);
        assert_eq!(tokens.used, 17);
    }

    #[test]
    fn typed_envelope_serializes_the_additive_fields() {
        let mut envelope = envelope();
        envelope.windows = Some(vec![UsageWindow {
            id: "7d".to_owned(),
            label: "7d".to_owned(),
            status: UsageWindowStatus::RateLimited,
            used_percent: Some(42.5),
            remaining_percent: Some(57.5),
            used: None,
            total: None,
            remaining: None,
            unit: None,
            unlimited: None,
            reset_at: Some("2026-10-28T12:00:00Z".to_owned()),
            duration_seconds: Some(604_800),
        }]);
        envelope.ledgers = Some(vec![UsageLedger {
            id: "on_demand".to_owned(),
            label: "On-demand".to_owned(),
            kind: UsageLedgerKind::OnDemand,
            unit: "USD".to_owned(),
            currency: Some("USD".to_owned()),
            used: Some(3.0),
            total: Some(50.0),
            remaining: Some(47.0),
        }]);
        let json = envelope.to_json();
        assert_eq!(json["windows"][0]["status"], json!("rate-limited"));
        assert_eq!(json["windows"][0]["usedPercent"], json!(42.5));
        assert_eq!(json["ledgers"][0]["kind"], json!("on-demand"));
        assert_eq!(json["ledgers"][0]["currency"], json!("USD"));
    }
}
