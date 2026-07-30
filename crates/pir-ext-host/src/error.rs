//! Main error enum for `pir-ext-host` (coding-standards §5.1).

/// Error type for extension-host operations (`ExtError` in coding-standards
/// §4.3 / design doc §7.1).
#[derive(Debug, thiserror::Error)]
pub enum ExtError {
    #[error("extension load failed: {0}")]
    Load(String),

    #[error("extension call failed: {0}")]
    Call(String),

    #[error("capability denied: {0}")]
    CapabilityDenied(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
