//! Library error enum for `pir` (coding-standards §5.1).
//!
//! Binary/mode entry points aggregate with `anyhow` instead (§5.1); this enum
//! is for the SDK surface where callers match on structured errors.

/// Error type for `pir` lib (SDK) fallible operations.
#[derive(Debug, thiserror::Error)]
pub enum PirError {
    #[error("session error: {0}")]
    Session(String),

    #[error("settings error: {0}")]
    Settings(String),

    #[error("resource loading error: {0}")]
    Resource(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl PirError {
    /// Raw message without the `Display` prefix — upstream surfaces
    /// `error.message` verbatim (print/RPC error paths).
    pub fn raw_message(&self) -> String {
        match self {
            PirError::Session(message)
            | PirError::Settings(message)
            | PirError::Resource(message) => message.clone(),
            other => other.to_string(),
        }
    }
}
