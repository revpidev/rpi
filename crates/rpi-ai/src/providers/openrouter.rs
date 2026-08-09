//! Port of `packages/ai/src/providers/openrouter.ts` @ pi 0.82.1 (2efa728).
//!
//! T13 W5: OAuth (`loadOpenRouterOAuth`: PKCE exchange for a permanent key,
//! no-op refresh) landed in [`crate::auth::oauth::openrouter`] and replaces
//! the W4 `PendingOAuth` placeholder (deviation D-032). The upstream
//! `loginLabel` ("Sign in with OpenRouter") has no `OAuthAuth` slot and stays
//! unported.

use std::sync::Arc;

use crate::api::openai_completions::OpenAiCompletions;
use crate::auth::oauth::openrouter_oauth;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `openrouterProvider()`.
pub fn openrouter_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "openrouter".to_owned(),
        name: Some("OpenRouter".to_owned()),
        base_url: Some("https://openrouter.ai/api/v1".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "OpenRouter API key",
                &["OPENROUTER_API_KEY"],
            ))),
            oauth: Some(openrouter_oauth()),
        },
        models: get_builtin_models("openrouter").to_vec(),
        api: ProviderApi::Single(Arc::new(OpenAiCompletions)),
    })
}
