//! V16-05 FR-A: scripted usage-provider framework (rpi-own).
//!
//! A usage provider is an executable script. The host resolves a provider to
//! its script path (explicit `usage.providers` settings override > user
//! directory > plugin registration), runs it through the existing exec
//! channel with a JSON context on stdin, and validates the JSON envelope it
//! prints on stdout. Execution governance (timeout, serialization, output
//! limits) and the last-success cache live here; extensions consume the
//! result through `ctx.usage.*`.
//!
//! The pinned stdout envelope (schemaVersion 1) — required fields
//! `schemaVersion` / `provider` / `displayText`, everything else optional:
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "provider": "deepseek",
//!   "plan": "Coding Plan",
//!   "balance": [{"currency": "CNY", "total": 12.5, "granted": 10.0, "toppedUp": 2.5}],
//!   "quota": {"used": 12.5, "total": 30.0, "remaining": 17.5, "unit": "CNY"},
//!   "used": 12.5,
//!   "resetAt": "2026-10-08T00:00:00Z",
//!   "displayText": "deepseek: CNY 12.50"
//! }
//! ```
//!
//! Unknown fields are ignored (forward compatibility); the parsed envelope
//! never carries secret fields, so the cache cannot persist one even when a
//! script echoes it. Credentials reach the script through the child process
//! environment only (the stdin context names the variable, never its value).

pub mod cache;
pub mod registry;
pub mod script;

pub use cache::{DEFAULT_USAGE_CACHE_TTL_MS, UsageCache, UsageCacheEntry};
pub use registry::{
    DEFAULT_USAGE_TIMEOUT_MS, MAX_USAGE_STDOUT_BYTES, MAX_USAGE_TIMEOUT_MS, MIN_USAGE_TIMEOUT_MS,
    UsageFrameworkSettings, UsageProviderRegistry, UsageProviderSource, UsageProviderSpec,
};
pub use script::{UsageScriptContext, UsageScriptEnv, execute_usage_script, usage_api_key_env};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pinned envelope schema version (V16-05 §7.2).
pub const USAGE_ENVELOPE_SCHEMA_VERSION: u64 = 1;

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

/// Plan-quota block (`quota`): used/total/remaining in an optional unit.
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

/// Parse and validate a script's stdout envelope. Failures degrade the
/// provider fetch (the caller keeps the last success); the message is for
/// diagnostics, never for the footer.
pub fn parse_usage_envelope(stdout: &str) -> Result<UsageEnvelope, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("stdout is not JSON: {error}"))?;
    let envelope: UsageEnvelope =
        serde_json::from_value(value).map_err(|error| format!("envelope shape: {error}"))?;
    if envelope.schema_version != USAGE_ENVELOPE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schemaVersion {}",
            envelope.schema_version
        ));
    }
    if envelope.provider.trim().is_empty() {
        return Err("provider must be a non-empty string".to_owned());
    }
    if envelope.display_text.trim().is_empty() {
        return Err("displayText must be a non-empty string".to_owned());
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_envelope() {
        let envelope = parse_usage_envelope(
            r#"{
                "schemaVersion": 1,
                "provider": "deepseek",
                "plan": "Coding Plan",
                "balance": [{"currency": "CNY", "total": 12.5, "granted": 10.0, "toppedUp": 2.5}],
                "quota": {"used": 12.5, "total": 30.0, "remaining": 17.5, "unit": "CNY"},
                "used": 12.5,
                "resetAt": "2026-10-08T00:00:00Z",
                "displayText": "deepseek: CNY 12.50"
            }"#,
        )
        .expect("valid envelope");
        assert_eq!(envelope.provider, "deepseek");
        assert_eq!(envelope.plan.as_deref(), Some("Coding Plan"));
        assert_eq!(envelope.balance.as_ref().unwrap()[0].total, Some(12.5));
        assert_eq!(envelope.quota.as_ref().unwrap().remaining, Some(17.5));
        assert_eq!(envelope.used, Some(12.5));
        assert_eq!(envelope.reset_at.as_deref(), Some("2026-10-08T00:00:00Z"));
        assert_eq!(envelope.display_text, "deepseek: CNY 12.50");
    }

    #[test]
    fn parses_a_minimal_envelope() {
        let envelope =
            parse_usage_envelope(r#"{"schemaVersion":1,"provider":"x","displayText":"x: ok"}"#)
                .expect("minimal envelope");
        assert!(envelope.plan.is_none());
        assert!(envelope.balance.is_none());
        assert!(envelope.quota.is_none());
        assert!(envelope.used.is_none());
        assert!(envelope.reset_at.is_none());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse_usage_envelope(r#"{"provider":"x","displayText":"ok"}"#).is_err());
        assert!(parse_usage_envelope(r#"{"schemaVersion":1,"displayText":"ok"}"#).is_err());
        assert!(parse_usage_envelope(r#"{"schemaVersion":1,"provider":"x"}"#).is_err());
        assert!(
            parse_usage_envelope(r#"{"schemaVersion":1,"provider":"x","displayText":""}"#).is_err()
        );
    }

    #[test]
    fn rejects_wrong_types_and_schema_versions() {
        assert!(
            parse_usage_envelope(r#"{"schemaVersion":"1","provider":"x","displayText":"ok"}"#)
                .is_err()
        );
        assert!(
            parse_usage_envelope(r#"{"schemaVersion":2,"provider":"x","displayText":"ok"}"#)
                .is_err()
        );
        assert!(
            parse_usage_envelope(
                r#"{"schemaVersion":1,"provider":"x","displayText":"ok","used":"many"}"#
            )
            .is_err()
        );
        assert!(parse_usage_envelope("not json").is_err());
        assert!(parse_usage_envelope("").is_err());
    }

    #[test]
    fn ignores_unknown_fields_and_never_serializes_secrets() {
        let envelope = parse_usage_envelope(
            r#"{"schemaVersion":1,"provider":"x","displayText":"ok","apiKey":"sk-secret","token":"t"}"#,
        )
        .expect("unknown fields are ignored");
        let json = envelope.to_json();
        assert_eq!(json.get("apiKey"), None);
        assert_eq!(json.get("token"), None);
        assert_eq!(json.get("schemaVersion"), Some(&Value::from(1)));
    }
}
