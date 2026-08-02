//! Cross-cutting utilities, mirroring `packages/ai/src/utils/` (design §3.6).
//!
//! `transform_messages` lives here per design §3.6 (upstream keeps it under
//! `api/`); `cost` holds `calculateCost` (upstream `models.ts`).

pub mod cost;
pub mod deferred_tools;
pub mod error_body;
pub mod estimate;
pub mod event_stream;
pub mod hash;
pub mod headers;
pub mod json_parse;
pub mod overflow;
pub mod provider_env;
pub mod provider_retry;
pub mod retry;
pub mod sanitize_unicode;
pub mod transform_messages;
pub mod uuid;
pub mod validation;
