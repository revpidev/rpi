//! Login-help message formats.
//!
//! Port of `packages/coding-agent/src/core/auth-guidance.ts` @ pi 0.82.1
//! (2efa728).

use crate::config::get_docs_path;

const UNKNOWN_PROVIDER: &str = "unknown";

/// `getProviderLoginHelp` (auth-guidance.ts:7-13).
pub fn get_provider_login_help() -> String {
    let docs = get_docs_path();
    format!(
        "Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display()
    )
}

/// `formatNoModelsAvailableMessage` (auth-guidance.ts:15-17).
pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", get_provider_login_help())
}

/// `formatNoModelSelectedMessage` (auth-guidance.ts:19-21).
pub fn format_no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        get_provider_login_help()
    )
}

/// `formatNoApiKeyFoundMessage` (auth-guidance.ts:23-26).
pub fn format_no_api_key_found_message(provider: &str) -> String {
    let provider_display = if provider == UNKNOWN_PROVIDER {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {provider_display}.\n\n{}",
        get_provider_login_help()
    )
}
