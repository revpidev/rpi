//! Port of `packages/ai/src/auth/` @ pi 0.82.1 (2efa728) — T03 skeleton.
//!
//! This module defines the auth **interfaces** (credential types, store,
//! resolution) that `Models` depends on. The concrete credential persistence
//! (JSON file, 0600, key-value DSL), OAuth flows and provider auth handlers
//! are delivered by T04.

pub mod credential_store;
pub mod resolve;
pub mod types;

pub use credential_store::InMemoryCredentialStore;
pub use resolve::{resolve_provider_auth, AuthResolutionOverrides, ModelsError, ModelsErrorCode};
pub use types::*;
