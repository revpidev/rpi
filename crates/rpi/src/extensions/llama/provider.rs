//! Port of `packages/coding-agent/src/extensions/llama/provider.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! `createLlamaProvider` — the dynamic `"llama.cpp"` provider plus its
//! api-key auth (login/check/resolve). Streams delegate to the
//! openai-completions adapter (`rpi_ai::api::openai_completions::
//! OpenAiCompletions`), the Rust counterpart of upstream's
//! `stream`/`streamSimple` from `@earendil-works/pi-ai/compat`; no new
//! public surface in rpi-ai was needed.
//!
//! Intentional differences:
//! - `create_llama_provider` builds a controller around a concrete
//!   [`LlamaProvider`] (the `Provider` trait impl) with a `set_catalog`
//!   method — upstream returns `{ provider, setCatalog }` closures.
//! - Errors from URL normalization surface as [`LlamaError`] mapped into
//!   `ModelsError` on the auth/refresh paths (upstream lets them throw).
//! - The provider-level `baseUrl` is computed at construction; the
//!   `DEFAULT_LLAMA_SERVER_URL` constant is valid, so the fallback keeps the
//!   literal concatenation the normalize step would produce.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rpi_ai::api::openai_completions::OpenAiCompletions;
use rpi_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction, AuthPrompt, AuthResult,
    AuthType, Credential, ModelAuth, ModelsError, ModelsErrorCode, ProviderAuth,
};
use rpi_ai::models::{now_millis, Provider, ProviderStreams, RefreshModelsContext};
use rpi_ai::models_store::ModelsStoreEntry;
use rpi_ai::types::{
    ApiKind, Context, InputModality, MaxTokensField, Model, ModelCompat, ProviderEnv,
    SimpleStreamOptions, StreamOptions,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use tokio_util::sync::CancellationToken;

use super::client::{
    llama_inference_url, normalize_llama_server_url, LlamaClient, LlamaError, LlamaModelInfo,
    LlamaModelStatusValue,
};

/// `LLAMA_PROVIDER_ID` (provider.ts:13).
pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";

/// `DEFAULT_LLAMA_SERVER_URL` (provider.ts:14).
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";

fn lock(models: &Mutex<Vec<Model>>) -> std::sync::MutexGuard<'_, Vec<Model>> {
    models.lock().unwrap_or_else(|e| e.into_inner())
}

fn store_error(error: rpi_ai::error::AiError) -> ModelsError {
    ModelsError::new(ModelsErrorCode::ModelSource, error.to_string())
}

fn auth_error(error: LlamaError) -> ModelsError {
    ModelsError::new(ModelsErrorCode::Auth, error.message)
}

/// `credentialServerUrl(credential)` (provider.ts:15-18).
fn credential_server_url(
    credential: Option<&ApiKeyCredential>,
) -> Result<Option<String>, LlamaError> {
    let Some(value) = credential
        .and_then(|credential| credential.env.as_ref())
        .and_then(|env| env.get("LLAMA_BASE_URL"))
    else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    normalize_llama_server_url(value).map(Some)
}

/// `resolveServerUrl(ctx, credential)` (provider.ts:20-26).
async fn resolve_server_url(
    ctx: &dyn AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> Result<Option<String>, LlamaError> {
    let configured = match credential_server_url(credential)? {
        Some(server_url) => Some(server_url),
        None => ctx
            .env("LLAMA_BASE_URL")
            .await
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    };
    match configured {
        Some(value) => normalize_llama_server_url(&value).map(Some),
        None => Ok(None),
    }
}

/// `toPiModel(model, serverUrl)` (provider.ts:28-51).
fn to_pi_model(model: &LlamaModelInfo, server_url: &str) -> Result<Model, LlamaError> {
    let reported_context_window = model
        .meta
        .as_ref()
        .and_then(|meta| meta.n_ctx)
        .or_else(|| model.meta.as_ref().and_then(|meta| meta.n_ctx_train));
    let context_window = reported_context_window
        .filter(|window| *window > 0)
        .and_then(|window| u32::try_from(window).ok())
        .unwrap_or(128_000);
    let mut input = vec![InputModality::Text];
    if model
        .architecture
        .as_ref()
        .map(|architecture| &architecture.input_modalities)
        .is_some_and(|modalities| modalities.iter().any(|modality| modality == "image"))
    {
        input.push(InputModality::Image);
    }
    Ok(Model {
        id: model.id.clone(),
        name: model.id.clone(),
        api: ApiKind::from(ApiKind::OPENAI_COMPLETIONS),
        provider: LLAMA_PROVIDER_ID.to_owned(),
        base_url: llama_inference_url(server_url)?,
        reasoning: false,
        thinking_level_map: None,
        input,
        cost: rpi_ai::types::ModelCost::default(),
        context_window,
        max_tokens: context_window,
        headers: None,
        compat: Some(ModelCompat {
            supports_store: Some(false),
            supports_developer_role: Some(false),
            supports_reasoning_effort: Some(false),
            supports_usage_in_streaming: Some(false),
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            supports_strict_mode: Some(false),
            ..Default::default()
        }),
    })
}

/// `LlamaProvider` — the `Provider` implementation behind
/// `createLlamaProvider` (provider.ts:58-131).
pub struct LlamaProvider {
    models: Mutex<Vec<Model>>,
    base_url: String,
    auth: ProviderAuth,
    api: Arc<dyn ProviderStreams>,
}

impl LlamaProvider {
    fn new() -> Self {
        LlamaProvider {
            models: Mutex::new(Vec::new()),
            // `DEFAULT_LLAMA_SERVER_URL` is a valid http URL, so the fallback
            // is exactly what `llamaInferenceUrl` would produce.
            base_url: llama_inference_url(DEFAULT_LLAMA_SERVER_URL)
                .unwrap_or_else(|_| format!("{DEFAULT_LLAMA_SERVER_URL}/v1")),
            auth: ProviderAuth {
                api_key: Some(Arc::new(LlamaApiKeyAuth)),
                oauth: None,
            },
            api: Arc::new(OpenAiCompletions),
        }
    }

    /// `setCatalog` (provider.ts:61-63): only `status.value === "loaded"`
    /// entries are published as pi models.
    pub fn set_catalog(
        &self,
        catalog: &[LlamaModelInfo],
        server_url: &str,
    ) -> Result<(), LlamaError> {
        let mut models = Vec::new();
        for model in catalog {
            if model.status.value == LlamaModelStatusValue::LOADED {
                models.push(to_pi_model(model, server_url)?);
            }
        }
        *lock(&self.models) = models;
        Ok(())
    }
}

impl Provider for LlamaProvider {
    fn id(&self) -> &str {
        LLAMA_PROVIDER_ID
    }

    fn name(&self) -> &str {
        "llama.cpp"
    }

    fn base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn headers(&self) -> Option<&rpi_ai::types::ProviderHeaders> {
        None
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        lock(&self.models).clone()
    }

    /// `refreshModels` (provider.ts:113-128): restore the stored catalog,
    /// then — when network access is allowed, not cancelled, the credential
    /// is an api-key credential and it carries a `LLAMA_BASE_URL` — fetch
    /// the router catalog and persist it.
    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<Pin<Box<dyn Future<Output = Result<(), ModelsError>> + Send + '_>>> {
        let this = self;
        Some(Box::pin(async move {
            let stored = context.store.read().await.map_err(store_error)?;
            if let Some(stored) = stored {
                *lock(&this.models) = stored
                    .models
                    .into_iter()
                    .filter(|model| {
                        model.provider == LLAMA_PROVIDER_ID
                            && model.api.as_str() == ApiKind::OPENAI_COMPLETIONS
                    })
                    .collect();
            }

            let network_allowed = context.allow_network
                && !context
                    .signal
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                && matches!(context.credential.as_ref(), Some(Credential::ApiKey(_)));
            if !network_allowed {
                return Ok(());
            }
            let Some(Credential::ApiKey(credential)) = context.credential.as_ref() else {
                return Ok(());
            };
            let Some(server_url) = credential_server_url(Some(credential)).map_err(auth_error)?
            else {
                return Ok(());
            };
            let client =
                LlamaClient::new(&server_url, credential.key.as_deref()).map_err(auth_error)?;
            let catalog = client
                .list(false, context.signal.as_ref())
                .await
                .map_err(auth_error)?;
            this.set_catalog(&catalog, &server_url)
                .map_err(auth_error)?;
            if !context
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                let models = lock(&this.models).clone();
                context
                    .store
                    .write(ModelsStoreEntry {
                        models,
                        last_modified: None,
                        checked_at: Some(now_millis()),
                        etag: None,
                    })
                    .await
                    .map_err(store_error)?;
            }
            Ok(())
        }))
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.api.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.api.stream_simple(model, context, options)
    }
}

/// `auth.apiKey` of the llama provider (provider.ts:70-110).
struct LlamaApiKeyAuth;

#[async_trait::async_trait]
impl ApiKeyAuth for LlamaApiKeyAuth {
    fn name(&self) -> &str {
        "llama.cpp server"
    }

    fn supports_login(&self) -> bool {
        true
    }

    /// `login` (provider.ts:72-92): prompt for the server URL (placeholder
    /// `$LLAMA_BASE_URL` or the default), then an optional API key; verify
    /// by listing the router catalog; return the credential with the
    /// normalized URL in `env.LLAMA_BASE_URL`.
    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<ApiKeyCredential, ModelsError> {
        let fallback = std::env::var("LLAMA_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_LLAMA_SERVER_URL.to_owned());
        let entered = interaction
            .prompt(AuthPrompt::Text {
                message: "llama.cpp server URL".to_owned(),
                placeholder: Some(fallback.clone()),
                signal: interaction.signal(),
            })
            .await?;
        let entered = entered.trim();
        let server_url = normalize_llama_server_url(if entered.is_empty() {
            &fallback
        } else {
            entered
        })
        .map_err(auth_error)?;
        let api_key = interaction
            .prompt(AuthPrompt::Secret {
                message: "API key (optional)".to_owned(),
                placeholder: None,
                signal: interaction.signal(),
            })
            .await?;
        let api_key = api_key.trim();
        let key = (!api_key.is_empty()).then(|| api_key.to_owned());
        let client = LlamaClient::new(&server_url, key.as_deref()).map_err(auth_error)?;
        let signal = interaction.signal();
        client
            .list(false, signal.as_ref())
            .await
            .map_err(auth_error)?;
        Ok(ApiKeyCredential {
            key,
            env: Some(ProviderEnv::from([(
                "LLAMA_BASE_URL".to_owned(),
                server_url,
            )])),
        })
    }

    /// `check` (provider.ts:94-99).
    async fn check(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthCheck>, ModelsError> {
        if resolve_server_url(ctx, credential)
            .await
            .map_err(auth_error)?
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(AuthCheck {
            source: Some(if credential.is_some() {
                "stored credential".to_owned()
            } else {
                "LLAMA_BASE_URL".to_owned()
            }),
            kind: AuthType::ApiKey,
        }))
    }

    /// `resolve` (provider.ts:100-109).
    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let Some(server_url) = resolve_server_url(ctx, credential)
            .await
            .map_err(auth_error)?
        else {
            return Ok(None);
        };
        let api_key = credential
            .and_then(|credential| credential.key.clone())
            .or(ctx.env("LLAMA_API_KEY").await)
            .unwrap_or_else(|| "local".to_owned());
        let mut env = ProviderEnv::new();
        if let Some(credential_env) = credential.and_then(|credential| credential.env.clone()) {
            env.extend(credential_env);
        }
        env.insert("LLAMA_BASE_URL".to_owned(), server_url.clone());
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(api_key),
                headers: None,
                base_url: Some(llama_inference_url(&server_url).map_err(auth_error)?),
            },
            env: Some(env),
            source: Some(if credential.is_some() {
                "stored credential".to_owned()
            } else {
                "LLAMA_BASE_URL".to_owned()
            }),
        }))
    }
}

/// `LlamaProviderController` (provider.ts:53-56, 133): the provider plus its
/// `setCatalog` controller.
#[derive(Clone)]
pub struct LlamaProviderController {
    provider: Arc<LlamaProvider>,
}

impl LlamaProviderController {
    /// The provider as the trait object to register with
    /// `ModelRuntime::register_native_provider`.
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    /// `setCatalog`.
    pub fn set_catalog(
        &self,
        catalog: &[LlamaModelInfo],
        server_url: &str,
    ) -> Result<(), LlamaError> {
        self.provider.set_catalog(catalog, server_url)
    }
}

/// `createLlamaProvider()` (provider.ts:58).
pub fn create_llama_provider() -> LlamaProviderController {
    LlamaProviderController {
        provider: Arc::new(LlamaProvider::new()),
    }
}

/// The process-wide controller behind the built-in hidden extension
/// (extensions/index.ts: the single `llama.cpp` entry in
/// `builtInExtensions`). Upstream creates the provider inside the extension
/// factory and closes over it; rpi's registration seam
/// (`create_agent_session_services`) and the `/llama` command handler share
/// this instance instead (D-047).
pub fn shared_llama_provider() -> LlamaProviderController {
    static SHARED: std::sync::OnceLock<LlamaProviderController> = std::sync::OnceLock::new();
    SHARED.get_or_init(create_llama_provider).clone()
}

#[cfg(test)]
mod tests {
    use super::super::client::{LlamaArchitecture, LlamaModelMeta, LlamaModelStatusValue};
    use super::*;

    fn model_info(id: &str, status: &str) -> LlamaModelInfo {
        LlamaModelInfo {
            id: id.to_owned(),
            status: super::super::client::LlamaModelStatus {
                value: status.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Upstream: "exposes only loaded models with router metadata"
    /// (llama-extension.test.ts).
    #[test]
    fn exposes_only_loaded_models_with_router_metadata() {
        let controller = create_llama_provider();
        let mut loaded = model_info("loaded", LlamaModelStatusValue::LOADED);
        loaded.status.args = vec![
            "llama-server".to_owned(),
            "--n-gpu-layers".to_owned(),
            "999".to_owned(),
        ];
        loaded.architecture = Some(LlamaArchitecture {
            input_modalities: vec!["text".to_owned(), "image".to_owned()],
            output_modalities: Vec::new(),
        });
        loaded.meta = Some(LlamaModelMeta {
            n_ctx: Some(65536),
            n_ctx_train: Some(131072),
            ..Default::default()
        });
        controller
            .set_catalog(
                &[
                    loaded,
                    model_info("unloaded", LlamaModelStatusValue::UNLOADED),
                    model_info("loading", LlamaModelStatusValue::LOADING),
                ],
                "http://localhost:8080",
            )
            .expect("set catalog");

        let models = controller.provider().get_models();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.id, "loaded");
        assert_eq!(model.provider, LLAMA_PROVIDER_ID);
        assert_eq!(model.api.as_str(), "openai-completions");
        assert_eq!(model.base_url, "http://localhost:8080/v1");
        assert_eq!(model.context_window, 65536);
        assert_eq!(model.max_tokens, 65536);
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        let compat = model.compat.as_ref().expect("compat");
        assert_eq!(compat.supports_store, Some(false));
        assert_eq!(compat.supports_developer_role, Some(false));
        assert_eq!(compat.supports_reasoning_effort, Some(false));
        assert_eq!(compat.supports_usage_in_streaming, Some(false));
        assert_eq!(compat.supports_strict_mode, Some(false));
        assert_eq!(compat.max_tokens_field, Some(MaxTokensField::MaxTokens));
    }

    /// `toPiModel` context fallback (provider.ts:29-30): missing/zero
    /// reported context falls back to 128000; `n_ctx_train` fills in for a
    /// missing `n_ctx`.
    #[test]
    fn to_pi_model_context_window_fallbacks() {
        let mut model = model_info("m", LlamaModelStatusValue::LOADED);
        let converted = to_pi_model(&model, "http://localhost:8080").expect("convert");
        assert_eq!(converted.context_window, 128_000);

        model.meta = Some(LlamaModelMeta {
            n_ctx_train: Some(8192),
            ..Default::default()
        });
        let converted = to_pi_model(&model, "http://localhost:8080").expect("convert");
        assert_eq!(converted.context_window, 8192);

        model.meta = Some(LlamaModelMeta {
            n_ctx: Some(0),
            ..Default::default()
        });
        let converted = to_pi_model(&model, "http://localhost:8080").expect("convert");
        assert_eq!(converted.context_window, 128_000);
    }

    struct StaticAuthContext {
        vars: std::collections::HashMap<String, String>,
    }

    #[async_trait::async_trait]
    impl AuthContext for StaticAuthContext {
        async fn env(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        async fn file_exists(&self, _path: &str) -> bool {
            false
        }
    }

    /// Upstream: "stays dormant until configured…" — the `check`/`resolve`
    /// half (the login half needs a loopback server and lives in
    /// `tests/llama_extension.rs`).
    #[tokio::test]
    async fn stays_dormant_until_configured() {
        let controller = create_llama_provider();
        let auth = controller
            .provider()
            .auth()
            .api_key
            .clone()
            .expect("api key auth");
        let empty = StaticAuthContext {
            vars: Default::default(),
        };
        assert!(auth.check(&empty, None).await.expect("check").is_none());
        assert!(auth.resolve(&empty, None).await.expect("resolve").is_none());

        // LLAMA_BASE_URL env configures without a stored credential.
        let with_env = StaticAuthContext {
            vars: std::collections::HashMap::from([(
                "LLAMA_BASE_URL".to_owned(),
                "http://127.0.0.1:8080/v1/".to_owned(),
            )]),
        };
        let check = auth.check(&with_env, None).await.expect("check");
        assert_eq!(
            check.and_then(|check| check.source).as_deref(),
            Some("LLAMA_BASE_URL")
        );
        let resolved = auth
            .resolve(&with_env, None)
            .await
            .expect("resolve")
            .expect("configured");
        assert_eq!(
            resolved.auth.base_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
        // No key anywhere: the llama.cpp router default is "local"
        // (provider.ts:103).
        assert_eq!(resolved.auth.api_key.as_deref(), Some("local"));
        assert_eq!(resolved.source.as_deref(), Some("LLAMA_BASE_URL"));
    }

    /// `resolve` precedence (provider.ts:100-109): stored credential wins;
    /// `LLAMA_API_KEY` fills a keyless credential.
    #[tokio::test]
    async fn resolve_merges_credential_and_environment() {
        let controller = create_llama_provider();
        let auth = controller
            .provider()
            .auth()
            .api_key
            .clone()
            .expect("api key auth");
        let with_key = StaticAuthContext {
            vars: std::collections::HashMap::from([(
                "LLAMA_API_KEY".to_owned(),
                "env-key".to_owned(),
            )]),
        };
        let credential = ApiKeyCredential {
            key: None,
            env: Some(ProviderEnv::from([(
                "LLAMA_BASE_URL".to_owned(),
                "http://10.0.0.2:9000".to_owned(),
            )])),
        };
        let resolved = auth
            .resolve(&with_key, Some(&credential))
            .await
            .expect("resolve")
            .expect("configured");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("env-key"));
        assert_eq!(
            resolved.auth.base_url.as_deref(),
            Some("http://10.0.0.2:9000/v1")
        );
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));
        assert_eq!(
            resolved
                .env
                .as_ref()
                .and_then(|env| env.get("LLAMA_BASE_URL"))
                .map(String::as_str),
            Some("http://10.0.0.2:9000")
        );
    }
}
