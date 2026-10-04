//! Port of `packages/ai/src/providers/openai.ts` @ pi 0.82.1 (2efa728).

use std::sync::Arc;

use crate::api::openai_responses::OpenAiResponses;
use crate::auth::oauth::openai_chatgpt_oauth;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::generated::get_builtin_models;
use crate::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};

/// `openaiProvider()`.
pub fn openai_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "openai".to_owned(),
        name: Some("OpenAI".to_owned()),
        base_url: Some("https://api.openai.com/v1".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "OpenAI API key",
                &["OPENAI_API_KEY"],
            ))),
            // `lazyOAuth({ name: "OpenAI (ChatGPT subscription)", ... })`
            // (`02eed88fd`); loginLabel has no `OAuthAuth` slot in rpi.
            oauth: Some(openai_chatgpt_oauth()),
        },
        models: get_builtin_models("openai").to_vec(),
        api: ProviderApi::Single(Arc::new(OpenAiResponses)),
        ..Default::default()
    })
}
