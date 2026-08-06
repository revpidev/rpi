//! Port of `packages/ai/src/providers/vercel-ai-gateway.ts` @ pi 0.82.1
//! (2efa728) — Vercel AI Gateway, Anthropic Messages transport.

use std::sync::Arc;

use crate::api::anthropic_messages::AnthropicMessages;
use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

/// `vercelAIGatewayProvider()`.
pub fn vercel_ai_gateway_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "vercel-ai-gateway".to_owned(),
        name: Some("Vercel AI Gateway".to_owned()),
        base_url: Some("https://ai-gateway.vercel.sh".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Vercel AI Gateway API key",
                &["AI_GATEWAY_API_KEY"],
            ))),
            oauth: None,
        },
        models: get_builtin_models("vercel-ai-gateway").to_vec(),
        api: ProviderApi::Single(Arc::new(AnthropicMessages)),
    })
}
