//! Consumed subset of `@earendil-works/chord` (port of `packages/chord` @
//! 4a080252f).
//!
//! Only the surfaces durable consumes are carried (V17-13 FR-B): the cancellation
//! context family, `copyJson`/`JsonValue` helpers, the `Op` delta applier, the
//! `replicatedState` document-state service, and (V18-05) the portable SQLite
//! drivers with their generic migration runner. Bundler/facets/protocol stay
//! [DEFER] (R5); the IndexedDB storage remains [N/A-struct].

pub mod context;
pub mod delta;
pub mod json;
pub mod sqlite;
pub mod state;
pub mod telemetry;
pub mod tracker;
