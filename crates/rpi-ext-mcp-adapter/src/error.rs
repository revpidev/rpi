//! Crate-level error type (coding-standards §5.1: one typed error enum per
//! library crate).
//!
//! Counterpart of upstream `errors.ts` @ 3d953f90. This wave only needs the
//! configuration/cache variants; the MCP protocol error classification from
//! upstream `errors.ts` (SDK error → needs-auth mapping etc.) lands here with
//! the transport wave.

/// Errors from configuration resolution and the metadata cache.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// `resolveServerUrl` (utils.ts): URL is not a string, references unset
    /// environment variables, or fails WHATWG URL parsing after
    /// interpolation.
    #[error("invalid MCP server URL: {0}")]
    InvalidServerUrl(String),

    /// Non-string value where an interpolatable string was required
    /// (upstream throws a generic Error from the interpolation helpers).
    #[error("invalid config value: {0}")]
    InvalidConfigValue(String),

    /// Metadata cache filesystem failure (cache writes are atomic
    /// tmp+rename; a failure here is a persistence error, not a cache miss).
    #[error("metadata cache io error: {0}")]
    CacheIo(#[from] std::io::Error),

    /// Metadata cache serialization failure.
    #[error("metadata cache serialization error: {0}")]
    CacheSerialize(String),
}
