//! Shell `auth logout` (cli/auth-logout.ts @ b4afdddf, commit 9b9cb30ae).

use std::sync::Arc;

use rpi_ai::auth::CredentialType;
use tokio_util::sync::CancellationToken;

use crate::cli::auth_login::matching_providers;
use crate::cli::auth_terminal::{AuthOutput, replace_control_characters};
use crate::core::model_runtime::ModelRuntime;

pub const AUTH_LOGOUT_USAGE: &str = "rpi auth logout <provider>";

pub struct AuthLogoutOptions {
    pub model_runtime: Arc<ModelRuntime>,
    pub terminal: Arc<dyn AuthOutput>,
    pub signal: CancellationToken,
}

pub async fn run_auth_logout(args: &[String], options: AuthLogoutOptions) -> i32 {
    let [provider_ref, rest @ ..] = args else {
        options
            .terminal
            .error(&format!("Usage: {AUTH_LOGOUT_USAGE}"));
        return 1;
    };
    if provider_ref.starts_with('-') || !rest.is_empty() {
        options
            .terminal
            .error(&format!("Usage: {AUTH_LOGOUT_USAGE}"));
        return 1;
    }

    let stored = tokio::select! {
        stored = options.model_runtime.list_credentials() => stored,
        _ = options.signal.cancelled() => {
            options.terminal.error("Could not read stored credentials: Login cancelled");
            return 1;
        }
    };
    let stored = match stored {
        Ok(stored) => stored,
        Err(error) => {
            options.terminal.error(&format!(
                "Could not read stored credentials: {}",
                error.message
            ));
            return 1;
        }
    };
    let provider = matching_providers(&options.model_runtime, provider_ref)
        .into_iter()
        .next();
    let provider_id = provider
        .as_ref()
        .map(|provider| provider.id())
        .unwrap_or(provider_ref);
    let credential = stored
        .iter()
        .find(|credential| credential.provider_id == provider_id);
    if provider.is_none() && credential.is_none() {
        options.terminal.error(&format!(
            "Unknown provider \"{}\".",
            replace_control_characters(provider_ref)
        ));
        let mut stored_ids = stored
            .iter()
            .map(|credential| credential.provider_id.clone())
            .collect::<Vec<_>>();
        stored_ids.sort();
        options.terminal.error(&format!(
            "Stored credentials: {}",
            if stored_ids.is_empty() {
                "none".to_owned()
            } else {
                stored_ids.join(", ")
            }
        ));
        return 1;
    }
    let provider_name = provider
        .as_ref()
        .map(|provider| provider.name())
        .unwrap_or(provider_ref);
    let Some(credential) = credential else {
        options.terminal.write(&format!(
            "No stored credentials for {provider_name}. Environment variables and models.json config are unchanged."
        ));
        return 0;
    };

    let logout = options.model_runtime.logout(&credential.provider_id);
    tokio::pin!(logout);
    let result = tokio::select! {
        result = &mut logout => result,
        _ = options.signal.cancelled() => {
            options.terminal.error("Logout failed: Login cancelled");
            return 1;
        }
    };
    if let Err(error) = result {
        if error.to_string().contains("synchronization") {
            options.terminal.error(&format!(
                "Credentials removed for {provider_name}, but local model state could not be synchronized: {error}"
            ));
        } else {
            options.terminal.error(&format!("Logout failed: {error}"));
        }
        return 1;
    }
    let message = if credential.credential_type == CredentialType::Oauth {
        format!("Logged out of {provider_name}.")
    } else {
        format!(
            "Removed stored API key for {provider_name}. Environment variables and models.json config are unchanged."
        )
    };
    options.terminal.write(&message);
    0
}
