//! Port of `packages/ai/src/providers/opencode.ts` @ pi 0.82.1 (2efa728) —
//! OpenCode Zen: mixed 4-API dispatch on `model.api` (`anthropic-messages` /
//! `google-generative-ai` / `openai-completions` / `openai-responses`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::anthropic_messages::AnthropicMessages;
use crate::api::google_generative_ai::GoogleGenerativeAi;
use crate::api::openai_completions::OpenAiCompletions;
use crate::api::openai_responses::OpenAiResponses;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{
    create_provider, CreateProviderOptions, Provider, ProviderApi, ProviderStreams,
};
use crate::types::ApiKind;

/// `opencodeProvider()`.
pub fn opencode_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "opencode".to_owned(),
        name: Some("OpenCode Zen".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "OpenCode API key",
                &["OPENCODE_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("opencode").to_vec(),
        api: ProviderApi::Map(api_map()),
        ..Default::default()
    })
}

/// Mixed-API dispatch table (opencode.ts:17-22).
fn api_map() -> HashMap<String, Arc<dyn ProviderStreams>> {
    HashMap::from([
        (
            ApiKind::ANTHROPIC_MESSAGES.to_owned(),
            Arc::new(AnthropicMessages) as Arc<dyn ProviderStreams>,
        ),
        (
            ApiKind::GOOGLE_GENERATIVE_AI.to_owned(),
            Arc::new(GoogleGenerativeAi) as Arc<dyn ProviderStreams>,
        ),
        (
            ApiKind::OPENAI_COMPLETIONS.to_owned(),
            Arc::new(OpenAiCompletions) as Arc<dyn ProviderStreams>,
        ),
        (
            ApiKind::OPENAI_RESPONSES.to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn ProviderStreams>,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_map_covers_the_four_catalog_apis() {
        let map = api_map();
        for key in [
            ApiKind::ANTHROPIC_MESSAGES,
            ApiKind::GOOGLE_GENERATIVE_AI,
            ApiKind::OPENAI_COMPLETIONS,
            ApiKind::OPENAI_RESPONSES,
        ] {
            assert!(map.contains_key(key), "missing dispatch key {key}");
        }
        assert_eq!(map.len(), 4);
        for model in get_builtin_models("opencode") {
            assert!(
                map.contains_key(model.api.as_str()),
                "catalog model {} has undispatched api {}",
                model.id,
                model.api
            );
        }
    }
}
