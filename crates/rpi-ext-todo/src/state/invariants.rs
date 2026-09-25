//! Status-machine invariant table.
//!
//! Port of upstream `packages/rpiv-todo/state/invariants.ts` @ `0fdf4f8`.

use crate::tool::types::TaskStatus;

/// Allowed forward transitions per source status. `completed` is one-way to
/// `deleted` (never back to `in_progress`); `deleted` is terminal.
///
/// Idempotent same→same is checked separately in [`is_transition_valid`] so
/// this table only enumerates actual transitions.
pub fn valid_transitions(from: TaskStatus) -> &'static [TaskStatus] {
    match from {
        TaskStatus::Pending => &[
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Deleted,
        ],
        TaskStatus::InProgress => &[
            TaskStatus::Pending,
            TaskStatus::Completed,
            TaskStatus::Deleted,
        ],
        TaskStatus::Completed => &[TaskStatus::Deleted],
        TaskStatus::Deleted => &[],
    }
}

/// `isTransitionValid` — same→same is always valid (idempotent no-op
/// transition), otherwise the table decides.
pub fn is_transition_valid(from: TaskStatus, to: TaskStatus) -> bool {
    if from == to {
        return true;
    }
    valid_transitions(from).contains(&to)
}
