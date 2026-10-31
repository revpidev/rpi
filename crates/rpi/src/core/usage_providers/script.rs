//! Script execution for the usage-provider framework (V16-05 FR-A R2/R3;
//! V18-10 error classification).
//!
//! Runs one provider script through the shared exec channel
//! ([`crate::core::extension_actions::exec_script`]): the context JSON goes
//! to stdin, the credential reaches the child only through its environment,
//! stdout is capped, and a timeout kills and reaps the process.
//!
//! A script may report a classified failure as `exit 2` plus
//! `{schemaVersion:1,provider,error:{kind}}`; the error envelope is
//! recognized before the normal envelope's required fields. Other exits,
//! malformed output, timeouts and spawn failures map to the fixed
//! [`UsageErrorKind`] vocabulary. Diagnostics never include stdout/stderr.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::identity::UsageCredentialKind;
use super::{
    UsageEnvelope, UsageError, UsageErrorKind, parse_error_envelope, parse_usage_envelope,
};
use crate::core::extension_actions::{ScriptExecRequest, exec_script};

pub use super::identity::{USAGE_OAUTH_ACCESS_ENV, USAGE_OAUTH_ACCOUNT_ID_ENV};

/// stdin context JSON (pinned; `apiKeyEnv` names the variable, never its
/// value — the value only ever crosses through the child environment).
/// `credentialKind` is additive in V18-10 and absent for legacy scripts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageScriptContext {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_kind: Option<UsageCredentialKind>,
}

/// Credential environment additions for one script run.
pub type UsageScriptEnv = Vec<(String, String)>;

/// Environment variable the provider scripts read for their in-script
/// request timeouts. The host injects the same budget it enforces, so a
/// script's degradation path can finish before the process is killed
/// (v0.1.6 review round 2: the built-in scripts default to 8s while the
/// host kills at the 3s default, making their fallbacks unreachable).
pub const USAGE_TIMEOUT_ENV: &str = "RPI_USAGE_TIMEOUT_MS";

/// One classified script failure. `detail` is a fixed diagnostic category
/// (never stdout/stderr or a credential) for debug logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageScriptError {
    pub error: UsageError,
    pub detail: String,
}

impl UsageScriptError {
    pub fn new(kind: UsageErrorKind, detail: impl Into<String>) -> Self {
        Self {
            error: UsageError::new(kind),
            detail: detail.into(),
        }
    }
}

/// Best-effort API-key env var name for `provider`: the model catalog's
/// conventional variable when the usage provider id matches a provider id
/// (`deepseek` → `DEEPSEEK_API_KEY`). Custom ids rely on the explicit
/// `usage.providers` object form (`apiKeyEnv`) or on the script knowing its
/// own variable (the built-in rpi-usage scripts do).
pub fn usage_api_key_env(provider: &str) -> Option<String> {
    rpi_ai::auth::api_key_env_vars(provider)
        .and_then(|vars| vars.first())
        .map(|name| (*name).to_owned())
}

/// Run `script_path` and return its validated envelope. Any failure
/// (spawn/timeout/overflow/non-zero exit/invalid envelope/error envelope) is
/// a classified [`UsageScriptError`]; the reason never includes stdout or
/// stderr content.
pub async fn execute_usage_script(
    script_path: &Path,
    cwd: &str,
    context: &UsageScriptContext,
    timeout_ms: u64,
    env: &UsageScriptEnv,
) -> Result<UsageEnvelope, UsageScriptError> {
    let stdin = serde_json::to_string(context).map_err(|_| {
        UsageScriptError::new(UsageErrorKind::ShapeChanged, "context serialization")
    })?;
    let command = script_path.to_string_lossy().into_owned();
    // The script's own timeout budget mirrors the enforced one; a caller
    // cannot override it (the value is authoritative for the process kill).
    let mut env: UsageScriptEnv = env
        .iter()
        .filter(|(name, _)| name != USAGE_TIMEOUT_ENV)
        .cloned()
        .collect();
    env.push((USAGE_TIMEOUT_ENV.to_owned(), timeout_ms.to_string()));
    let outcome = exec_script(ScriptExecRequest {
        command: &command,
        args: &[],
        cwd,
        timeout_ms: Some(timeout_ms),
        stdin: Some(&stdin),
        env: &env,
        max_stdout_bytes: Some(super::MAX_USAGE_STDOUT_BYTES),
    })
    .await;
    if outcome.spawn_failed {
        return Err(UsageScriptError::new(
            UsageErrorKind::RuntimeUnavailable,
            "script could not be spawned",
        ));
    }
    if outcome.killed {
        return Err(UsageScriptError::new(
            UsageErrorKind::Timeout,
            "script timed out",
        ));
    }
    if outcome.stdout_overflow {
        return Err(UsageScriptError::new(
            UsageErrorKind::ShapeChanged,
            "stdout exceeded the limit",
        ));
    }
    // The script's classified failure envelope wins over the exit code and
    // over the normal envelope's required fields.
    if let Some(error) = parse_error_envelope(&outcome.stdout) {
        return Err(UsageScriptError {
            error,
            detail: "script reported a classified failure".to_owned(),
        });
    }
    if outcome.code != 0 {
        let kind = if outcome.code == 2 {
            UsageErrorKind::ShapeChanged
        } else {
            UsageErrorKind::Transport
        };
        return Err(UsageScriptError::new(
            kind,
            format!("script exited with code {}", outcome.code),
        ));
    }
    parse_usage_envelope(&outcome.stdout)
        .map_err(|_| UsageScriptError::new(UsageErrorKind::ShapeChanged, "invalid usage envelope"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_serializes_camel_case_and_omits_absent_fields() {
        let context = UsageScriptContext {
            provider: "deepseek".to_owned(),
            base_url: None,
            api_key_env: Some("DEEPSEEK_API_KEY".to_owned()),
            model: None,
            credential_kind: Some(UsageCredentialKind::ApiKey),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&context).unwrap()).unwrap();
        assert_eq!(json.get("provider"), Some(&serde_json::json!("deepseek")));
        assert_eq!(
            json.get("apiKeyEnv"),
            Some(&serde_json::json!("DEEPSEEK_API_KEY"))
        );
        assert_eq!(
            json.get("credentialKind"),
            Some(&serde_json::json!("api-key"))
        );
        assert_eq!(json.get("baseUrl"), None);
        assert_eq!(json.get("model"), None);

        let oauth = UsageScriptContext {
            credential_kind: Some(UsageCredentialKind::Oauth),
            ..UsageScriptContext::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&oauth).unwrap()).unwrap();
        assert_eq!(
            json.get("credentialKind"),
            Some(&serde_json::json!("oauth"))
        );
    }

    #[test]
    fn known_provider_ids_resolve_their_env_var() {
        assert_eq!(
            usage_api_key_env("deepseek").as_deref(),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(
            usage_api_key_env("kimi-coding").as_deref(),
            Some("KIMI_API_KEY")
        );
        assert_eq!(usage_api_key_env("custom-usage-provider"), None);
    }
}
