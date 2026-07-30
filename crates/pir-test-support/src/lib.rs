//! `pir-test-support` — test infrastructure crate (coding-standards §12).
//!
//! Faux provider, golden JSONL fixtures, normalize/diff helpers, and virtual
//! terminal helpers. Referenced only as a **dev-dependency** — it must never
//! enter the release dependency chain (coding-standards §2.2).
//!
//! Skeleton only (T01); the parity harness lands in T02.

pub mod error;

pub use error::TestSupportError;
