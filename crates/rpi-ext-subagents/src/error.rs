//! Crate error type (coding-standards §5.1: thiserror per library crate).
//!
//! Errors crossing the ABI boundary become tool-result text (the ABI has no
//! throw channel — same tradeoff mcp-adapter recorded in TE-D04); the enum
//! keeps the internal failure kinds typed for tests.

#[derive(Debug, thiserror::Error)]
pub enum SubagentsError {
    /// IO failures from argv assembly (temp files, session directories).
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for SubagentsError {
    fn from(error: std::io::Error) -> Self {
        SubagentsError::Io(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SubagentsError>;
