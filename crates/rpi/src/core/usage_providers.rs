//! V16-05 FR-A / V18-10: scripted usage-provider framework (rpi-own).
//!
//! A usage provider is an executable script. The host resolves a provider to
//! its script path (explicit `usage.providers` settings override > user
//! directory > plugin registration), runs it through the existing exec
//! channel with a JSON context on stdin, and validates the JSON envelope it
//! prints on stdout. Execution governance (timeout, serialization, output
//! limits), credential identity, offline gating and the last-success cache
//! live here; extensions consume the result through `ctx.usage.*`.
//!
//! The pinned stdout envelope (schemaVersion 1) — required fields
//! `schemaVersion` / `provider` / `displayText`, everything else optional:
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "provider": "xai",
//!   "plan": "SuperGrok",
//!   "displayText": "7d 42.5%(~3d22h)",
//!   "windows": [{
//!     "id": "included_allowance",
//!     "label": "7d",
//!     "usedPercent": 42.5,
//!     "remainingPercent": 57.5,
//!     "resetAt": "2026-10-28T12:00:00Z",
//!     "status": "ok",
//!     "durationSeconds": 604800
//!   }],
//!   "ledgers": [{
//!     "id": "on_demand",
//!     "label": "On-demand",
//!     "kind": "on-demand",
//!     "unit": "USD",
//!     "currency": "USD",
//!     "used": 3.0,
//!     "total": 50.0,
//!     "remaining": 47.0
//!   }]
//! }
//! ```
//!
//! The typed value itself lives in [`rpi_ext_host::usage`] so the host and
//! native plugins share one definition. Unknown fields are ignored (forward
//! compatibility); the parsed envelope never carries secret fields, so the
//! cache cannot persist one even when a script echoes it. Credentials reach
//! the script through the child process environment only (the stdin context
//! names the variable, never its value).

pub mod cache;
pub mod identity;
pub mod registry;
pub mod script;

pub use cache::{DEFAULT_USAGE_CACHE_TTL_MS, UsageCache, UsageCacheEntry, UsageCacheKey};
pub use identity::{
    LegacyCredentialResolver, SessionUsageCredentialResolver, UsageCredentialKind,
    UsageCredentialResolver, UsageCredentials, builtin_usage_auth_providers,
    builtin_usage_credential_kind,
};
pub use registry::{
    DEFAULT_USAGE_TIMEOUT_MS, MAX_USAGE_STDOUT_BYTES, MAX_USAGE_TIMEOUT_MS, MIN_USAGE_TIMEOUT_MS,
    UsageFrameworkSettings, UsageProviderRegistry, UsageProviderSource, UsageProviderSpec,
};
pub use rpi_ext_host::usage::{
    LocalTokenUsage, USAGE_SCHEMA_VERSION, UsageBalanceEntry, UsageEnvelope, UsageError,
    UsageErrorKind, UsageFetchResult, UsageLedger, UsageLedgerKind, UsageQuota, UsageReport,
    UsageReportSource, UsageSource, UsageWindow, UsageWindowStatus,
};
pub use script::{
    USAGE_OAUTH_ACCESS_ENV, USAGE_OAUTH_ACCOUNT_ID_ENV, UsageScriptContext, UsageScriptEnv,
    UsageScriptError, execute_usage_script, usage_api_key_env,
};

use std::collections::HashSet;

use serde_json::Value;

/// Pinned envelope schema version (V16-05 §7.2; unchanged by V18-10).
pub const USAGE_ENVELOPE_SCHEMA_VERSION: u64 = USAGE_SCHEMA_VERSION;

/// Error envelope recognized before the normal envelope's `displayText`
/// check (04 §5.1): an object carrying `error:{kind}`. Invalid or unknown
/// kinds are a shape change, never a silent success.
pub fn parse_error_envelope(stdout: &str) -> Option<UsageError> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let error = value.as_object()?.get("error")?;
    if error.is_null() {
        return None;
    }
    let kind = error
        .as_object()
        .and_then(|error| error.get("kind"))
        .and_then(Value::as_str)
        .and_then(|kind| kind.parse::<UsageErrorKind>().ok());
    Some(UsageError::new(
        kind.unwrap_or(UsageErrorKind::ShapeChanged),
    ))
}

/// Parse and validate a script's stdout envelope. Failures degrade the
/// provider fetch (the caller keeps the last success); the message is for
/// diagnostics, never for the footer. An error envelope is recognized before
/// the required normal fields, so a missing `displayText` on the error path is
/// not misreported as a shape change.
pub fn parse_usage_envelope(stdout: &str) -> Result<UsageEnvelope, String> {
    if let Some(error) = parse_error_envelope(stdout) {
        return Err(format!("error envelope ({})", error.kind));
    }
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("stdout is not JSON: {error}"))?;
    let mut envelope: UsageEnvelope =
        serde_json::from_value(value).map_err(|error| format!("envelope shape: {error}"))?;
    validate_usage_envelope(&mut envelope)?;
    Ok(envelope)
}

/// Validate the typed envelope and fill safe derivations (a missing
/// percentage side or count side when the total is known).
pub fn validate_usage_envelope(envelope: &mut UsageEnvelope) -> Result<(), String> {
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
    if let Some(windows) = envelope.windows.as_mut() {
        let mut ids = HashSet::new();
        for window in windows {
            validate_window(window)?;
            if !ids.insert(window.id.clone()) {
                return Err(format!("duplicate window id {:?}", window.id));
            }
        }
    }
    if let Some(ledgers) = envelope.ledgers.as_mut() {
        let mut ids = HashSet::new();
        for ledger in ledgers {
            validate_ledger(ledger)?;
            if !ids.insert(ledger.id.clone()) {
                return Err(format!("duplicate ledger id {:?}", ledger.id));
            }
        }
    }
    Ok(())
}

fn validate_window(window: &mut UsageWindow) -> Result<(), String> {
    if window.id.trim().is_empty() {
        return Err("window id must be a non-empty string".to_owned());
    }
    if window.label.trim().is_empty() {
        return Err(format!(
            "window {:?}: label must be a non-empty string",
            window.id
        ));
    }
    if window.duration_seconds.is_some_and(|seconds| seconds == 0) {
        return Err(format!(
            "window {:?}: durationSeconds must be a positive integer",
            window.id
        ));
    }
    if let Some(reset_at) = window.reset_at.as_deref()
        && chrono::DateTime::parse_from_rfc3339(reset_at).is_err()
    {
        return Err(format!(
            "window {:?}: resetAt must be RFC3339 with a timezone",
            window.id
        ));
    }

    let unlimited = window.unlimited == Some(true);
    let percentage = window.used_percent.is_some() || window.remaining_percent.is_some();
    let counted = window.used.is_some()
        || window.total.is_some()
        || window.remaining.is_some()
        || window.unit.is_some();

    if unlimited {
        if percentage || counted {
            return Err(format!(
                "window {:?}: unlimited cannot carry numbers or a unit",
                window.id
            ));
        }
        return Ok(());
    }
    // An `unknown` window may carry no representation at all (the provider
    // does not expose it); the formatter skips it. An `ok`/`rate-limited`
    // window without numbers is a shape change.
    if !percentage && !counted {
        if window.status == UsageWindowStatus::Unknown {
            return Ok(());
        }
        return Err(format!(
            "window {:?}: expected a percentage, counted or unlimited representation",
            window.id
        ));
    }
    if percentage && counted {
        return Err(format!(
            "window {:?}: percentage and counted representations are mutually exclusive",
            window.id
        ));
    }
    if percentage {
        let used = window.used_percent.ok_or_else(|| {
            format!(
                "window {:?}: usedPercent is required for a percentage window",
                window.id
            )
        })?;
        validate_percentage(used, &window.id)?;
        match window.remaining_percent {
            Some(remaining) => {
                validate_percentage(remaining, &window.id)?;
                if (used + remaining - 100.0).abs() > 1.0 {
                    return Err(format!(
                        "window {:?}: usedPercent + remainingPercent must be within 1 point of 100",
                        window.id
                    ));
                }
            }
            None => window.remaining_percent = Some(100.0 - used),
        }
        return Ok(());
    }
    if counted {
        let unit = window
            .unit
            .as_deref()
            .map(str::trim)
            .filter(|unit| !unit.is_empty())
            .ok_or_else(|| {
                format!(
                    "window {:?}: a counted window requires a non-empty unit",
                    window.id
                )
            })?;
        validate_amounts(window.used, window.total, window.remaining, &window.id)?;
        if window.used.is_none() && window.total.is_none() && window.remaining.is_none() {
            return Err(format!(
                "window {:?}: a counted window requires used, total or remaining",
                window.id
            ));
        }
        let tolerance = count_tolerance(unit);
        let (used, remaining) = derive_sides(
            window.used,
            window.total,
            window.remaining,
            tolerance,
            &window.id,
        )?;
        window.used = used;
        window.remaining = remaining;
        return Ok(());
    }
    Err(format!(
        "window {:?}: expected a percentage, counted or unlimited representation",
        window.id
    ))
}

fn validate_ledger(ledger: &mut UsageLedger) -> Result<(), String> {
    if ledger.id.trim().is_empty() {
        return Err("ledger id must be a non-empty string".to_owned());
    }
    if ledger.label.trim().is_empty() {
        return Err(format!(
            "ledger {:?}: label must be a non-empty string",
            ledger.id
        ));
    }
    let unit = ledger.unit.trim().to_owned();
    if unit.is_empty() {
        return Err(format!(
            "ledger {:?}: unit must be a non-empty string",
            ledger.id
        ));
    }
    if ledger.kind.is_monetary()
        && ledger
            .currency
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        return Err(format!(
            "ledger {:?}: a monetary ledger requires a currency",
            ledger.id
        ));
    }
    if ledger
        .currency
        .as_deref()
        .is_some_and(|currency| currency.trim().is_empty())
    {
        return Err(format!(
            "ledger {:?}: currency must be a non-empty string when present",
            ledger.id
        ));
    }
    if ledger.used.is_none() && ledger.total.is_none() && ledger.remaining.is_none() {
        return Err(format!(
            "ledger {:?}: expected used, total or remaining",
            ledger.id
        ));
    }
    validate_amounts(ledger.used, ledger.total, ledger.remaining, &ledger.id)?;
    let tolerance = count_tolerance(&unit);
    let (used, remaining) = derive_sides(
        ledger.used,
        ledger.total,
        ledger.remaining,
        tolerance,
        &ledger.id,
    )?;
    ledger.used = used;
    ledger.remaining = remaining;
    Ok(())
}

fn validate_percentage(value: f64, id: &str) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(format!(
            "window {id:?}: percentages must be finite and within 0-100"
        ));
    }
    Ok(())
}

fn validate_amounts(
    used: Option<f64>,
    total: Option<f64>,
    remaining: Option<f64>,
    id: &str,
) -> Result<(), String> {
    for (name, value) in [("used", used), ("total", total), ("remaining", remaining)] {
        if let Some(value) = value
            && (!value.is_finite() || value < 0.0)
        {
            return Err(format!("{id:?}: {name} must be finite and non-negative"));
        }
    }
    if total.is_some_and(|total| total <= 0.0) {
        return Err(format!("{id:?}: total must be positive"));
    }
    Ok(())
}

/// Fill a missing used/remaining side from `total` and validate the sum when
/// both sides are present.
fn derive_sides(
    used: Option<f64>,
    total: Option<f64>,
    remaining: Option<f64>,
    tolerance: f64,
    id: &str,
) -> Result<(Option<f64>, Option<f64>), String> {
    let Some(total) = total else {
        return Ok((used, remaining));
    };
    match (used, remaining) {
        (Some(used), None) => {
            let derived = total - used;
            if derived < -tolerance {
                return Err(format!("{id:?}: used exceeds total"));
            }
            Ok((Some(used), Some(derived.max(0.0))))
        }
        (None, Some(remaining)) => {
            let derived = total - remaining;
            if derived < -tolerance {
                return Err(format!("{id:?}: remaining exceeds total"));
            }
            Ok((Some(derived.max(0.0)), Some(remaining)))
        }
        (Some(used), Some(remaining)) => {
            if (used + remaining - total).abs() > tolerance {
                return Err(format!(
                    "{id:?}: used + remaining must match total within the tolerance"
                ));
            }
            Ok((Some(used), Some(remaining)))
        }
        (None, None) => Ok((None, None)),
    }
}

/// Counting tolerance: currency-style units round at cents, everything else
/// at one whole unit.
fn count_tolerance(unit: &str) -> f64 {
    let unit = unit.trim();
    if unit.len() == 3 && unit.chars().all(|character| character.is_ascii_uppercase()) {
        0.01
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(stdout: &str) -> Result<UsageEnvelope, String> {
        parse_usage_envelope(stdout)
    }

    #[test]
    fn parses_a_full_envelope() {
        let envelope = parse(
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
            parse(r#"{"schemaVersion":1,"provider":"x","displayText":"x: ok"}"#).expect("minimal");
        assert!(envelope.plan.is_none());
        assert!(envelope.balance.is_none());
        assert!(envelope.windows.is_none());
        assert!(envelope.ledgers.is_none());
        assert!(envelope.quota.is_none());
        assert!(envelope.used.is_none());
        assert!(envelope.reset_at.is_none());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse(r#"{"provider":"x","displayText":"ok"}"#).is_err());
        assert!(parse(r#"{"schemaVersion":1,"displayText":"ok"}"#).is_err());
        assert!(parse(r#"{"schemaVersion":1,"provider":"x"}"#).is_err());
        assert!(parse(r#"{"schemaVersion":1,"provider":"x","displayText":" "}"#).is_err());
    }

    #[test]
    fn rejects_wrong_types_and_schema_versions() {
        assert!(parse(r#"{"schemaVersion":"1","provider":"x","displayText":"ok"}"#).is_err());
        assert!(parse(r#"{"schemaVersion":2,"provider":"x","displayText":"ok"}"#).is_err());
        assert!(
            parse(r#"{"schemaVersion":1,"provider":"x","displayText":"ok","used":"many"}"#)
                .is_err()
        );
        assert!(parse("not json").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn error_envelope_wins_over_missing_display_text() {
        let error = parse_error_envelope(
            r#"{"schemaVersion":1,"provider":"x","error":{"kind":"unauthorized"}}"#,
        )
        .expect("error envelope");
        assert_eq!(error.kind, UsageErrorKind::Unauthorized);
        assert_eq!(error.action, UsageErrorKind::Unauthorized.action());
        assert!(parse(r#"{"schemaVersion":1,"provider":"x","error":{"kind":"timeout"}}"#).is_err());

        let shape =
            parse_error_envelope(r#"{"schemaVersion":1,"provider":"x","error":{"kind":"nope"}}"#)
                .expect("unknown kind");
        assert_eq!(shape.kind, UsageErrorKind::ShapeChanged);
        assert!(parse_error_envelope(r#"{"schemaVersion":1,"provider":"x","error":{}}"#).is_some());
        assert!(
            parse_error_envelope(r#"{"schemaVersion":1,"provider":"x","error":null}"#).is_none()
        );
        assert!(parse_error_envelope(r#"{"schemaVersion":1,"provider":"x"}"#).is_none());
    }

    #[test]
    fn percentage_windows_validate_and_derive_remaining() {
        let mut envelope = parse(
            r#"{
                "schemaVersion":1,"provider":"x","displayText":"x",
                "windows":[{"id":"five_hour","label":"5h","status":"ok","usedPercent":39}]
            }"#,
        )
        .expect("percentage window");
        let window = &mut envelope.windows.as_mut().unwrap()[0];
        assert_eq!(window.remaining_percent, Some(61.0));

        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10,"remainingPercent":90}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10,"remainingPercent":88}]}"#
            )
            .is_err(),
            "one-point tolerance is inclusive; two points is not"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":101}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","remainingPercent":50}]}"#
            )
            .is_err(),
            "usedPercent is required"
        );
    }

    #[test]
    fn counted_and_unlimited_windows() {
        let mut envelope = parse(
            r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                {"id":"requests","label":"5h","status":"ok","used":12,"total":30,"unit":"requests"}]}"#,
        )
        .expect("counted window");
        assert_eq!(envelope.windows.as_mut().unwrap()[0].remaining, Some(18.0));

        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","remaining":18,"total":30,"unit":"requests"}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","total":30,"unit":"requests"}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","used":12,"total":30,"unit":"requests"}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","used":12,"remaining":18,"unit":"requests"}]}"#
            )
            .is_ok(),
            "used+remaining without total is allowed"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","used":12,"remaining":18,"total":30,"unit":"requests"}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"requests","label":"5h","status":"ok","used":12,"remaining":20,"total":30,"unit":"requests"}]}"#
            )
            .is_err(),
            "sum mismatch beyond tolerance"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"usd","label":"60d","status":"ok","used":3,"total":50,"remaining":46.99,"unit":"USD"}]}"#
            )
            .is_ok(),
            "currency tolerance is one cent"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"usd","label":"60d","status":"ok","used":3,"total":50,"remaining":46.98,"unit":"USD"}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"u","label":"u","status":"ok","unlimited":true}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"u","label":"u","status":"ok","unlimited":true,"used":1}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"u","label":"u","status":"ok"}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"u","label":"u","status":"unknown"}]}"#
            )
            .is_ok(),
            "an unknown window may carry no representation"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"u","label":"u","status":"rate-limited"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn window_status_and_fields_are_strict() {
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","usedPercent":10}]}"#
            )
            .is_err(),
            "missing status must be shape-changed"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"weird","usedPercent":10}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10,"used":1,"unit":"x"}]}"#
            )
            .is_err(),
            "mixed representations"
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","used":1,"unit":" "}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","used":1,"unit":"x","durationSeconds":0}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10,"resetAt":"2026-10-08"}]}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10,"resetAt":"2026-10-08T00:00:00+08:00"}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","windows":[
                    {"id":"a","label":"a","status":"ok","usedPercent":10},
                    {"id":"a","label":"b","status":"ok","usedPercent":10}]}"#
            )
            .is_err(),
            "duplicate window ids"
        );
    }

    #[test]
    fn ledgers_validate_kind_currency_and_sums() {
        let envelope = parse(
            r#"{"schemaVersion":1,"provider":"x","displayText":"x","ledgers":[
                {"id":"on_demand","label":"On-demand","kind":"on-demand","unit":"USD",
                 "currency":"USD","used":3,"total":50}]}"#,
        )
        .expect("ledger");
        let ledger = &envelope.ledgers.as_ref().unwrap()[0];
        assert_eq!(ledger.remaining, Some(47.0));

        for bad in [
            r#"{"id":"l","label":"l","kind":"nope","unit":"USD","currency":"USD","used":1}"#,
            r#"{"id":"l","label":"l","kind":"on-demand","unit":"USD","used":1}"#,
            r#"{"id":"l","label":"l","kind":"on-demand","unit":"","currency":"USD","used":1}"#,
            r#"{"id":"l","label":"l","kind":"on-demand","unit":"USD","currency":"USD"}"#,
            r#"{"id":"l","label":"l","kind":"on-demand","unit":"USD","currency":"USD","used":-1}"#,
            r#"{"id":"l","label":"l","kind":"on-demand","unit":"USD","currency":"USD","total":0}"#,
        ] {
            let stdout = format!(
                r#"{{"schemaVersion":1,"provider":"x","displayText":"x","ledgers":[{bad}]}}"#
            );
            assert!(parse(&stdout).is_err(), "{bad} must be rejected");
        }
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","ledgers":[
                    {"id":"q","label":"Quota","kind":"quota","unit":"requests","used":1}]}"#
            )
            .is_ok()
        );
        assert!(
            parse(
                r#"{"schemaVersion":1,"provider":"x","displayText":"x","ledgers":[
                    {"id":"l","label":"l","kind":"on-demand","unit":"USD","currency":"USD","used":1}]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn ignores_unknown_fields_and_never_serializes_secrets() {
        let envelope = parse(
            r#"{"schemaVersion":1,"provider":"x","displayText":"ok","apiKey":"sk-secret","token":"t"}"#,
        )
        .expect("unknown fields are ignored");
        let json = envelope.to_json();
        assert_eq!(json.get("apiKey"), None);
        assert_eq!(json.get("token"), None);
        assert_eq!(json.get("schemaVersion"), Some(&Value::from(1)));
    }
}
