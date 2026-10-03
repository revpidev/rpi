//! `rpi` — port of `@earendil-works/pi-coding-agent` @ pi 0.82.1 (2efa728).
//!
//! CLI modes (interactive / print / json / rpc) + lib SDK. This crate is the
//! single assembly point of the workspace (coding-standards §2.2): it defines
//! the `ExtensionHost` trait, binds the `rpi-ext-host` implementation, and
//! injects `rpi-ai`'s `Models::stream` as the agent's `StreamFn`.
//!
//! The binary target is `rpi` (`src/main.rs`); this lib target is the SDK
//! surface (requirements §2.5).
//!
//! Skeleton only (T01); modes land in T10/T12.

pub mod app;
pub mod cli;
pub mod config;
pub mod core;
pub mod error;
pub mod extensions;
pub mod logging;
pub mod modes;
pub mod sdk;
pub mod tools;
pub mod utils;

pub use error::RpiError;

/// Test-only process-wide locks for state that has no injection seam.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global terminal capability
    /// overrides ([`rpi_tui::terminal_image::set_capability_overrides`]).
    /// Theme construction reads the overrides (#9973), so mutating tests
    /// and theme-loading tests must not overlap.
    pub(crate) static CAPABILITY_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());
}
