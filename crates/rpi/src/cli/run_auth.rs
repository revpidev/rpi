//! `runAuthCommand` — port of the auth-command orchestration in
//! `packages/coding-agent/src/main.ts:139-214` @ 4181f66
//! (commit 99e34013d + a261366bd).
//!
//! Entry point for `rpi auth <subcommand>`. Dispatches to the parser
//! ([`super::auth_command`]), the check logic ([`super::auth_check`]), and
//! the credential-print logic ([`super::credential_print`]).
//!
//! Returns `Some(exit_code)` when the command was handled (caller exits with
//! that code), or `None` when `args[0]` is not `"auth"`.
//!
//! Intentional differences: none.

use std::sync::Arc;

use rpi_ai::auth::FileCredentialStore;

use crate::cli::args::parse_args;
use crate::cli::auth_check::{
    check_provider_auth, create_auth_check_model_runtime, get_provider_credential, AuthCheckResult,
};
use crate::cli::auth_command::{
    get_auth_command_name, get_auth_command_usage, is_auth_command_help, parse_auth_command,
    print_auth_command_help, AuthCommand, AuthCommandKind,
};
use crate::cli::credential_print::resolve_credential_for_print;
use crate::config::APP_NAME;

/// `runAuthCommand` (main.ts:139-214).
///
/// Returns `Some(exit_code)` when handled, `None` when `args[0] != "auth"`.
pub async fn run_auth(args: &[String]) -> Option<i32> {
    // Step 1: help (main.ts:140-143).
    if is_auth_command_help(args) {
        print!("{}", print_auth_command_help());
        return Some(0);
    }

    // Step 2: parse (main.ts:145-153).
    let command: AuthCommand = match parse_auth_command(args) {
        Ok(Some(command)) => command,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("Error: {error}");
            return Some(1);
        }
    };

    // Step 3: parse the remaining args (main.ts:156).
    let parsed = parse_args(&command.args);

    // Step 4: unknown flags check (main.ts:157-163).
    if !parsed.unknown_flags.is_empty() {
        let option = &parsed.unknown_flags[0].0;
        eprintln!(
            "Error: Unknown option --{option} for \"{}\".",
            get_auth_command_name(command.kind)
        );
        eprintln!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            get_auth_command_usage(command.kind)
        );
        return Some(1);
    }

    // Step 5: diagnostics (main.ts:165-167 + 212).
    // Upstream throws diagnostics into the outer catch — exit code is
    // command.kind === "check" ? 2 : 1.
    if !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("Error: {message}");
        return Some(if command.kind == AuthCommandKind::Check {
            2
        } else {
            1
        });
    }

    // Step 6: dispatch by kind (main.ts:168-213).
    match command.kind {
        AuthCommandKind::ApiKey | AuthCommandKind::BearerToken => {
            run_credential_print(&parsed, &command).await
        }
        AuthCommandKind::Check => run_check(&parsed, &command).await,
    }
}

/// Credential print path (main.ts:168-179).
async fn run_credential_print(
    parsed: &crate::cli::args::Args,
    command: &AuthCommand,
) -> Option<i32> {
    use crate::core::model_runtime::CreateModelRuntimeOptions;
    use crate::core::model_runtime::ModelsPathInput;

    let model_runtime =
        crate::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
            // print-api-key/print-bearer-token use the real auth.json (so
            // OAuth tokens can be refreshed/persisted), but no models.json
            // (upstream: `ModelRuntime.create({ allowModelNetwork: false })`,
            // main.ts:170).
            models_path: ModelsPathInput::Disabled,
            allow_model_network: false,
            ..Default::default()
        })
        .await;

    match resolve_credential_for_print(parsed, &model_runtime, command.kind, command.min_expiry_ms)
        .await
    {
        Ok(credential) => {
            // stdout is the credential body (legitimate — G4 does not apply
            // to print command stdout, only to logs/errors).
            println!("{credential}");
            Some(0)
        }
        Err(error) => {
            eprintln!("Error: {error}");
            Some(1)
        }
    }
}

/// Auth check path (main.ts:182-213).
async fn run_check(parsed: &crate::cli::args::Args, command: &AuthCommand) -> Option<i32> {
    // main.ts:182: validateAuthCommandArgs to get provider/model for the
    // fallback `invalid` result.
    let requested_auth =
        match crate::cli::auth_command::validate_auth_command_args(parsed, command.kind) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("Error: {error}");
                return Some(2); // check command error exit code (main.ts:212)
            }
        };

    // main.ts:186-187: create credential store + model runtime.
    // Upstream uses `ReadOnlyAuthStorage` for `--no-refresh` (throws on
    // modify/delete); in rpi we pass the same `FileCredentialStore` — the
    // `--no-refresh` path in `getProviderCredential` skips the refresh
    // branch, so no write occurs.
    let credentials: Arc<dyn rpi_ai::auth::CredentialStore> = Arc::new(FileCredentialStore::new(
        crate::config::get_agent_dir().join("auth.json"),
    ));
    let model_runtime = create_auth_check_model_runtime(credentials.clone()).await;

    // main.ts:188-203: run the check, handle errors → invalid.
    let (result, credential): (AuthCheckResult, Option<String>) =
        match check_provider_auth(parsed, &model_runtime, !command.no_refresh).await {
            Ok(result) => {
                if command.credentials && result.status == super::auth_check::AuthCheckStatus::Ready
                {
                    match get_provider_credential(
                        &result.provider,
                        &model_runtime,
                        &credentials,
                        !command.no_refresh,
                    )
                    .await
                    {
                        Ok(Some(cred)) => (result, Some(cred)),
                        // main.ts:193-195: credential not available → not_ready.
                        Ok(None) => (
                            AuthCheckResult {
                                status: super::auth_check::AuthCheckStatus::NotReady,
                                provider: result.provider.clone(),
                                reason: Some(
                                    super::auth_check::AuthCheckReason::CredentialNotAvailable,
                                ),
                                auth_type: None,
                            },
                            None,
                        ),
                        Err(_) => (
                            // main.ts:197-203: inner catch — any thrown error
                            // (read failure, OAuth refresh failure, …) maps to
                            // invalid/invalid_state (exit 2), NOT not_ready.
                            AuthCheckResult {
                                status: super::auth_check::AuthCheckStatus::Invalid,
                                provider: result.provider.clone(),
                                reason: Some(super::auth_check::AuthCheckReason::InvalidState),
                                auth_type: None,
                            },
                            None,
                        ),
                    }
                } else {
                    (result, None)
                }
            }
            Err(_error) => {
                // main.ts:197-203: any error → invalid.
                let provider = requested_auth.0.or(requested_auth.1).unwrap_or_default();
                let result = AuthCheckResult {
                    status: super::auth_check::AuthCheckStatus::Invalid,
                    provider,
                    reason: Some(super::auth_check::AuthCheckReason::InvalidState),
                    auth_type: None,
                };
                (result, None)
            }
        };

    // main.ts:204-207: format output.
    let output = if command.json {
        format_json_output(&result, credential.as_deref())
    } else {
        credential.unwrap_or_else(|| result.status.as_str().to_owned())
    };
    println!("{output}");

    // main.ts:208: exit code.
    Some(result.status.exit_code())
}

/// Format the JSON output for `auth check --json` (main.ts:204-205).
fn format_json_output(result: &AuthCheckResult, credential: Option<&str>) -> String {
    // Build the JSON manually to control field order and avoid pulling in a
    // serde derive on AuthCheckResult (which is a CLI-local type). The output
    // shape matches upstream: `{status, provider, reason?, authType?, credentials?}`.
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("\"status\":\"{}\"", result.status.as_str()));
    parts.push(format!(
        "\"provider\":\"{}\"",
        json_escape(&result.provider)
    ));
    if let Some(reason) = &result.reason {
        parts.push(format!("\"reason\":\"{}\"", reason.as_str()));
    }
    if let Some(auth_type) = &result.auth_type {
        parts.push(format!("\"authType\":\"{auth_type}\""));
    }
    if let Some(credential) = credential {
        parts.push(format!("\"credentials\":\"{}\"", json_escape(credential)));
    }
    format!("{{{}}}", parts.join(","))
}

/// Minimal JSON string escaper for the credential/provider values.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
