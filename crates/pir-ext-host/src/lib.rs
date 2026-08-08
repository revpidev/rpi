//! `pir-ext-host` — Rust + Wasm extension host @ design doc §7 (ADR-0001).
//!
//! This crate is a pir-native addition (no upstream counterpart): it
//! implements the extension-host capability surface (33 events + 24 API
//! methods + 28 UI methods) with Rust built-in (L0) and Wasm (L1,
//! `wasmtime`) backends. No JS/TS execution capability anywhere (red line,
//! coding-standards §1.3).
//!
//! L0 core: [`host::NativeExtensionHost`] = [`loader::ExtensionLoader`]
//! (factories, discovery, cache) + [`runner::ExtensionRunnerCore`]
//! (registries, conflict rules, serial emit dispatch), with extensions
//! driving [`api::ExtensionApi`]. L1 (wasm): [`wasm`] — ABI v1 host
//! (docs/extension-abi.md). The T02 spike was removed in W6 (its protocol
//! conclusions became the ABI).

pub mod api;
pub mod bridges;
pub mod error;
pub mod host;
pub mod loader;
pub mod native;
pub mod runner;
pub mod types;
pub mod wasm;

pub use error::ExtError;
