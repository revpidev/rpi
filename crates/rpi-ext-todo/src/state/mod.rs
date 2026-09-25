//! Pure state domain: reducer, store, replay, selectors, graph, invariants.
//!
//! Mirrors upstream `packages/rpiv-todo/state/*` @ `0fdf4f8`. None of
//! these modules depend on host SDK types (design §2 dependency
//! boundary — `serde_json::Value` at the edges only).

pub mod invariants;
pub mod reducer;
pub mod replay;
pub mod selectors;
pub mod store;
pub mod task_graph;

use crate::tool::types::Task;

/// Canonical state for the todo tool (upstream `TaskState`). Single
/// source of truth — both the reducer ([`reducer`]) and the live store
/// cell ([`store`]) read this shape; replay ([`replay`]) returns a fresh
/// one.
///
/// The shape is intentionally minimal — no derived caches or runtime
/// cells. Selectors own all derivations.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaskState {
    pub tasks: Vec<Task>,
    pub next_id: i64,
}

impl TaskState {
    /// Fresh, non-aliasing empty-state copy (upstream `freshState()` —
    /// callers never share one live vector).
    pub fn empty() -> Self {
        TaskState {
            tasks: Vec::new(),
            next_id: EMPTY_NEXT_ID,
        }
    }
}

/// The empty-state `nextId` (upstream `EMPTY_STATE`).
pub const EMPTY_NEXT_ID: i64 = 1;
