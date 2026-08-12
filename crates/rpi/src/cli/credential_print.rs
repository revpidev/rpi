//! Port of `packages/coding-agent/src/cli/credential-print.ts` @ 4181f66
//! (commit 99e34013d: print-api-key/print-bearer-token #7168).
//!
//! Resolve one configured provider credential for export to external clients.
//! Calls `ModelRuntime.get_auth()` which refreshes and persists OAuth
//! credentials with less than five minutes remaining (T21), unless
//! `--min-expiry` requests a longer validity floor.
//!
//! Intentional differences: none.

use rpi_ai::auth::{AuthResult, CredentialType};
use rpi_ai::types::Model;

use crate::cli::args::Args;
use crate::cli::auth_command::{
    get_auth_credential, validate_auth_command_args, AuthCommandError, AuthCommandKind,
};
use crate::core::model_resolver::{resolve_cli_model, ResolveCliModelOptions};
use crate::core::model_runtime::{ModelRuntime, ModelRuntimeAuthOverrides};

/// `DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS` (credential-print.ts:7):
/// 30 minutes — the floor when `--min-expiry` is not given.
pub const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: u64 = 30 * 60_000;

/// `resolveCredentialForPrint` (credential-print.ts:17-87).
///
/// Returns the credential string on success, or `Err(AuthCommandError)` with
/// a user-facing message (no secret values).
pub async fn resolve_credential_for_print(
    args: &Args,
    model_runtime: &ModelRuntime,
    kind: AuthCommandKind,
    min_expiry_ms: Option<u64>,
) -> Result<String, AuthCommandError> {
    let (cli_provider, cli_model) = validate_auth_command_args(args, kind)?;
    let credential_types = model_runtime
        .list_credentials()
        .await
        .map_err(|e| AuthCommandError(e.message.clone()))?;
    let credential_type_map: std::collections::HashMap<String, CredentialType> = credential_types
        .iter()
        .map(|c| (c.provider_id.clone(), c.credential_type))
        .collect();

    // Build the list of providers to query (credential-print.ts:28-54).
    let mut providers: Vec<(String, Option<Model>)> = Vec::new();

    if let Some(cli_provider) = &cli_provider {
        let provider = model_runtime.get_provider(cli_provider).ok_or_else(|| {
            AuthCommandError(format!(
                "Unknown provider \"{cli_provider}\". Use --list-models to see available providers."
            ))
        })?;
        let provider_id = provider.id().to_owned();
        if let Some(cli_model) = &cli_model {
            let resolved = resolve_cli_model(ResolveCliModelOptions {
                cli_provider: Some(&provider_id),
                cli_model: Some(cli_model),
                cli_thinking: None,
                model_runtime,
            });
            if resolved.error.is_some() || resolved.model.is_none() {
                return Err(AuthCommandError(resolved.error.unwrap_or_else(|| {
                    "Unable to resolve the requested provider/model".to_owned()
                })));
            }
            providers.push((provider_id, resolved.model));
        } else {
            providers.push((provider_id, None));
        }
    } else {
        // No --provider: iterate all providers that have a stored credential,
        // try to resolve the model on each (credential-print.ts:43-53).
        let cli_model = cli_model
            .as_deref()
            .expect("validated: at least one of provider/model is Some");
        for provider in model_runtime.get_providers() {
            let provider_id = provider.id().to_owned();
            if !credential_type_map.contains_key(&provider_id) {
                continue;
            }
            let resolved = resolve_cli_model(ResolveCliModelOptions {
                cli_provider: Some(&provider_id),
                cli_model: Some(cli_model),
                cli_thinking: None,
                model_runtime,
            });
            if resolved.model.is_some()
                && resolved.error.is_none()
                && !resolved
                    .warning
                    .as_deref()
                    .is_some_and(|w| w.contains("Using custom model id"))
            {
                providers.push((provider_id, resolved.model));
            }
        }
        if providers.is_empty() {
            return Err(AuthCommandError(format!(
                "Model \"{cli_model}\" not found. Use --list-models to see available models."
            )));
        }
    }

    // Collect matching credentials (credential-print.ts:56-70).
    let mut credentials: Vec<(String, String)> = Vec::new();
    for (provider_id, model) in &providers {
        let credential_type = credential_type_map.get(provider_id);
        // Filter by kind (credential-print.ts:59-60).
        match kind {
            AuthCommandKind::ApiKey => {
                if credential_type == Some(&CredentialType::Oauth) {
                    continue;
                }
            }
            AuthCommandKind::BearerToken => {
                if credential_type != Some(&CredentialType::Oauth) {
                    continue;
                }
            }
            AuthCommandKind::Check => {}
        }

        let overrides = if kind == AuthCommandKind::BearerToken {
            Some(ModelRuntimeAuthOverrides {
                min_oauth_validity_ms: Some(
                    min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS),
                ),
                ..Default::default()
            })
        } else {
            None
        };

        let auth: Option<AuthResult> = if let Some(model) = model {
            model_runtime.get_auth(model, overrides.as_ref()).await
        } else {
            model_runtime
                .get_provider_auth(
                    provider_id,
                    overrides_to_auth_resolution(overrides).as_ref(),
                )
                .await
        }
        .map_err(|e| AuthCommandError(e.message.clone()))?;

        if let Some(value) = auth.as_ref().and_then(get_auth_credential) {
            credentials.push((provider_id.clone(), value));
        }
    }

    // Return the single match, or construct an appropriate error
    // (credential-print.ts:72-86).
    if credentials.len() == 1 {
        return Ok(credentials.into_iter().next().expect("exactly one").1);
    }
    if credentials.is_empty() {
        let provider_id = providers.first().map(|(id, _)| id.as_str());
        let credential_type = provider_id.and_then(|id| credential_type_map.get(id));
        if let (Some(provider_id), Some(credential_type)) = (provider_id, credential_type) {
            match (kind, credential_type) {
                (AuthCommandKind::ApiKey, CredentialType::Oauth) => {
                    return Err(AuthCommandError(format!(
                        "Provider \"{provider_id}\" is configured with OAuth, not an API key"
                    )));
                }
                (AuthCommandKind::BearerToken, _) => {
                    return Err(AuthCommandError(format!(
                        "Provider \"{provider_id}\" is not configured with an OAuth bearer token"
                    )));
                }
                _ => {}
            }
        }
        let label = match kind {
            AuthCommandKind::ApiKey => "API key",
            AuthCommandKind::BearerToken => "OAuth bearer token",
            AuthCommandKind::Check => "credential",
        };
        return Err(AuthCommandError(format!("No usable {label} is configured")));
    }
    // Multiple matches (credential-print.ts:84-86).
    let provider_ids: Vec<&str> = credentials.iter().map(|(id, _)| id.as_str()).collect();
    Err(AuthCommandError(format!(
        "Multiple configured providers matched ({}). Specify --provider.",
        provider_ids.join(", ")
    )))
}

/// Convert `ModelRuntimeAuthOverrides` into the `AuthResolutionOverrides`
/// needed for the `get_provider_auth` (string-arm) path. The model-arm path
/// (`get_auth`) handles the conversion internally.
fn overrides_to_auth_resolution(
    overrides: Option<ModelRuntimeAuthOverrides>,
) -> Option<rpi_ai::auth::AuthResolutionOverrides> {
    overrides.map(|o| rpi_ai::auth::AuthResolutionOverrides {
        min_oauth_validity_ms: o.min_oauth_validity_ms,
        ..Default::default()
    })
}
