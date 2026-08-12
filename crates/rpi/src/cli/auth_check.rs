//! Port of `packages/coding-agent/src/cli/auth-check.ts` @ 4181f66
//! (commit a261366bd: auth check).
//!
//! Provider/model authentication pre-check with three-state exit code:
//! `ready=0` / `not_ready=1` / `invalid=2`.
//!
//! The decision matrix (auth-check.ts:22-53):
//! 1. Resolve provider from `--provider` or `--model`.
//! 2. If `modelRuntime.getError()` → `invalid` (`invalid_state`).
//! 3. If provider not found → `not_ready` (`provider_not_found`).
//! 4. `checkAuth(provider)` → `None` → `not_ready` (`credentials_not_configured`).
//! 5. If `refresh` and `getAuth(provider)` returns `None` → `not_ready`
//!    (`credentials_not_configured`).
//! 6. Otherwise → `ready` with `authType`.
//!
//! Any thrown error → `invalid` (`invalid_state`).
//!
//! Intentional differences: none.

use std::sync::Arc;

use rpi_ai::auth::{AuthResult, CredentialStore, ModelsError};

use crate::cli::args::Args;
use crate::cli::auth_command::{get_auth_credential, validate_auth_command_args, AuthCommandError};
use crate::core::model_resolver::{resolve_cli_model, ResolveCliModelOptions};
use crate::core::model_runtime::ModelRuntime;

/// `AuthCheckStatus` (auth-check.ts:8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheckStatus {
    Ready,
    NotReady,
    Invalid,
}

impl AuthCheckStatus {
    /// Upstream string representation (main.ts:208).
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthCheckStatus::Ready => "ready",
            AuthCheckStatus::NotReady => "not_ready",
            AuthCheckStatus::Invalid => "invalid",
        }
    }

    /// Exit code mapping (main.ts:208):
    /// ready=0 / not_ready=1 / invalid=2.
    pub fn exit_code(&self) -> i32 {
        match self {
            AuthCheckStatus::Ready => 0,
            AuthCheckStatus::NotReady => 1,
            AuthCheckStatus::Invalid => 2,
        }
    }
}

/// `AuthCheckReason` (auth-check.ts:9-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheckReason {
    ProviderNotFound,
    CredentialsNotConfigured,
    CredentialNotAvailable,
    InvalidState,
}

impl AuthCheckReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthCheckReason::ProviderNotFound => "provider_not_found",
            AuthCheckReason::CredentialsNotConfigured => "credentials_not_configured",
            AuthCheckReason::CredentialNotAvailable => "credential_not_available",
            AuthCheckReason::InvalidState => "invalid_state",
        }
    }
}

/// `AuthCheckResult` (auth-check.ts:15-20).
#[derive(Debug, Clone)]
pub struct AuthCheckResult {
    pub status: AuthCheckStatus,
    pub provider: String,
    pub reason: Option<AuthCheckReason>,
    pub auth_type: Option<&'static str>,
}

/// `checkProviderAuth` (auth-check.ts:22-53).
///
/// Returns `Ok(AuthCheckResult)` on success, or `Err(AuthCommandError)` if
/// the provider/model cannot be resolved from the CLI args.
pub async fn check_provider_auth(
    args: &Args,
    model_runtime: &ModelRuntime,
    refresh: bool,
) -> Result<AuthCheckResult, AuthCommandError> {
    let (cli_provider, cli_model) =
        validate_auth_command_args(args, crate::cli::auth_command::AuthCommandKind::Check)?;
    let mut provider = cli_provider.clone();

    // Resolve provider from model if needed (auth-check.ts:29-35).
    if let Some(cli_model) = &cli_model {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: cli_provider.as_deref(),
            cli_model: Some(cli_model),
            cli_thinking: None,
            model_runtime,
        });
        if resolved.error.is_some() || resolved.model.is_none() {
            return Err(AuthCommandError(resolved.error.unwrap_or_else(|| {
                format!("Unable to resolve model \"{cli_model}\"")
            })));
        }
        // Invariant: `resolved.model` is `Some` here — both `error.is_some()`
        // and `model.is_none()` were checked above.
        provider = Some(resolved.model.expect("checked above").provider);
    }

    let provider = provider
        .ok_or_else(|| AuthCommandError("Unable to resolve an auth provider".to_owned()))?;

    // Step 2: runtime error → invalid (auth-check.ts:37-39).
    if model_runtime.get_error().is_some() {
        return Ok(AuthCheckResult {
            status: AuthCheckStatus::Invalid,
            provider,
            reason: Some(AuthCheckReason::InvalidState),
            auth_type: None,
        });
    }

    // Step 3: provider not found → not_ready (auth-check.ts:40-42).
    if model_runtime.get_provider(&provider).is_none() {
        return Ok(AuthCheckResult {
            status: AuthCheckStatus::NotReady,
            provider,
            reason: Some(AuthCheckReason::ProviderNotFound),
            auth_type: None,
        });
    }

    // Step 4-6: check auth, optionally refresh (auth-check.ts:43-52).
    match model_runtime.check_auth(&provider).await {
        Ok(Some(auth)) => {
            // Step 5: if refresh, verify getAuth succeeds (auth-check.ts:46-48).
            if refresh
                && model_runtime
                    .get_provider_auth(&provider, None)
                    .await
                    .is_err()
            {
                return Ok(AuthCheckResult {
                    status: AuthCheckStatus::NotReady,
                    provider,
                    reason: Some(AuthCheckReason::CredentialsNotConfigured),
                    auth_type: None,
                });
            }
            // Step 6: ready (auth-check.ts:49).
            let auth_type = match auth.kind {
                rpi_ai::auth::AuthType::ApiKey => Some("api_key"),
                rpi_ai::auth::AuthType::Oauth => Some("oauth"),
            };
            Ok(AuthCheckResult {
                status: AuthCheckStatus::Ready,
                provider,
                reason: None,
                auth_type,
            })
        }
        Ok(None) => Ok(AuthCheckResult {
            status: AuthCheckStatus::NotReady,
            provider,
            reason: Some(AuthCheckReason::CredentialsNotConfigured),
            auth_type: None,
        }),
        Err(_) => Ok(AuthCheckResult {
            status: AuthCheckStatus::Invalid,
            provider,
            reason: Some(AuthCheckReason::InvalidState),
            auth_type: None,
        }),
    }
}

/// `getProviderCredential` (auth-check.ts:55-64): resolve a credential string
/// for the `--credentials` flag path.
///
/// When `refresh` is false and the stored credential is OAuth, returns the
/// stored `access` token without triggering a refresh. Otherwise resolves
/// through `getAuth` (which may refresh OAuth).
pub async fn get_provider_credential(
    provider_id: &str,
    model_runtime: &ModelRuntime,
    credentials: &Arc<dyn CredentialStore>,
    refresh: bool,
) -> Result<Option<String>, ModelsError> {
    let stored = credentials.read(provider_id, None).await?;
    if !refresh {
        if let Some(rpi_ai::auth::Credential::OAuth(oauth)) = &stored {
            return Ok(Some(oauth.access.clone()));
        }
    }
    let auth: Option<AuthResult> = model_runtime.get_provider_auth(provider_id, None).await?;
    Ok(auth.and_then(|a| get_auth_credential(&a)))
}

/// `createAuthCheckModelRuntime` (auth-check.ts:66-73): create a `ModelRuntime`
/// for the auth check command — no network, no refresh-on-create, in-memory
/// model store (so nothing is persisted to disk).
pub async fn create_auth_check_model_runtime(
    credentials: Arc<dyn CredentialStore>,
) -> Arc<ModelRuntime> {
    use crate::core::model_runtime::CreateModelRuntimeOptions;
    ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(credentials),
        // In-memory models store (no models.json on disk) — mirrors upstream's
        // `new InMemoryCodingAgentModelsStore()`.
        models_path: crate::core::model_runtime::ModelsPathInput::Disabled,
        allow_model_network: false,
        ..Default::default()
    })
    .await
}
