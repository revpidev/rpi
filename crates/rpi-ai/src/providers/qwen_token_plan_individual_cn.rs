//! Port of `packages/ai/src/providers/qwen-token-plan-individual-cn.ts` @ pi
//! v1.5.2 (`b4afdddf`).

use std::sync::Arc;

use crate::api::openai_completions::OpenAiCompletions;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};

/// `qwenTokenPlanIndividualCnProvider()`.
pub fn qwen_token_plan_individual_cn_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "qwen-token-plan-individual-cn".to_owned(),
        name: Some("Qwen Token Plan Individual CN".to_owned()),
        base_url: Some(
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1".to_owned(),
        ),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Qwen Token Plan Individual CN API key",
                &["QWEN_TOKEN_PLAN_CN_API_KEY"],
            ))),
            oauth: None,
        },
        shared_all_models: Some(crate::generated::get_builtin_all_models_shared(
            "qwen-token-plan-individual-cn",
        )),
        api: ProviderApi::Single(Arc::new(OpenAiCompletions)),
        ..Default::default()
    })
}
