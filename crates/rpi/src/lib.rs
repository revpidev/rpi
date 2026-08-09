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

pub use error::PirError;
