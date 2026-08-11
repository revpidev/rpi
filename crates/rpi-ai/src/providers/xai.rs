//! Port of `packages/ai/src/providers/xai.ts` @ pi 0.82.1 (2efa728) — T13 W4.
//!
//! Mixed-API provider: `openai-completions` and `openai-responses` (Grok 4.5),
//! dispatched on `model.api` (upstream api map). Models come from the vendored
//! catalog (upstream `xai.models.ts` is a generated `flattenModelCatalog`
//! re-export of `providers/data/xai.json`, corrections baked in).
//!
//! Upstream also wires `oauth: lazyOAuth({ name: "xAI (Grok/X subscription)",
//! loginLabel: "Sign in with SuperGrok or X Premium", load: loadXaiOAuth })`;
//! rpi wires the flow directly (T13 W5, deviation D-031 closed) as
//! [`crate::auth::oauth::xai`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::openai_completions::OpenAiCompletions;
use crate::api::openai_responses::OpenAiResponses;
use crate::auth::oauth::xai_oauth;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{
    create_provider, CreateProviderOptions, Provider, ProviderApi, ProviderStreams,
};
use crate::types::ApiKind;

/// `xaiProvider()`.
pub fn xai_provider() -> Arc<dyn Provider> {
    let api: HashMap<String, Arc<dyn ProviderStreams>> = HashMap::from([
        (
            ApiKind::OPENAI_COMPLETIONS.to_owned(),
            Arc::new(OpenAiCompletions) as Arc<dyn ProviderStreams>,
        ),
        (
            ApiKind::OPENAI_RESPONSES.to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn ProviderStreams>,
        ),
    ]);
    create_provider(CreateProviderOptions {
        id: "xai".to_owned(),
        name: Some("xAI".to_owned()),
        base_url: Some("https://api.x.ai/v1".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth("xAI API key", &["XAI_API_KEY"]))),
            oauth: Some(xai_oauth()),
        },
        models: get_builtin_models("xai").to_vec(),
        api: ProviderApi::Map(api),
        ..Default::default()
    })
}
