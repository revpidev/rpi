//! Port of `packages/ai/src/auth/` @ pi 0.82.1 (2efa728).
//!
//! Auth interfaces (credential types, store, resolution) plus the T04 part 1
//! building blocks: the key-value config DSL (`config_value`, port of
//! `coding-agent/src/core/resolve-config-value.ts`), the auth.json-backed
//! credential store (`file_store`, port of
//! `coding-agent/src/core/auth-storage.ts`), the known env-var table
//! (`env_keys`, port of `env-api-keys.ts`), standard provider auth helpers
//! (`helpers` / `anthropic_auth`) and the login interaction surface
//! (`interaction`). OAuth flows land with T04 part 2.

pub mod anthropic_auth;
pub mod cloudflare_auth;
pub mod config_value;
pub mod credential_store;
pub mod env_keys;
pub mod file_store;
pub mod helpers;
pub mod interaction;
pub mod oauth;
pub mod resolve;
pub mod types;

pub use anthropic_auth::{anthropic_api_key_auth, AnthropicApiKeyAuth};
pub use credential_store::InMemoryCredentialStore;
pub use env_keys::{
    find_env_keys, get_env_api_key, ANTHROPIC_API_KEY_ENV, ANTHROPIC_AUTH_TOKEN_ENV,
    ANTHROPIC_OAUTH_TOKEN_ENV,
};
pub use file_store::{
    read_stored_credential, Backend, FileAuthStorageBackend, FileCredentialStore,
    InMemoryAuthStorageBackend,
};
pub use helpers::{env_api_key_auth, EnvApiKeyAuth};
pub use interaction::{AuthEvent, AuthInfoLink, AuthInteraction, AuthPrompt, SelectOption};
pub use oauth::{anthropic_oauth, OAuthCallbackServer};
pub use resolve::{resolve_provider_auth, AuthResolutionOverrides, ModelsError, ModelsErrorCode};
pub use types::*;
