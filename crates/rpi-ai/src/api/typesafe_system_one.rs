//! TypeSafe's native System One protocol (port of
//! `packages/ai/src/api/typesafe-system-one.ts` @ 4a080252f). OpenRouter
//! serves the same protocol, so both providers use this API with different
//! base URLs.

use std::sync::Arc;

use url::Url;

use crate::api::classifier_shared::is_record;
use crate::api::system_one_shared::{
    ResolvedSystemOneCompat, SystemOneTransport, classify_system_one, typesafe_payload,
};
use crate::types::{
    ClassifierContext, ClassifierModel, ClassifierOptions, ClassifierResult, ProviderClassifier,
    SystemOneImageInput,
};

pub const TYPESAFE_SYSTEM_ONE_API: &str = "typesafe-system-one";

fn detect_compat(model: &ClassifierModel) -> ResolvedSystemOneCompat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        ResolvedSystemOneCompat {
            image_input: SystemOneImageInput::StateImageUrls,
            send_image_opt_in_header: false,
        }
    } else {
        ResolvedSystemOneCompat {
            image_input: SystemOneImageInput::Assets,
            send_image_opt_in_header: true,
        }
    }
}

fn url(model: &ClassifierModel) -> Result<Url, String> {
    Url::parse(&format!(
        "{}/systemone",
        model.base_url.trim_end_matches('/')
    ))
    .map_err(|error| error.to_string())
}

fn output(body: &serde_json::Value) -> Result<serde_json::Value, String> {
    if !is_record(body) {
        return Err("System One API returned an unexpected response".to_owned());
    }
    Ok(body.clone())
}

static TRANSPORT: SystemOneTransport = SystemOneTransport {
    api: TYPESAFE_SYSTEM_ONE_API,
    label: "System One API",
    detect_compat,
    url,
    payload: typesafe_payload,
    output,
};

/// The `typesafe-system-one` api implementation (unit struct).
#[derive(Debug, Clone, Copy, Default)]
pub struct TypesafeSystemOne;

impl ProviderClassifier for TypesafeSystemOne {
    fn classify(
        &self,
        model: &ClassifierModel,
        context: &ClassifierContext,
        options: Option<&ClassifierOptions>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClassifierResult> + Send + 'static>>
    {
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned();
        Box::pin(async move {
            classify_system_one(&TRANSPORT, &model, &context, options.as_ref()).await
        })
    }
}

/// `typesafeSystemOneApi()`.
pub fn typesafe_system_one_api() -> Arc<dyn ProviderClassifier> {
    Arc::new(TypesafeSystemOne)
}
