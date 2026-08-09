//! Port of `packages/ai/src/providers/cloudflare-workers-ai.ts` @ pi 0.82.1
//! (2efa728) — Cloudflare Workers AI: OpenAI Completions transport, wrapped
//! so the `{CLOUDFLARE_ACCOUNT_ID}` base-URL placeholder materializes from
//! the resolved provider env before dispatch (see `cloudflare_stream`).
//!
//! Auth (`cloudflare-auth.ts`) lands in `crate::auth::cloudflare_auth`:
//! `CLOUDFLARE_API_KEY` + `CLOUDFLARE_ACCOUNT_ID`, request auth via api key.

use std::sync::Arc;

use crate::api::openai_completions::OpenAiCompletions;
use crate::auth::cloudflare_auth::cloudflare_workers_ai_auth;
use crate::auth::ProviderAuth;
use crate::generated::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Provider, ProviderApi};

use super::cloudflare_stream::cloudflare_streams;

/// `cloudflareWorkersAIProvider()`.
pub fn cloudflare_workers_ai_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "cloudflare-workers-ai".to_owned(),
        name: Some("Cloudflare Workers AI".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(cloudflare_workers_ai_auth()),
            oauth: None,
        },
        models: get_builtin_models("cloudflare-workers-ai").to_vec(),
        api: ProviderApi::Single(cloudflare_streams(Arc::new(OpenAiCompletions))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ApiKind;

    #[test]
    fn catalog_base_urls_carry_the_account_placeholder() {
        let models = get_builtin_models("cloudflare-workers-ai");
        assert!(!models.is_empty());
        for model in models {
            assert_eq!(model.api.as_str(), ApiKind::OPENAI_COMPLETIONS);
            assert!(
                model.base_url.contains("{CLOUDFLARE_ACCOUNT_ID}"),
                "{}: {}",
                model.id,
                model.base_url
            );
        }
    }
}
