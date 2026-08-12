//! Port of `packages/coding-agent/src/cli/auth-command.ts` @ 4181f66
//! (commit 99e34013d: print-api-key/print-bearer-token #7168; a261366bd:
//! auth check).
//!
//! Auth command parsing, help text, and arg validation. The actual auth
//! resolution (check / credential print) lives in [`super::auth_check`] and
//! [`super::credential_print`].
//!
//! Intentional differences: none.

use crate::cli::args::Args;

/// `AuthCommandKind` (auth-command.ts:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCommandKind {
    Check,
    ApiKey,
    BearerToken,
}

/// `AuthCommand` (auth-command.ts:6-13).
#[derive(Debug, Clone)]
pub struct AuthCommand {
    pub kind: AuthCommandKind,
    /// Remaining args after subcommand keyword (passed to `parse_args`).
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<u64>,
}

/// `AuthCommandError` (auth-command.ts:15).
#[derive(Debug, Clone)]
pub struct AuthCommandError(pub String);

impl std::fmt::Display for AuthCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthCommandError {}

/// `AUTH_COMMAND_USAGE` (auth-command.ts:17-21).
const AUTH_CHECK_USAGE: &str =
    "rpi auth check --provider <provider> [--json] [--credentials] [--no-refresh]";
const AUTH_API_KEY_USAGE: &str = "rpi auth print-api-key --provider <provider> [--model <model>]";
const AUTH_BEARER_TOKEN_USAGE: &str =
    "rpi auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]";

/// `getAuthCommandName` (auth-command.ts:23-25).
pub fn get_auth_command_name(kind: AuthCommandKind) -> &'static str {
    match kind {
        AuthCommandKind::Check => "auth check",
        AuthCommandKind::ApiKey => "auth print-api-key",
        AuthCommandKind::BearerToken => "auth print-bearer-token",
    }
}

/// `getAuthCommandUsage` (auth-command.ts:27-29).
pub fn get_auth_command_usage(kind: AuthCommandKind) -> &'static str {
    match kind {
        AuthCommandKind::Check => AUTH_CHECK_USAGE,
        AuthCommandKind::ApiKey => AUTH_API_KEY_USAGE,
        AuthCommandKind::BearerToken => AUTH_BEARER_TOKEN_USAGE,
    }
}

/// `isAuthCommandHelp` (auth-command.ts:31-36).
pub fn is_auth_command_help(args: &[String]) -> bool {
    args.first().is_some_and(|first| first == "auth")
        && (args.len() == 1
            || args.get(1).is_some_and(|second| second == "help")
            || args.iter().any(|a| a == "--help" || a == "-h"))
}

/// `printAuthCommandHelp` (auth-command.ts:38-45).
pub fn print_auth_command_help() -> String {
    format!(
        "Usage:\n  \
         {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]\n  \
         {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]\n  \
         {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]\n\n\
         Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; \
         --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.\n",
        APP_NAME = crate::config::APP_NAME,
    )
}

/// Parse a `--min-expiry <duration>` value (auth-command.ts:74-79).
///
/// Format: `<number>(ms|s|m|h)`, case-insensitive.
fn parse_min_expiry(value: &str) -> Result<u64, AuthCommandError> {
    // Upstream regex: /^(\d+)(ms|s|m|h)$/iu
    // Find the boundary between digits and unit.
    let split = value
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i);
    let Some(split) = split else {
        return Err(AuthCommandError(
            "--min-expiry must use a duration such as 30m or 1h".to_owned(),
        ));
    };
    let (amount_str, unit) = value.split_at(split);
    let amount: u64 = amount_str.parse().map_err(|_| {
        AuthCommandError("--min-expiry must use a duration such as 30m or 1h".to_owned())
    })?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => {
            return Err(AuthCommandError(
                "--min-expiry must use a duration such as 30m or 1h".to_owned(),
            ));
        }
    };
    Ok(amount.saturating_mul(multiplier))
}

/// `parseAuthCommand` (auth-command.ts:47-95).
///
/// `args[0]` must be `"auth"`. Returns `None` when `args[0] != "auth"`.
pub fn parse_auth_command(args: &[String]) -> Result<Option<AuthCommand>, AuthCommandError> {
    if args.first().is_none_or(|first| first != "auth") {
        return Ok(None);
    }
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        _ => {
            let sub = args.get(1).cloned().unwrap_or_default();
            return Err(AuthCommandError(format!(
                "Unknown auth command \"{sub}\". Use \"rpi auth print-api-key\", \"rpi auth print-bearer-token\", or \"rpi auth check\"."
            )));
        }
    };

    let mut command_args: Vec<String> = Vec::new();
    let mut json = false;
    let mut credentials = false;
    let mut no_refresh = false;
    let mut min_expiry_ms: Option<u64> = None;

    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--min-expiry" {
            if kind != AuthCommandKind::BearerToken {
                return Err(AuthCommandError(
                    "--min-expiry is only supported by print-bearer-token".to_owned(),
                ));
            }
            index += 1;
            let value = args.get(index).map(String::as_str).unwrap_or("");
            min_expiry_ms = Some(parse_min_expiry(value)?);
        } else if arg == "--json" || arg == "--credentials" || arg == "--no-refresh" {
            if kind != AuthCommandKind::Check {
                return Err(AuthCommandError(format!(
                    "{arg} is only supported by auth check"
                )));
            }
            match arg.as_str() {
                "--json" => json = true,
                "--credentials" => credentials = true,
                "--no-refresh" => no_refresh = true,
                _ => {}
            }
        } else {
            command_args.push(arg.clone());
        }
        index += 1;
    }

    Ok(Some(AuthCommand {
        kind,
        args: command_args,
        json,
        credentials,
        no_refresh,
        min_expiry_ms,
    }))
}

/// `validateAuthCommandArgs` (auth-command.ts:97-117).
///
/// Returns `(provider, model)` after validating the parsed `Args`. On error,
/// returns `Err(AuthCommandError)`.
pub fn validate_auth_command_args(
    parsed: &Args,
    kind: AuthCommandKind,
) -> Result<(Option<String>, Option<String>), AuthCommandError> {
    let provider = parsed
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let model = parsed
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if !parsed.unknown_flags.is_empty() {
        let option = &parsed.unknown_flags[0].0;
        return Err(AuthCommandError(format!(
            "Unknown option --{option} for \"{}\".",
            get_auth_command_name(kind)
        )));
    }
    if parsed.api_key.is_some() || !parsed.messages.is_empty() || !parsed.file_args.is_empty() {
        return Err(AuthCommandError(
            "Auth commands only accept --provider and --model".to_owned(),
        ));
    }
    if provider.is_none() && model.is_none() {
        let msg = if kind == AuthCommandKind::Check {
            "Auth checks require --provider <provider> or --model <model>"
        } else {
            "Credential printing requires --provider <provider> or --model <model>"
        };
        return Err(AuthCommandError(msg.to_owned()));
    }
    Ok((provider, model))
}

/// `getAuthCredential` (auth-command.ts:119-124): extract the secret value
/// from an `AuthResult` — API key first, then `Bearer <token>` from headers.
pub fn get_auth_credential(auth: &rpi_ai::auth::AuthResult) -> Option<String> {
    if let Some(api_key) = &auth.auth.api_key {
        return Some(api_key.clone());
    }
    let headers = auth.auth.headers.as_ref()?;
    // Find the Authorization header (case-insensitive), extract Bearer value.
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.as_ref())
        .and_then(|value| {
            // /^Bearer\s+(.+)$/iu
            let bearer_prefix = value
                .strip_prefix("Bearer")
                .or_else(|| value.strip_prefix("bearer"))?;
            let token = bearer_prefix.strip_prefix(' ').or_else(|| {
                // Try other whitespace (tab etc.) — upstream regex uses \s+.
                bearer_prefix.strip_prefix('\t')
            })?;
            if token.is_empty() {
                None
            } else {
                Some(token.to_owned())
            }
        })
}

#[cfg(test)]
mod tests {
    //! Port of the parser / validation / help tests for auth commands
    //! (auth-command.ts:125 lines; tests follow the same structure as the
    //! upstream vitest suite, plus the hand-rolled parser convention of the
    //! `rpi` CLI).

    use super::*;
    use crate::cli::args::parse_args;

    fn owned(input: &[&str]) -> Vec<String> {
        input.iter().map(|s| s.to_string()).collect()
    }

    // ---- is_auth_command_help ----

    #[test]
    fn test_help_bare_auth() {
        assert!(is_auth_command_help(&owned(&["auth"])));
    }

    #[test]
    fn test_help_auth_help_keyword() {
        assert!(is_auth_command_help(&owned(&["auth", "help"])));
    }

    #[test]
    fn test_help_with_help_flag() {
        assert!(is_auth_command_help(&owned(&["auth", "check", "--help"])));
        assert!(is_auth_command_help(&owned(&[
            "auth",
            "print-api-key",
            "-h"
        ])));
    }

    #[test]
    fn test_help_not_auth() {
        assert!(!is_auth_command_help(&owned(&["config"])));
        assert!(!is_auth_command_help(&owned(&[])));
    }

    // ---- parse_auth_command ----

    #[test]
    fn test_parse_returns_none_for_non_auth() {
        let result = parse_auth_command(&owned(&["config"])).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_unknown_subcommand_errors() {
        let err = parse_auth_command(&owned(&["auth", "bogus"])).unwrap_err();
        assert!(err.0.contains("Unknown auth command"));
        assert!(err.0.contains("bogus"));
    }

    #[test]
    fn test_parse_unknown_subcommand_empty() {
        let err = parse_auth_command(&owned(&["auth"])).unwrap_err();
        assert!(err.0.contains("Unknown auth command"));
    }

    #[test]
    fn test_parse_check_subcommand() {
        let cmd = parse_auth_command(&owned(&["auth", "check", "--provider", "openai"]))
            .unwrap()
            .unwrap();
        assert_eq!(cmd.kind, AuthCommandKind::Check);
        assert_eq!(cmd.args, vec!["--provider", "openai"]);
        assert!(!cmd.json);
        assert!(!cmd.credentials);
        assert!(!cmd.no_refresh);
        assert_eq!(cmd.min_expiry_ms, None);
    }

    #[test]
    fn test_parse_print_api_key_subcommand() {
        let cmd = parse_auth_command(&owned(&["auth", "print-api-key", "--provider", "openai"]))
            .unwrap()
            .unwrap();
        assert_eq!(cmd.kind, AuthCommandKind::ApiKey);
        assert_eq!(cmd.args, vec!["--provider", "openai"]);
    }

    #[test]
    fn test_parse_print_bearer_token_subcommand() {
        let cmd = parse_auth_command(&owned(&[
            "auth",
            "print-bearer-token",
            "--provider",
            "openai-codex",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cmd.kind, AuthCommandKind::BearerToken);
        assert_eq!(cmd.args, vec!["--provider", "openai-codex"]);
    }

    // ---- flag scoping ----

    #[test]
    fn test_min_expiry_only_on_bearer_token() {
        let cmd = parse_auth_command(&owned(&[
            "auth",
            "print-bearer-token",
            "--min-expiry",
            "30m",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cmd.min_expiry_ms, Some(30 * 60_000));

        let err = parse_auth_command(&owned(&["auth", "print-api-key", "--min-expiry", "30m"]))
            .unwrap_err();
        assert!(err
            .0
            .contains("--min-expiry is only supported by print-bearer-token"));

        let err =
            parse_auth_command(&owned(&["auth", "check", "--min-expiry", "30m"])).unwrap_err();
        assert!(err
            .0
            .contains("--min-expiry is only supported by print-bearer-token"));
    }

    #[test]
    fn test_check_flags_only_on_check() {
        let cmd = parse_auth_command(&owned(&[
            "auth",
            "check",
            "--json",
            "--credentials",
            "--no-refresh",
            "--provider",
            "openai",
        ]))
        .unwrap()
        .unwrap();
        assert!(cmd.json);
        assert!(cmd.credentials);
        assert!(cmd.no_refresh);

        for flag in ["--json", "--credentials", "--no-refresh"] {
            let err = parse_auth_command(&owned(&[
                "auth",
                "print-api-key",
                flag,
                "--provider",
                "openai",
            ]))
            .unwrap_err();
            assert!(
                err.0.contains("is only supported by auth check"),
                "flag {flag} should be check-only"
            );
        }
    }

    // ---- min-expiry duration parsing ----

    #[test]
    fn test_min_expiry_duration_formats() {
        let parse_val = |s: &str| {
            parse_auth_command(&owned(&["auth", "print-bearer-token", "--min-expiry", s]))
                .unwrap()
                .unwrap()
                .min_expiry_ms
                .unwrap()
        };
        assert_eq!(parse_val("500ms"), 500);
        assert_eq!(parse_val("30s"), 30_000);
        assert_eq!(parse_val("5m"), 300_000);
        assert_eq!(parse_val("1h"), 3_600_000);
        // case-insensitive
        assert_eq!(parse_val("5M"), 300_000);
        assert_eq!(parse_val("1H"), 3_600_000);
        assert_eq!(parse_val("500MS"), 500);
        assert_eq!(parse_val("30S"), 30_000);
    }

    #[test]
    fn test_min_expiry_invalid_duration() {
        for bad in ["abc", "30", "30x", "", "-5m", "5min"] {
            let result =
                parse_auth_command(&owned(&["auth", "print-bearer-token", "--min-expiry", bad]));
            assert!(result.is_err(), "expected error for --min-expiry \"{bad}\"");
            let err = result.unwrap_err();
            assert!(
                err.0.contains("--min-expiry must use a duration"),
                "error should explain format: got \"{}\" for \"{bad}\"",
                err.0
            );
        }
    }

    #[test]
    fn test_min_expiry_missing_value() {
        let result = parse_auth_command(&owned(&["auth", "print-bearer-token", "--min-expiry"]));
        assert!(result.is_err());
    }

    // ---- validate_auth_command_args ----

    #[test]
    fn test_validate_extracts_provider_and_model() {
        let args = parse_args(&owned(&["--provider", "openai", "--model", "gpt-4o"]));
        let (provider, model) = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap();
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn test_validate_trims_provider_and_model() {
        let args = parse_args(&owned(&["--provider", "  openai  ", "--model", "  "]));
        let (provider, model) = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap();
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model, None);
    }

    #[test]
    fn test_validate_rejects_unknown_flag() {
        let args = parse_args(&owned(&["--provider", "openai", "--bogus", "value"]));
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.0.contains("Unknown option --bogus"));
    }

    #[test]
    fn test_validate_rejects_api_key() {
        let args = parse_args(&owned(&["--provider", "openai", "--api-key", "sk-xxx"]));
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.0.contains("Auth commands only accept"));
    }

    #[test]
    fn test_validate_rejects_messages() {
        let args = parse_args(&owned(&["hello", "world"]));
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.0.contains("Auth commands only accept"));
    }

    #[test]
    fn test_validate_rejects_file_args() {
        let args = parse_args(&owned(&["@file.txt"]));
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.0.contains("Auth commands only accept"));
    }

    #[test]
    fn test_validate_requires_provider_or_model_check() {
        let args = parse_args(&owned(&[]));
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.0.contains("Auth checks require"));
    }

    #[test]
    fn test_validate_requires_provider_or_model_print() {
        let args = parse_args(&owned(&[]));
        let err = validate_auth_command_args(&args, AuthCommandKind::ApiKey).unwrap_err();
        assert!(err.0.contains("Credential printing requires"));
    }

    // ---- help text ----

    #[test]
    fn test_help_text_contains_all_subcommands() {
        let help = print_auth_command_help();
        assert!(help.contains("print-api-key"));
        assert!(help.contains("print-bearer-token"));
        assert!(help.contains("auth check"));
        assert!(help.contains("--min-expiry"));
        assert!(help.contains("--json"));
        assert!(help.contains("--credentials"));
        assert!(help.contains("--no-refresh"));
    }

    #[test]
    fn test_command_name_and_usage() {
        assert_eq!(get_auth_command_name(AuthCommandKind::Check), "auth check");
        assert_eq!(
            get_auth_command_name(AuthCommandKind::ApiKey),
            "auth print-api-key"
        );
        assert_eq!(
            get_auth_command_name(AuthCommandKind::BearerToken),
            "auth print-bearer-token"
        );
        assert!(get_auth_command_usage(AuthCommandKind::Check).contains("--json"));
        assert!(get_auth_command_usage(AuthCommandKind::BearerToken).contains("--min-expiry"));
    }

    // ---- get_auth_credential ----

    #[test]
    fn test_get_auth_credential_api_key() {
        use rpi_ai::auth::{AuthResult, ModelAuth};
        let auth = AuthResult {
            auth: ModelAuth {
                api_key: Some("sk-test-key".to_owned()),
                headers: None,
                base_url: None,
            },
            env: None,
            source: None,
        };
        assert_eq!(get_auth_credential(&auth), Some("sk-test-key".to_owned()));
    }

    #[test]
    fn test_get_auth_credential_bearer_header() {
        use rpi_ai::auth::{AuthResult, ModelAuth};
        use rpi_ai::types::ProviderHeaders;
        let mut headers = ProviderHeaders::new();
        headers.insert(
            "Authorization".to_owned(),
            Some("Bearer my-token-123".to_owned()),
        );
        let auth = AuthResult {
            auth: ModelAuth {
                api_key: None,
                headers: Some(headers),
                base_url: None,
            },
            env: None,
            source: None,
        };
        assert_eq!(get_auth_credential(&auth), Some("my-token-123".to_owned()));
    }

    #[test]
    fn test_get_auth_credential_no_secret() {
        use rpi_ai::auth::{AuthResult, ModelAuth};
        let auth = AuthResult {
            auth: ModelAuth {
                api_key: None,
                headers: None,
                base_url: None,
            },
            env: None,
            source: None,
        };
        assert_eq!(get_auth_credential(&auth), None);
    }
}
