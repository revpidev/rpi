use std::sync::Arc;

use rpi_ai::auth::{
    Credential, CredentialStore, CredentialType, InMemoryCredentialStore, OAuthCredential,
};
use rpi_ai::models::{CreateModelsOptions, Models};
use rpi_ai::types::{Context, StopReason};
use serde_json::{Map, json};

async fn store_oauth(
    store: &Arc<InMemoryCredentialStore>,
    provider: &str,
    credential: OAuthCredential,
) {
    store
        .modify(
            provider,
            Arc::new(move |_| {
                let credential = credential.clone();
                Box::pin(async move { Ok(Some(Credential::OAuth(credential))) })
            }),
            None,
        )
        .await
        .expect("store credential");
}

fn oauth(access: &str, account_id: Option<&str>) -> OAuthCredential {
    let mut extra = Map::new();
    if let Some(account_id) = account_id {
        extra.insert("accountId".to_owned(), json!(account_id));
    }
    OAuthCredential {
        refresh: "test-refresh-token".to_owned(),
        access: access.to_owned(),
        expires: i64::MAX,
        extra,
    }
}

fn models(store: Arc<InMemoryCredentialStore>) -> Models {
    Models::new(Some(CreateModelsOptions {
        credentials: Some(store),
        ..Default::default()
    }))
}

#[tokio::test]
async fn codex_account_and_access_changes_produce_distinct_generations() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let models = models(store.clone());
    let provider = rpi_ai::providers::openai_codex::openai_codex_provider();
    let model = provider.get_models()[0].clone();
    models.set_provider(provider);
    store_oauth(&store, "openai-codex", oauth("access-a", Some("account-a"))).await;
    let first = models
        .get_resolved_request_auth(&model, None)
        .await
        .expect("resolve")
        .expect("configured");
    assert_eq!(first.credential_kind, CredentialType::Oauth);
    assert_eq!(first.effective_origin, "https://chatgpt.com");

    store_oauth(&store, "openai-codex", oauth("access-a", Some("account-b"))).await;
    let other_account = models
        .get_resolved_request_auth(&model, None)
        .await
        .expect("resolve")
        .expect("configured");
    assert_ne!(first.generation, other_account.generation);

    store_oauth(&store, "openai-codex", oauth("access-b", Some("account-b"))).await;
    let rotated = models
        .get_resolved_request_auth(&model, None)
        .await
        .expect("resolve")
        .expect("configured");
    assert_ne!(other_account.generation, rotated.generation);
    let debug = format!("{first:?} {other_account:?} {rotated:?}");
    assert!(!debug.contains("access-a"));
    assert!(!debug.contains("access-b"));
    assert!(!debug.contains("account-a"));
    assert!(!debug.contains("account-b"));

    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "jwt-account"}
        }))
        .expect("json"),
    );
    let access = format!("header.{payload}.signature");
    store_oauth(
        &store,
        "openai-codex",
        oauth(&access, Some("other-account")),
    )
    .await;
    let error = models
        .get_resolved_request_auth(&model, None)
        .await
        .expect_err("conflicting account must fail");
    assert!(error.message.contains("does not match"));
}

#[tokio::test]
async fn copilot_derives_its_native_origin_but_subscription_fails_on_a_proxy() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let models = models(store.clone());
    let provider = rpi_ai::providers::github_copilot::github_copilot_provider();
    let model = provider.get_models()[0].clone();
    models.set_provider(provider);
    let mut credential = oauth("copilot-access", None);
    credential
        .extra
        .insert("enterpriseUrl".to_owned(), json!("example.com"));
    store_oauth(&store, "github-copilot", credential).await;
    let resolved = models
        .get_resolved_request_auth(&model, None)
        .await
        .expect("resolve")
        .expect("configured");
    assert_eq!(resolved.effective_origin, "https://copilot-api.example.com");
    assert_eq!(
        resolved.auth.base_url.as_deref(),
        Some("https://copilot-api.example.com")
    );

    let mut proxy = model;
    proxy.base_url = "https://proxy.example.com/v1".to_owned();
    let error = models
        .get_auth(&proxy, None)
        .await
        .expect_err("proxy must not inherit subscription OAuth");
    assert!(error.message.contains("cannot inherit"));
    let message = models
        .stream(&proxy, &Context::default(), None)
        .result()
        .await
        .expect("stream error result");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("cannot inherit"))
    );
}
