//! Port of `packages/ai/src/providers/ant-ling.ts` @ pi 0.82.1 (2efa728).
//!
//! Models come from the vendored catalog (`generated.rs`), which carries the
//! same data as upstream `ant-ling.models.ts`.

use std::sync::Arc;

use crate::api::openai_completions::OpenAiCompletions;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `antLingProvider()`.
pub fn ant_ling_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "ant-ling".to_owned(),
        name: Some("Ant Ling".to_owned()),
        base_url: Some("https://api.ant-ling.com/v1".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Ant Ling API key",
                &["ANT_LING_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("ant-ling").to_vec(),
        api: ProviderApi::Single(Arc::new(OpenAiCompletions)),
        ..Default::default()
    })
}
