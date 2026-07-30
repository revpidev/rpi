//! `pir-ext-host` — Rust + Wasm extension host @ design doc §7 (ADR-0001).
//!
//! This crate is a pir-native addition (no upstream counterpart): it
//! implements the `ExtensionHost` trait defined by the `pir` crate with Rust
//! built-in (L0, dynamic library via `abi_stable`) and Wasm (L1, `wasmtime`)
//! backends. No JS/TS execution capability anywhere (red line,
//! coding-standards §1.3).
//!
//! Skeleton only (T01); the host lands in T15.

pub mod error;

pub use error::ExtError;
