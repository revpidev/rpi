//! Port of `packages/ai/src/providers/openrouter.ts` @ pi 1.4.0
//! (4a080252f), including the per-key `/models/user` catalog from ee409a85f.

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::api::anthropic_messages::AnthropicMessages;
use crate::api::openai_completions::OpenAiCompletions;
use crate::api::openrouter_images::openrouter_images_v6_api;
use crate::api::typesafe_system_one::{TYPESAFE_SYSTEM_ONE_API, typesafe_system_one_api};
use crate::auth::oauth::openrouter_oauth;
use crate::auth::{Credential, ModelsError, ModelsErrorCode, ProviderAuth, env_api_key_auth};
use crate::generated::{get_builtin_all_models, get_builtin_models};
use crate::models::{
    CreateProviderOptions, Provider, ProviderApi, ProviderStreams, RefreshModelsContext,
    create_provider,
};
use crate::providers::openrouter_api::{OpenRouterModel, open_router_cost};
use crate::types::{AnyModel, ApiKind, ModelType};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_USER_MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `openrouterProvider()`.
pub fn openrouter_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "openrouter".to_owned(),
        name: Some("OpenRouter".to_owned()),
        base_url: Some(OPENROUTER_BASE_URL.to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "OpenRouter API key",
                &["OPENROUTER_API_KEY"],
            ))),
            oauth: Some(openrouter_oauth()),
        },
        models: get_builtin_models("openrouter").to_vec(),
        all_models: get_builtin_all_models("openrouter").to_vec(),
        classifiers: HashMap::from([(
            TYPESAFE_SYSTEM_ONE_API.to_owned(),
            typesafe_system_one_api(),
        )]),
        images: HashMap::from([("openrouter-images".to_owned(), openrouter_images_v6_api())]),
        api: ProviderApi::Map(api_map()),
        ..Default::default()
    })
}

/// Mixed-API dispatch table (openrouter.ts:81-84).
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
    ])
}

/// Irreversible namespace for a credential-specific catalog cache. The key
/// itself is never included in the namespace, logs, or persisted entry.
pub fn credential_fingerprint(credential: Option<&Credential>) -> Option<String> {
    let (kind, secret) = match credential {
        Some(Credential::ApiKey(credential)) => ("api_key", credential.key.as_deref()),
        Some(Credential::OAuth(credential)) => ("oauth", Some(credential.access.as_str())),
        None => ("", None),
    };
    let secret = secret.filter(|secret| !secret.is_empty())?;
    let mut hasher = Sha256::new();
    hasher.update(b"rpi-openrouter-catalog-v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(secret.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn credential_key(credential: Option<&Credential>) -> Option<&str> {
    match credential {
        Some(Credential::ApiKey(credential)) => credential.key.as_deref(),
        Some(Credential::OAuth(credential)) => Some(credential.access.as_str()),
        None => None,
    }
    .filter(|key| !key.is_empty())
}

/// `personalizeOpenRouterCatalog`: restrict chat entries to the models the
/// key may use, preserving image/classifier entries and catalog fallbacks.
pub async fn personalize_openrouter_catalog(
    catalog: &[AnyModel],
    context: &RefreshModelsContext,
) -> Result<Vec<AnyModel>, ModelsError> {
    let Some(key) = credential_key(context.credential.as_ref()) else {
        return Ok(catalog.to_vec());
    };
    let base_url = context.base_url.as_deref().unwrap_or(OPENROUTER_BASE_URL);
    let url = url::Url::parse(&format!("{}/models/user", base_url.trim_end_matches('/'))).map_err(
        |error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                "Invalid OpenRouter base URL",
                &error.to_string(),
            )
        },
    )?;
    let client = reqwest::Client::builder()
        .timeout(OPENROUTER_USER_MODELS_TIMEOUT)
        .build()
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                "Failed to create OpenRouter model list client",
                &error.to_string(),
            )
        })?;
    let response = tokio::select! {
        biased;
        () = context.signal.cancelled() => return Err(ModelsError::aborted()),
        response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .send() => response.map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::ModelSource,
                    "OpenRouter model list request failed",
                    &error.to_string(),
                )
            })?,
    };
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::NOT_FOUND
    ) {
        return Ok(catalog.to_vec());
    }
    if !status.is_success() {
        return Err(ModelsError::new(
            ModelsErrorCode::ModelSource,
            format!("OpenRouter model list request failed: {}", status.as_u16()),
        ));
    }
    let body: serde_json::Value = response.json().await.map_err(|error| {
        ModelsError::with_cause(
            ModelsErrorCode::ModelSource,
            "Invalid OpenRouter model list response",
            &error.to_string(),
        )
    })?;
    let entries = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::ModelSource,
                "Invalid OpenRouter model list response",
            )
        })?;
    let allowed: HashMap<String, OpenRouterModel> = entries
        .iter()
        .filter_map(|entry| serde_json::from_value::<OpenRouterModel>(entry.clone()).ok())
        .map(|entry| (entry.id.clone(), entry))
        .collect();

    Ok(catalog
        .iter()
        .filter_map(|model| {
            if model.model_type() != ModelType::Chat {
                return Some(model.clone());
            }
            let chat = model.as_chat()?;
            let entry = allowed.get(&chat.id)?;
            let mut model = chat.clone();
            let top = entry.top_provider.as_ref();
            model.context_window = top
                .and_then(|top| top.context_length)
                .filter(|limit| *limit > 0)
                .or(entry.context_length.filter(|limit| *limit > 0))
                .unwrap_or(model.context_window);
            model.max_tokens = top
                .and_then(|top| top.max_completion_tokens)
                .filter(|limit| *limit > 0)
                .unwrap_or(model.max_tokens);
            if let Some(pricing) = &entry.pricing {
                model.cost = open_router_cost(Some(pricing));
            }
            Some(AnyModel::Chat(model))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::auth::{ApiKeyCredential, OAuthCredential};
    use crate::generated::get_builtin_model;
    use crate::models::{PublishHandle, PublishShared};
    use crate::models_store::{InMemoryModelsStore, ModelsStore};
    use crate::types::{ModelCostRates, ModelThinkingLevel};

    /// `650e7a612` / #8454: the provider dispatches on both apis.
    #[test]
    fn api_map_covers_anthropic_and_completions() {
        let map = api_map();
        for api in [ApiKind::ANTHROPIC_MESSAGES, ApiKind::OPENAI_COMPLETIONS] {
            assert!(map.contains_key(api), "missing dispatch entry {api}");
        }
        assert_eq!(map.len(), 2, "exactly the two upstream entries");
    }

    #[test]
    fn credential_fingerprint_is_namespaced_and_does_not_expose_the_key() {
        let api_key = Credential::ApiKey(ApiKeyCredential {
            key: Some("sk-or-test".to_owned()),
            env: None,
        });
        let oauth = Credential::OAuth(OAuthCredential {
            refresh: "refresh".to_owned(),
            access: "sk-or-test".to_owned(),
            expires: i64::MAX,
            extra: Default::default(),
        });
        let first = credential_fingerprint(Some(&api_key)).expect("fingerprint");
        assert!(!first.contains("sk-or-test"));
        assert_eq!(first.len(), 64);
        assert_ne!(first, credential_fingerprint(Some(&oauth)).expect("oauth"));
        assert_ne!(
            first,
            credential_fingerprint(Some(&Credential::ApiKey(ApiKeyCredential {
                key: Some("sk-other".to_owned()),
                env: None,
            })))
            .expect("other key")
        );
        assert_eq!(credential_fingerprint(None), None);
    }

    fn chat_model(id: &str) -> AnyModel {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "api": "openai-completions", "provider": "openrouter",
            "baseUrl": "https://openrouter.ai/api/v1", "reasoning": false, "input": ["text"],
            "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.0},
            "contextWindow": 200000, "maxTokens": 50000
        }))
        .expect("chat model")
    }

    fn refresh_context(base_url: &str) -> RefreshModelsContext {
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let signal = tokio_util::sync::CancellationToken::new();
        let shared = Arc::new(PublishShared {
            provider_id: "openrouter".to_owned(),
            generation: 1,
            signal: signal.clone(),
            store,
            chain: Arc::new(tokio::sync::Mutex::new(None)),
            refresh_generations: Arc::new(std::sync::RwLock::new(HashMap::from([(
                "openrouter".to_owned(),
                1,
            )]))),
        });
        RefreshModelsContext {
            credential: Some(Credential::ApiKey(ApiKeyCredential {
                key: Some("sk-or-test".to_owned()),
                env: None,
            })),
            stored: None,
            publish: PublishHandle { shared },
            allow_network: true,
            force: None,
            signal,
            base_url: Some(base_url.to_owned()),
        }
    }

    async fn serve_once(
        status: u16,
        body: serde_json::Value,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = vec![0; 8192];
            let read = socket.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            request_tx.send(request).expect("record request");
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });
        (format!("http://{address}/api/v1"), request_rx)
    }

    /// Port of `openrouter-user-models.test.ts`: per-key filtering, limits,
    /// prices, negative router prices, and non-chat pass-through.
    #[tokio::test]
    async fn personalize_catalog_uses_regional_url_and_key_specific_metadata() {
        let (base_url, request) = serve_once(
            200,
            serde_json::json!({"data": [
                {"id": "listed", "context_length": 1000000,
                 "top_provider": {"context_length": 262144, "max_completion_tokens": 32768},
                 "pricing": {"prompt": "0.000003", "completion": "0.000015"}},
                {"id": "no-limits", "context_length": 0,
                 "top_provider": {"context_length": null, "max_completion_tokens": null}},
                {"id": "router", "pricing": {"prompt": "-1", "completion": "-1"}}
            ]}),
        )
        .await;
        let classifier: AnyModel = serde_json::from_value(serde_json::json!({
            "type": "classifier", "id": "classifier", "name": "c", "api": "typesafe-system-one",
            "provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1",
            "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 64000
        }))
        .expect("classifier");
        let catalog = vec![
            chat_model("listed"),
            chat_model("no-limits"),
            chat_model("router"),
            chat_model("blocked"),
            classifier,
        ];
        let result = personalize_openrouter_catalog(&catalog, &refresh_context(&base_url))
            .await
            .expect("personalized catalog");
        let ids: Vec<&str> = result.iter().map(AnyModel::id).collect();
        assert_eq!(ids, ["listed", "no-limits", "router", "classifier"]);
        let listed = result[0].as_chat().expect("listed");
        assert_eq!(listed.context_window, 262_144);
        assert_eq!(listed.max_tokens, 32_768);
        assert_eq!(listed.cost.rates.input, 3.0);
        assert_eq!(listed.cost.rates.output, 15.0);
        assert_eq!(
            result[1].as_chat().expect("no limits").context_window,
            200_000
        );
        assert_eq!(
            result[2].as_chat().expect("router").cost.rates,
            ModelCostRates::default()
        );
        assert!(matches!(result[3], AnyModel::Classifier(_)));
        let request = request.await.expect("request");
        assert!(request.starts_with("GET /api/v1/models/user "), "{request}");
        assert!(
            request.contains("authorization: Bearer sk-or-test"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn personalize_catalog_preserves_catalog_on_rejection_or_missing_route() {
        for status in [401, 403, 404] {
            let (base_url, request) = serve_once(status, serde_json::Value::Null).await;
            let catalog = vec![chat_model("a")];
            let result = personalize_openrouter_catalog(&catalog, &refresh_context(&base_url))
                .await
                .expect("fallback");
            assert_eq!(result, catalog);
            let _ = request.await.expect("request");
        }
    }

    #[test]
    fn grok_xhigh_metadata_remains_available() {
        let model = get_builtin_model("xai", "grok-4.5").expect("grok");
        assert_eq!(
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ModelThinkingLevel::Xhigh)),
            Some(&Some("xhigh".to_owned()))
        );
    }
}
