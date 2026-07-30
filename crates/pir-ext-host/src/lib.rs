//! `pir-ext-host` — Rust + Wasm extension host @ design doc §7 (ADR-0001).
//!
//! This crate is a pir-native addition (no upstream counterpart): it
//! implements the `ExtensionHost` trait defined by the `pir` crate with Rust
//! built-in (L0, dynamic library via `abi_stable`) and Wasm (L1, `wasmtime`)
//! backends. No JS/TS execution capability anywhere (red line,
//! coding-standards §1.3).
//!
//! Skeleton only (T01); the host lands in T15. The T02 Wasm ABI spike lives in
//! `examples/wasm_spike.rs` (+ `examples/wasm-spike/guest`).

pub mod error;

pub use error::ExtError;

/// Smoke hook for the embedded Wasm runtime (T02 binary-size measurement).
/// Instantiating an `Engine` and compiling a module keeps the wasmtime
/// dependency reachable from the release binary; T15 replaces this with the
/// real host.
pub fn wasm_smoke() -> Result<String, ExtError> {
    let engine = wasmtime::Engine::default();
    let module =
        wasmtime::Module::new(&engine, "(module)").map_err(|e| ExtError::Load(e.to_string()))?;
    Ok(format!(
        "wasmtime engine ok ({} imports, {} exports)",
        module.imports().len(),
        module.exports().len()
    ))
}
