//! Main error enum for `rpi-tui` (coding-standards §5.1).

/// Error type for `rpi-tui` fallible operations.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("render error: {0}")]
    Render(String),
}
