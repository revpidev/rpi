//! Port of `packages/ai/src/providers/opencode-go.ts` @ pi `05558a792` / `4181f66`
//! — OpenCode Go: mixed 3-API dispatch on `model.api`
//! (`anthropic-messages` / `openai-completions` / `openai-responses`).
//! Display name changed from "OpenCode Zen Go" to "OpenCode Go" in
//! `05558a792` (#7157).

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::anthropic_messages::AnthropicMessages;
use crate::api::openai_completions::OpenAiCompletions;
use crate::api::openai_responses::OpenAiResponses;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{
    create_provider, CreateProviderOptions, Provider, ProviderApi, ProviderStreams,
};
use crate::types::ApiKind;

/// `opencodeGoProvider()`.
pub fn opencode_go_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "opencode-go".to_owned(),
        name: Some("OpenCode Go".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "OpenCode API key",
                &["OPENCODE_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("opencode-go").to_vec(),
        api: ProviderApi::Map(api_map()),
        ..Default::default()
    })
}

/// Mixed-API dispatch table (opencode-go.ts:13-17).
fn api_map() -> HashMap<String, Arc<dyn ProviderStreams>> {
    HashMap::from([
        (
            ApiKind::ANTHROPIC_MESSAGES.to_owned(),
            Arc::new(AnthropicMessages) as Arc<dyn ProviderStreams>,
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
    fn api_map_covers_the_three_catalog_apis() {
        let map = api_map();
        for key in [
            ApiKind::ANTHROPIC_MESSAGES,
            ApiKind::OPENAI_COMPLETIONS,
            ApiKind::OPENAI_RESPONSES,
        ] {
            assert!(map.contains_key(key), "missing dispatch key {key}");
        }
        assert_eq!(map.len(), 3);
        for model in get_builtin_models("opencode-go") {
            assert!(
                map.contains_key(model.api.as_str()),
                "catalog model {} has undispatched api {}",
                model.id,
                model.api
            );
        }
    }
}
