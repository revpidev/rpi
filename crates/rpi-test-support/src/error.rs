//! Main error enum for `rpi-test-support`.

/// Error type for test-support helpers.
#[derive(Debug, thiserror::Error)]
pub enum TestSupportError {
    #[error("fixture error: {0}")]
    Fixture(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
