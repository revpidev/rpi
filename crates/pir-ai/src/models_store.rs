//! Port of `packages/ai/src/models-store.ts` @ pi 0.82.1 (2efa728), plus a
//! JSON-file-backed persistence skeleton (T03; the remote overlay/refresh
//! flow lands with T13).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::error::AiError;
use crate::types::Model;

/// `ModelsStoreEntry`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    /// Unix timestamp from the remote catalog's Last-Modified header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,
    /// Unix timestamp of the last completed remote check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// Opaque validator from the remote catalog's ETag header, stored verbatim
    /// (quotes included) and echoed back as If-None-Match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// `ModelsStore` — persistent model catalogs keyed by provider ID.
#[async_trait::async_trait]
pub trait ModelsStore: Send + Sync {
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, AiError>;
    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> Result<(), AiError>;
    async fn delete(&self, provider_id: &str) -> Result<(), AiError>;
}

/// `ProviderModelsStore` — store scoped to one provider.
#[async_trait::async_trait]
pub trait ProviderModelsStore: Send + Sync {
    async fn read(&self) -> Result<Option<ModelsStoreEntry>, AiError>;
    async fn write(&self, entry: ModelsStoreEntry) -> Result<(), AiError>;
    async fn delete(&self) -> Result<(), AiError>;
}

/// `InMemoryModelsStore`.
#[derive(Debug, Default)]
pub struct InMemoryModelsStore {
    entries: Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, AiError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider_id)
            .cloned())
    }

    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> Result<(), AiError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(provider_id.to_owned(), entry);
        Ok(())
    }

    async fn delete(&self, provider_id: &str) -> Result<(), AiError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider_id);
        Ok(())
    }
}

/// JSON-file persistence skeleton. Layout: `{ "providers": { "<id>":
/// ModelsStoreEntry } }` in a single file, written atomically (tmp + rename).
///
/// The upstream package defines only the interface (apps provide the store);
/// this is the pir-side persistent implementation used by the remote catalog
/// overlay in T13. The on-disk shape is internal to pir (not a Pi wire
/// format).
pub struct JsonFileModelsStore {
    path: PathBuf,
    state: Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl JsonFileModelsStore {
    /// Loads the store file; a missing file starts empty.
    pub async fn load(path: PathBuf) -> Result<Self, AiError> {
        let state = match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let file: JsonFileModelsStoreFile = serde_json::from_str(&content)?;
                file.providers
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                return Err(AiError::ModelCatalog(format!(
                    "failed to read models store {}: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    async fn persist(&self) -> Result<(), AiError> {
        let snapshot = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let content = serde_json::to_string_pretty(&JsonFileModelsStoreFile {
            providers: snapshot,
        })?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                AiError::ModelCatalog(format!(
                    "failed to create models store directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, content).await.map_err(|error| {
            AiError::ModelCatalog(format!(
                "failed to write models store {}: {error}",
                tmp.display()
            ))
        })?;
        tokio::fs::rename(&tmp, &self.path).await.map_err(|error| {
            AiError::ModelCatalog(format!(
                "failed to persist models store {}: {error}",
                self.path.display()
            ))
        })?;
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JsonFileModelsStoreFile {
    #[serde(default)]
    providers: HashMap<String, ModelsStoreEntry>,
}

#[async_trait::async_trait]
impl ModelsStore for JsonFileModelsStore {
    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, AiError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider_id)
            .cloned())
    }

    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> Result<(), AiError> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(provider_id.to_owned(), entry);
        self.persist().await
    }

    async fn delete(&self, provider_id: &str) -> Result<(), AiError> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider_id);
        self.persist().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_json_file_models_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pir-models-store-{}", std::process::id()));
        let path = dir.join("models-store.json");
        let store = JsonFileModelsStore::load(path.clone())
            .await
            .expect("load empty");
        let entry = ModelsStoreEntry {
            models: vec![],
            last_modified: Some(1),
            checked_at: Some(2),
            etag: Some("\"abc\"".to_owned()),
        };
        store.write("openai", entry.clone()).await.expect("write");
        let loaded = JsonFileModelsStore::load(path.clone())
            .await
            .expect("reload");
        assert_eq!(loaded.read("openai").await.expect("read"), Some(entry));
        loaded.delete("openai").await.expect("delete");
        assert_eq!(loaded.read("openai").await.expect("read"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
