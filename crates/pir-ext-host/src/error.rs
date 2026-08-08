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

    /// Host action / UI bridge invoked before the host bound an
    /// implementation — mirrors the throwing stubs of
    /// `createExtensionRuntime` (loader.ts:170-173) and
    /// `noOpUIContext` being absent.
    #[error("extension runtime not initialized: {0}")]
    Unbound(String),

    /// A stale extension context was used after session replacement or
    /// reload (`assertActive`, runner.ts:548-552 / loader.ts:175-179).
    #[error("stale extension context: {0}")]
    Stale(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
