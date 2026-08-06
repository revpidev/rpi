//! Port of `packages/ai/src/providers/azure-openai-responses.ts` @ pi 0.82.1
//! (2efa728). No `baseUrl` upstream — the azure adapter derives it from the
//! `AZURE_OPENAI_RESOURCE_NAME`/base-URL env at request time.

use std::sync::Arc;

use crate::api::azure_openai_responses::AzureOpenAiResponses;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `azureOpenAIResponsesProvider()`.
pub fn azure_openai_responses_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "azure-openai-responses".to_owned(),
        name: Some("Azure OpenAI".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Azure OpenAI API key",
                &["AZURE_OPENAI_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("azure-openai-responses").to_vec(),
        api: ProviderApi::Single(Arc::new(AzureOpenAiResponses)),
    })
}
