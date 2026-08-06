//! Port of `packages/ai/src/providers/google.ts` @ pi 0.82.1 (2efa728).

use std::sync::Arc;

use crate::api::google_generative_ai::GoogleGenerativeAi;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `googleProvider()`.
pub fn google_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "google".to_owned(),
        name: Some("Google".to_owned()),
        base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Gemini API key",
                &["GEMINI_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("google").to_vec(),
        api: ProviderApi::Single(Arc::new(GoogleGenerativeAi)),
    })
}
