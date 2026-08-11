//! Port of `packages/ai/src/auth/credential-store.ts` @ pi 0.82.1 (2efa728).
//!
//! Default in-memory credential store. Writes are serialized per provider
//! (upstream: promise chain; here: a per-provider async mutex).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::resolve::ModelsError;
use super::types::{AuthOperationOptions, Credential, CredentialInfo, CredentialStore, ModifyFn};

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
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        Ok(self.credentials.lock().await.get(provider_id).cloned())
    }

    async fn list(
        &self,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, ModelsError> {
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
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        // T21b: a cancelled modify queued behind another mutation rejects
        // without ever running its task (credential-store.ts:14-28 @ 4181f66).
        AuthOperationOptions::throw_if_cancelled(options)?;
        let lock = self.provider_lock(provider_id).await;
        // Queue-cancellable: cancel during lock-wait rejects without running
        // the task; after lock acquisition, re-check before executing.
        let _guard = tokio::select! {
            biased;
            _ = cancel_future(options) => return Err(ModelsError::aborted()),
            guard = lock.lock() => guard,
        };
        AuthOperationOptions::throw_if_cancelled(options)?;
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

    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), ModelsError> {
        // T21b: a cancelled delete queued behind another mutation rejects
        // without ever running its task (credential-store.ts:14-28 @ 4181f66).
        AuthOperationOptions::throw_if_cancelled(options)?;
        let lock = self.provider_lock(provider_id).await;
        // Queue-cancellable: cancel during lock-wait rejects without running
        // the task; after lock acquisition, re-check before executing.
        let _guard = tokio::select! {
            biased;
            _ = cancel_future(options) => return Err(ModelsError::aborted()),
            guard = lock.lock() => guard,
        };
        AuthOperationOptions::throw_if_cancelled(options)?;
        self.credentials.lock().await.remove(provider_id);
        Ok(())
    }
}

/// Boxed future that resolves when the cancellation token in `options` fires,
/// or never when there is no token. Used in `select!` to cancel a queued
/// `modify`/`delete` while it waits for the per-provider lock.
pub(crate) fn cancel_future(
    options: Option<&AuthOperationOptions>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    match options.and_then(|o| o.signal.as_ref()) {
        Some(token) => Box::pin(token.cancelled()),
        None => Box::pin(std::future::pending()),
    }
}
