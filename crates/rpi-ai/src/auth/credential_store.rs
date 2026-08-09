//! Port of `packages/ai/src/auth/credential-store.ts` @ pi 0.82.1 (2efa728).
//!
//! Default in-memory credential store. Writes are serialized per provider
//! (upstream: promise chain; here: a per-provider async mutex).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::resolve::ModelsError;
use super::types::{Credential, CredentialInfo, CredentialStore, ModifyFn};

/// `InMemoryCredentialStore` — apps inject persistent stores (T04).
#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<HashMap<String, Credential>>,
    /// Serialize `modify`/`delete` per provider id.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    async fn provider_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(provider_id.to_owned())
            .or_default()
            .clone()
    }
}

#[async_trait::async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError> {
        Ok(self.credentials.lock().await.get(provider_id).cloned())
    }

    async fn list(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
        Ok(self
            .credentials
            .lock()
            .await
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                credential_type: credential.credential_type(),
            })
            .collect())
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyFn,
    ) -> Result<Option<Credential>, ModelsError> {
        let lock = self.provider_lock(provider_id).await;
        let _guard = lock.lock().await;
        let current = self.credentials.lock().await.get(provider_id).cloned();
        let next = f(current.clone()).await?;
        if let Some(next) = next {
            self.credentials
                .lock()
                .await
                .insert(provider_id.to_owned(), next);
        }
        // Upstream resolves with `next ?? current`.
        Ok(self.credentials.lock().await.get(provider_id).cloned())
    }

    async fn delete(&self, provider_id: &str) -> Result<(), ModelsError> {
        let lock = self.provider_lock(provider_id).await;
        let _guard = lock.lock().await;
        self.credentials.lock().await.remove(provider_id);
        Ok(())
    }
}
