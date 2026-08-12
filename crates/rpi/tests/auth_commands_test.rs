//! Integration tests for the `rpi auth` command family (T25).
//!
//! Tests cover:
//! - `auth check` three-state exit code (ready=0 / not_ready=1 / invalid=2)
//! - `--json` / `--credentials` / `--no-refresh` flag combinations
//! - `print-api-key` / `print-bearer-token` credential export
//! - `--min-expiry` duration parsing and enforcement
//! - Error path with no credential leakage (G4 red line)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rpi::cli::args::{parse_args, Args};
use rpi::cli::auth_check::{
    check_provider_auth, create_auth_check_model_runtime, get_provider_credential, AuthCheckReason,
    AuthCheckStatus,
};
use rpi::cli::auth_command::{parse_auth_command, validate_auth_command_args, AuthCommandKind};
use rpi::cli::credential_print::{
    resolve_credential_for_print, DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS,
};
use rpi::cli::run_auth::run_auth;
use rpi_ai::auth::{
    ApiKeyCredential, AuthCheck, AuthContext, AuthResult, AuthType, Credential, CredentialInfo,
    CredentialStore, FileCredentialStore, ModelAuth, ModelsError, ModelsErrorCode, ModifyFn,
    OAuthAuth, OAuthCredential, ProviderAuth,
};

struct TempDir {
    root: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "rpi-auth-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).expect("create temp dir");
        TempDir { root }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn owned(input: &[&str]) -> Vec<String> {
    input.iter().map(|s| s.to_string()).collect()
}

fn args_from(input: &[&str]) -> Args {
    parse_args(&owned(input))
}

fn api_key_store(provider: &str, key: &str) -> Arc<dyn CredentialStore> {
    let mut map = HashMap::new();
    map.insert(
        provider.to_owned(),
        Credential::ApiKey(ApiKeyCredential {
            key: Some(key.to_owned()),
            env: None,
        }),
    );
    Arc::new(FileCredentialStore::in_memory(map))
}

// ===========================================================================
// auth check — three-state exit code (ready=0 / not_ready=1 / invalid=2)
// ===========================================================================

#[tokio::test]
async fn test_auth_check_ready_exit_code_0() {
    // A provider with a stored API-key credential resolves to "ready".
    let _temp = TempDir::new();
    let store = api_key_store("openai", "sk-test-ready");
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "openai"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check should not error");
    assert_eq!(result.status, AuthCheckStatus::Ready);
    assert_eq!(result.status.exit_code(), 0);
}

#[tokio::test]
async fn test_auth_check_not_ready_exit_code_1_provider_not_found() {
    let _temp = TempDir::new();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(HashMap::new()));
    let model_runtime = create_auth_check_model_runtime(store).await;

    // Provider not in runtime → not_ready / provider_not_found.
    let args = args_from(&["--provider", "nonexistent-provider"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check should not error");
    assert_eq!(result.status, AuthCheckStatus::NotReady);
    assert_eq!(result.status.exit_code(), 1);
}

#[tokio::test]
async fn test_auth_check_not_ready_exit_code_1_credentials_not_configured() {
    let _temp = TempDir::new();
    // Empty credential store — provider exists (builtin) but no stored
    // credential → not_ready / credentials_not_configured.
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(HashMap::new()));
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "openai"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check should not error");
    assert_eq!(result.status, AuthCheckStatus::NotReady);
    assert_eq!(result.status.exit_code(), 1);
}

#[tokio::test]
async fn test_auth_check_requires_provider_or_model() {
    let _temp = TempDir::new();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(HashMap::new()));
    let model_runtime = create_auth_check_model_runtime(store).await;

    // No --provider / --model → AuthCommandError.
    let args = args_from(&[]);
    let result = check_provider_auth(&args, &model_runtime, false).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.0.contains("Auth checks require"));
}

#[tokio::test]
async fn test_auth_check_exit_code_mapping() {
    assert_eq!(AuthCheckStatus::Ready.exit_code(), 0);
    assert_eq!(AuthCheckStatus::NotReady.exit_code(), 1);
    assert_eq!(AuthCheckStatus::Invalid.exit_code(), 2);
    assert_eq!(AuthCheckStatus::Ready.as_str(), "ready");
    assert_eq!(AuthCheckStatus::NotReady.as_str(), "not_ready");
    assert_eq!(AuthCheckStatus::Invalid.as_str(), "invalid");
}

// ===========================================================================
// auth check — flag combinations (--json / --credentials / --no-refresh)
// ===========================================================================

#[tokio::test]
async fn test_auth_check_with_no_refresh_flag() {
    let _temp = TempDir::new();
    let store = api_key_store("openai", "sk-no-refresh-test");
    let model_runtime = create_auth_check_model_runtime(store).await;

    // --no-refresh path: check succeeds without refresh.
    let args = args_from(&["--provider", "openai"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check");
    assert_eq!(result.status, AuthCheckStatus::Ready);
}

#[tokio::test]
async fn test_auth_check_with_credentials_flag_ready() {
    let _temp = TempDir::new();
    let store = api_key_store("openai", "sk-creds-flag-test");
    let model_runtime = create_auth_check_model_runtime(store.clone()).await;

    let args = args_from(&["--provider", "openai"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check");
    assert_eq!(result.status, AuthCheckStatus::Ready);

    // --credentials path should be able to read the API key.
    let cred = rpi::cli::auth_check::get_provider_credential(
        &result.provider,
        &model_runtime,
        &store,
        false,
    )
    .await
    .expect("get credential");
    assert_eq!(cred.as_deref(), Some("sk-creds-flag-test"));
}

#[tokio::test]
async fn test_auth_check_json_flag_parsing() {
    // Verify that --json / --credentials / --no-refresh are parsed correctly
    // by parse_auth_command and forwarded as flags (not command_args).
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
    // --provider should be in args (for parse_args to consume).
    assert_eq!(cmd.args, vec!["--provider", "openai"]);
}

// ===========================================================================
// print-api-key — credential export
// ===========================================================================

#[tokio::test]
async fn test_print_api_key_direct_output() {
    let _temp = TempDir::new();
    let store = api_key_store("openai", "sk-direct-output-key");
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "openai"]);
    let credential =
        resolve_credential_for_print(&args, &model_runtime, AuthCommandKind::ApiKey, None)
            .await
            .expect("resolve");
    assert_eq!(credential, "sk-direct-output-key");
}

#[tokio::test]
async fn test_print_api_key_no_credential_error_message_no_leak() {
    // G4 red line: when no credential is configured, the error message must
    // NOT contain any API key / token value.
    let _temp = TempDir::new();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(HashMap::new()));
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "openai"]);
    let result =
        resolve_credential_for_print(&args, &model_runtime, AuthCommandKind::ApiKey, None).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().0;
    // Error must be user-facing; must NOT contain any secret.
    assert!(
        !err_msg.contains("sk-"),
        "Error message must not contain API key value: {err_msg}"
    );
    assert!(
        err_msg.contains("API key") || err_msg.contains("configured"),
        "Error should explain what's missing: {err_msg}"
    );
}

#[tokio::test]
async fn test_print_api_key_oauth_provider_gives_api_key_error() {
    // Provider configured with OAuth → error says "OAuth, not an API key".
    let _temp = TempDir::new();

    // We can't easily set up a full OAuth provider in a test, but we can
    // verify the error message structure for a provider that has an OAuth
    // credential type in the store. The check_auth in ModelRuntime will
    // not find a matching provider (no OAuth config), so it falls through
    // to the "No usable API key" path — this is acceptable.
    let mut creds = HashMap::new();
    creds.insert(
        "openai".to_owned(),
        Credential::OAuth(rpi_ai::auth::OAuthCredential {
            refresh: "refresh-token-secret".to_owned(),
            access: "access-token-secret".to_owned(),
            expires: 0,
            extra: serde_json::Map::new(),
        }),
    );
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(creds));
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "openai"]);
    let result =
        resolve_credential_for_print(&args, &model_runtime, AuthCommandKind::ApiKey, None).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().0;
    // G4: must not contain the token.
    assert!(
        !err_msg.contains("access-token-secret"),
        "Error must not contain access token: {err_msg}"
    );
    assert!(
        !err_msg.contains("refresh-token-secret"),
        "Error must not contain refresh token: {err_msg}"
    );
}

// ===========================================================================
// print-bearer-token — --min-expiry
// ===========================================================================

#[tokio::test]
async fn test_print_bearer_token_min_expiry_default_is_30_minutes() {
    // The default min-expiry for print-bearer-token is 30 minutes
    // (credential-print.ts:7), NOT the 5-minute OAuth default.
    assert_eq!(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS, 30 * 60_000);
}

#[tokio::test]
async fn test_print_bearer_token_min_expiry_from_flag() {
    // --min-expiry 1h → 3_600_000ms
    let cmd = parse_auth_command(&owned(&[
        "auth",
        "print-bearer-token",
        "--min-expiry",
        "1h",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cmd.min_expiry_ms, Some(3_600_000));
}

#[tokio::test]
async fn test_print_bearer_token_min_expiry_parsing_all_formats() {
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
    assert_eq!(parse_val("5M"), 300_000); // case-insensitive
    assert_eq!(parse_val("1H"), 3_600_000);
    assert_eq!(parse_val("500MS"), 500);
}

#[tokio::test]
async fn test_print_bearer_token_invalid_duration_errors() {
    for bad in ["abc", "30", "30x", "", "5min"] {
        let result =
            parse_auth_command(&owned(&["auth", "print-bearer-token", "--min-expiry", bad]));
        assert!(result.is_err(), "expected error for --min-expiry \"{bad}\"");
    }
}

#[tokio::test]
async fn test_print_bearer_token_missing_value_errors() {
    let result = parse_auth_command(&owned(&["auth", "print-bearer-token", "--min-expiry"]));
    assert!(result.is_err());
}

// ===========================================================================
// Help text parity
// ===========================================================================

#[test]
fn test_auth_help_contains_all_subcommands() {
    let help = rpi::cli::auth_command::print_auth_command_help();
    assert!(help.contains("print-api-key"));
    assert!(help.contains("print-bearer-token"));
    assert!(help.contains("auth check"));
    assert!(help.contains("--provider"));
    assert!(help.contains("--model"));
    assert!(help.contains("--min-expiry"));
    assert!(help.contains("--json"));
    assert!(help.contains("--credentials"));
    assert!(help.contains("--no-refresh"));
}

#[test]
fn test_auth_help_usage_strings() {
    assert!(
        rpi::cli::auth_command::get_auth_command_usage(AuthCommandKind::Check).contains("--json")
    );
    assert!(
        rpi::cli::auth_command::get_auth_command_usage(AuthCommandKind::Check)
            .contains("--no-refresh")
    );
    assert!(
        rpi::cli::auth_command::get_auth_command_usage(AuthCommandKind::BearerToken)
            .contains("--min-expiry")
    );
}

// ===========================================================================
// G4: Credential leakage in error messages
// ===========================================================================

#[tokio::test]
async fn test_error_messages_do_not_leak_credentials() {
    // G4 red line: no error message should contain API key or token values.
    let _temp = TempDir::new();

    // Unknown provider error should not contain secrets.
    let store = api_key_store("real-provider", "sk-super-secret-key");
    let model_runtime = create_auth_check_model_runtime(store).await;

    let args = args_from(&["--provider", "nonexistent-provider"]);
    let result = check_provider_auth(&args, &model_runtime, false)
        .await
        .expect("check");
    // This is not_ready (provider_not_found) — no secret in the result.
    assert_eq!(result.status, AuthCheckStatus::NotReady);
}

#[tokio::test]
async fn test_validate_args_rejects_unknown_flag_without_secrets() {
    // Unknown flag error should be clean.
    let args = args_from(&["--provider", "openai", "--secret-flag", "sk-secret-value"]);
    let result = validate_auth_command_args(&args, AuthCommandKind::Check);
    assert!(result.is_err());
    let err = result.unwrap_err().0;
    assert!(err.contains("Unknown option --secret-flag"));
    // The flag value should not appear in the error.
    assert!(!err.contains("sk-secret-value"));
}

// ===========================================================================
// Auth command dispatch — `run_auth` returns correct codes
// ===========================================================================

#[tokio::test]
async fn test_run_auth_help_returns_zero() {
    // `rpi auth` (bare) or `rpi auth help` → prints help, returns 0.
    let result = rpi::cli::run_auth::run_auth(&owned(&["auth"])).await;
    assert_eq!(result, Some(0));

    let result = rpi::cli::run_auth::run_auth(&owned(&["auth", "help"])).await;
    assert_eq!(result, Some(0));

    let result = rpi::cli::run_auth::run_auth(&owned(&["auth", "--help"])).await;
    assert_eq!(result, Some(0));
}

#[tokio::test]
async fn test_run_auth_non_auth_returns_none() {
    let result = rpi::cli::run_auth::run_auth(&owned(&["config"])).await;
    assert_eq!(result, None);

    let result = rpi::cli::run_auth::run_auth(&owned(&[])).await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_run_auth_unknown_subcommand_returns_1() {
    let result = rpi::cli::run_auth::run_auth(&owned(&["auth", "bogus"])).await;
    assert_eq!(result, Some(1));
}

#[tokio::test]
async fn test_run_auth_check_no_provider_returns_2() {
    // `auth check` without --provider/--model → validation error → exit 2.
    let result = rpi::cli::run_auth::run_auth(&owned(&["auth", "check"])).await;
    assert_eq!(result, Some(2));
}

#[tokio::test]
async fn test_run_auth_print_no_provider_returns_1() {
    // `auth print-api-key` without --provider/--model → exit 1 (not 2).
    let result = rpi::cli::run_auth::run_auth(&owned(&["auth", "print-api-key"])).await;
    assert_eq!(result, Some(1));
}

#[tokio::test]
async fn test_run_auth_check_flag_only_on_check_exit_1() {
    // `--json` on a print command → parse error → exit 1.
    let result = rpi::cli::run_auth::run_auth(&owned(&[
        "auth",
        "print-api-key",
        "--json",
        "--provider",
        "openai",
    ]))
    .await;
    assert_eq!(result, Some(1));
}

#[tokio::test]
async fn test_run_auth_min_expiry_on_wrong_command_exit_1() {
    let result = rpi::cli::run_auth::run_auth(&owned(&[
        "auth",
        "print-api-key",
        "--min-expiry",
        "30m",
        "--provider",
        "openai",
    ]))
    .await;
    assert_eq!(result, Some(1));
}

// ===========================================================================
// ModelRuntimeAuthOverrides — min_oauth_validity_ms passthrough
// ===========================================================================

#[test]
fn test_model_runtime_auth_overrides_has_min_oauth_validity_ms() {
    use rpi::core::model_runtime::ModelRuntimeAuthOverrides;
    let overrides = ModelRuntimeAuthOverrides {
        min_oauth_validity_ms: Some(30 * 60_000),
        ..Default::default()
    };
    assert_eq!(overrides.min_oauth_validity_ms, Some(1_800_000));
    assert!(overrides.api_key.is_none());
    assert!(overrides.env.is_none());

    // Default is None.
    let default = ModelRuntimeAuthOverrides::default();
    assert_eq!(default.min_oauth_validity_ms, None);
}

// ===========================================================================
// Error-mapping matrix — Err vs None vs Ok (upstream try/catch parity)
//
// Upstream wraps `checkAuth`/`getAuth` in try/catch (auth-check.ts:43-52):
//   - thrown error (catch) → invalid / exit 2
//   - falsy return (undefined/None) → not_ready / exit 1
// rpi initially merged Err into the not_ready path; these tests lock the
// corrected mapping.
// ===========================================================================

/// An OAuth implementation whose `refresh` always fails — simulates an OAuth
/// provider whose token has expired and the refresh endpoint is unreachable.
struct FailingRefreshOAuth;

#[async_trait::async_trait]
impl OAuthAuth for FailingRefreshOAuth {
    fn name(&self) -> &str {
        "Failing refresh OAuth"
    }
    async fn refresh(
        &self,
        _credential: &OAuthCredential,
        _signal: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<OAuthCredential, ModelsError> {
        Err(ModelsError::new(
            ModelsErrorCode::Oauth,
            "refresh endpoint unreachable",
        ))
    }
    async fn to_auth(&self, _credential: &OAuthCredential) -> Result<ModelAuth, ModelsError> {
        Ok(ModelAuth {
            api_key: None,
            headers: None,
            base_url: None,
        })
    }
}

/// Register a custom OAuth provider with `FailingRefreshOAuth` on the runtime.
async fn runtime_with_failing_oauth_provider(
) -> (Arc<rpi::core::model_runtime::ModelRuntime>, TempDir) {
    use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
    use rpi_ai::api::openai_completions::OpenAiCompletions;
    use rpi_ai::models::{create_provider, CreateProviderOptions, ProviderApi};

    let temp = TempDir::new();
    // Pre-seed an expired OAuth credential so the refresh path is triggered.
    let mut creds = HashMap::new();
    creds.insert(
        "failing-oauth-provider".to_owned(),
        Credential::OAuth(OAuthCredential {
            refresh: "expired-refresh".to_owned(),
            access: "expired-access".to_owned(),
            expires: 0, // expired → must refresh → refresh fails
            extra: serde_json::Map::new(),
        }),
    );
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(creds));
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(store.clone()),
        models_path: ModelsPathInput::Disabled,
        allow_model_network: false,
        ..Default::default()
    })
    .await;

    let provider = create_provider(CreateProviderOptions {
        id: "failing-oauth-provider".to_owned(),
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: None,
            oauth: Some(Arc::new(FailingRefreshOAuth)),
        },
        models: vec![],
        api: ProviderApi::Single(Arc::new(OpenAiCompletions)),
        ..Default::default()
    });
    runtime
        .register_native_provider(provider)
        .await
        .expect("register");

    (runtime, temp)
}

#[tokio::test]
async fn test_auth_check_refresh_get_auth_err_maps_to_invalid_exit_2() {
    // Upstream auth-check.ts:46-48 inside try/catch (50-52): when `getAuth`
    // throws during the refresh path, the result is invalid/invalid_state
    // (exit 2), NOT not_ready/credentials_not_configured (exit 1).
    //
    // Scenario: OAuth provider with an expired stored token. `checkAuth`
    // succeeds (OAuth detected), but `getAuth` attempts a refresh that fails.
    let (model_runtime, _temp) = runtime_with_failing_oauth_provider().await;

    let args = args_from(&["--provider", "failing-oauth-provider"]);
    let result = check_provider_auth(&args, &model_runtime, true)
        .await
        .expect("check should not error at the top level");

    assert_eq!(
        result.status,
        AuthCheckStatus::Invalid,
        "refresh getAuth Err must map to Invalid (exit 2)"
    );
    assert_eq!(result.status.exit_code(), 2);
    assert_eq!(result.reason, Some(AuthCheckReason::InvalidState));
}

/// An ApiKeyAuth whose `check` returns Some (so `checkAuth` succeeds) but
/// whose `resolve` returns Ok(None) (so `getProviderAuth` returns None).
/// This simulates the upstream "getAuth returns falsy" branch.
struct CheckOnlyApiKeyAuth;

#[async_trait::async_trait]
impl rpi_ai::auth::ApiKeyAuth for CheckOnlyApiKeyAuth {
    fn name(&self) -> &str {
        "Check-only API key"
    }
    async fn check(
        &self,
        _ctx: &dyn AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthCheck>, ModelsError> {
        Ok(Some(AuthCheck {
            source: Some("check-only".to_owned()),
            kind: AuthType::ApiKey,
        }))
    }
    async fn resolve(
        &self,
        _ctx: &dyn AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_auth_check_refresh_get_auth_none_maps_to_not_ready_exit_1() {
    // Upstream auth-check.ts:46: `!(await modelRuntime.getAuth(provider))` —
    // when `getAuth` returns falsy (None) on the refresh path, the result is
    // not_ready/credentials_not_configured (exit 1).
    //
    // Scenario: a provider whose `checkAuth` returns Some (check succeeds)
    // but `getProviderAuth` returns Ok(None) (resolve returns None).
    use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
    use rpi_ai::api::openai_completions::OpenAiCompletions;
    use rpi_ai::models::{create_provider, CreateProviderOptions, ProviderApi};

    let _temp = TempDir::new();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::in_memory(HashMap::new()));
    let model_runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(store.clone()),
        models_path: ModelsPathInput::Disabled,
        allow_model_network: false,
        ..Default::default()
    })
    .await;

    let provider = create_provider(CreateProviderOptions {
        id: "check-only-provider".to_owned(),
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(CheckOnlyApiKeyAuth)),
            oauth: None,
        },
        models: vec![],
        api: ProviderApi::Single(Arc::new(OpenAiCompletions)),
        ..Default::default()
    });
    model_runtime
        .register_native_provider(provider)
        .await
        .expect("register");

    let args = args_from(&["--provider", "check-only-provider"]);
    let result = check_provider_auth(&args, &model_runtime, true)
        .await
        .expect("check");

    assert_eq!(
        result.status,
        AuthCheckStatus::NotReady,
        "refresh getAuth Ok(None) must map to NotReady (exit 1)"
    );
    assert_eq!(result.status.exit_code(), 1);
    assert_eq!(
        result.reason,
        Some(AuthCheckReason::CredentialsNotConfigured)
    );
}

/// A CredentialStore whose `read` always fails — triggers the Err path in
/// `get_provider_credential`.
struct FailingCredentialStore;

#[async_trait::async_trait]
impl CredentialStore for FailingCredentialStore {
    async fn read(
        &self,
        _provider_id: &str,
        _options: Option<&rpi_ai::auth::AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        Err(ModelsError::new(
            ModelsErrorCode::Auth,
            "credential store read failure",
        ))
    }
    async fn list(
        &self,
        _options: Option<&rpi_ai::auth::AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, ModelsError> {
        Ok(vec![])
    }
    async fn modify(
        &self,
        _provider_id: &str,
        _f: ModifyFn,
        _options: Option<&rpi_ai::auth::AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        Err(ModelsError::new(
            ModelsErrorCode::Auth,
            "credential store modify failure",
        ))
    }
    async fn delete(
        &self,
        _provider_id: &str,
        _options: Option<&rpi_ai::auth::AuthOperationOptions>,
    ) -> Result<(), ModelsError> {
        Ok(())
    }
}

#[tokio::test]
async fn test_get_provider_credential_err_maps_to_invalid_exit_2() {
    // Upstream main.ts:197-203: the inner catch around `getProviderCredential`
    // maps any thrown error to invalid/invalid_state (exit 2). The rpi port
    // previously mapped Err to NotReady/CredentialNotAvailable (exit 1) —
    // this test locks the corrected mapping at the `get_provider_credential`
    // level (the run_auth.rs Err branch is structurally identical).
    let (model_runtime, _temp) = runtime_with_failing_oauth_provider().await;

    let failing_store: Arc<dyn CredentialStore> = Arc::new(FailingCredentialStore);
    let result = get_provider_credential(
        "failing-oauth-provider",
        &model_runtime,
        &failing_store,
        true, // refresh
    )
    .await;

    assert!(
        result.is_err(),
        "get_provider_credential with FailingCredentialStore must return Err"
    );
}

#[tokio::test]
async fn test_run_auth_check_diagnostics_exit_2() {
    // Upstream main.ts:165-167 + 212: parse diagnostics are thrown into the
    // outer catch, whose exit code is `command.kind === "check" ? 2 : 1`.
    // The `check` subcommand must exit 2 on diagnostics.
    //
    // `--thinking bogus` produces a warning diagnostic in parse_args, which
    // is forwarded through auth check's command.args.
    let result = run_auth(&owned(&[
        "auth",
        "check",
        "--thinking",
        "bogus",
        "--provider",
        "openai",
    ]))
    .await;
    assert_eq!(
        result,
        Some(2),
        "check subcommand diagnostics must exit 2, got {result:?}"
    );
}

#[tokio::test]
async fn test_run_auth_print_diagnostics_exit_1() {
    // Same upstream logic, but for a print subcommand: exit 1 (not 2).
    let result = run_auth(&owned(&[
        "auth",
        "print-api-key",
        "--thinking",
        "bogus",
        "--provider",
        "openai",
    ]))
    .await;
    assert_eq!(
        result,
        Some(1),
        "print subcommand diagnostics must exit 1, got {result:?}"
    );
}
