//! Port of `packages/ai/src/providers/mistral.ts` @ pi 0.82.1 (2efa728).

use std::sync::Arc;

use crate::api::mistral_conversations::MistralConversations;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `mistralProvider()`.
pub fn mistral_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "mistral".to_owned(),
        name: Some("Mistral".to_owned()),
        base_url: Some("https://api.mistral.ai".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Mistral API key",
                &["MISTRAL_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("mistral").to_vec(),
        api: ProviderApi::Single(Arc::new(MistralConversations)),
    })
}
