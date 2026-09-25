//! Pure selectors over [`TaskState`].
//!
//! Port of upstream `packages/rpiv-todo/state/selectors.ts` @ `0fdf4f8`.

use crate::state::TaskState;
use crate::tool::types::{Task, TaskStatus};

/// Tasks excluding deleted tombstones — the canonical "what's visible"
/// (upstream `selectVisibleTasks`).
pub fn select_visible_tasks(state: &TaskState) -> Vec<&Task> {
    state
        .tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Deleted)
        .collect()
}

/// Group visible tasks by status (upstream `selectTasksByStatus`).
///
/// Iteration order at call sites uses (`completed`, `in_progress`,
/// `pending`) to match the `/todos` header part order pinned by
/// `todo.command.test.ts`.
#[derive(Clone, Debug, Default)]
pub struct TasksByStatus<'a> {
    pub pending: Vec<&'a Task>,
    pub in_progress: Vec<&'a Task>,
    pub completed: Vec<&'a Task>,
}

pub fn select_tasks_by_status(state: &TaskState) -> TasksByStatus<'_> {
    let visible = select_visible_tasks(state);
    let mut groups = TasksByStatus::default();
    for task in visible {
        match task.status {
            TaskStatus::Pending => groups.pending.push(task),
            TaskStatus::InProgress => groups.in_progress.push(task),
            TaskStatus::Completed => groups.completed.push(task),
            TaskStatus::Deleted => {}
        }
    }
    groups
}

/// Total counts for the overlay heading (`Todos (n/m)`) and `/todos`
/// header (upstream `selectTodoCounts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TodoCounts {
    pub total: usize,
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
}

pub fn select_todo_counts(state: &TaskState) -> TodoCounts {
    let groups = select_tasks_by_status(state);
    TodoCounts {
        total: groups.pending.len() + groups.in_progress.len() + groups.completed.len(),
        pending: groups.pending.len(),
        in_progress: groups.in_progress.len(),
        completed: groups.completed.len(),
    }
}

/// Whether any visible task carries a `blockedBy` reference. The overlay
/// uses this to gate the `#id` prefix on per-task rows — without at least
/// one `⛓ #N` suffix, the per-row id has no anchor (upstream
/// `selectShowTaskIds`).
pub fn select_show_task_ids(state: &TaskState) -> bool {
    select_visible_tasks(state).into_iter().any(|task| {
        task.blocked_by
            .as_ref()
            .is_some_and(|deps| !deps.is_empty())
    })
}

/// Resolve a task's subject by id from the live state for renderCall's
/// accent label. `None` when the id is unknown — caller falls back to
/// `#id` plain rendering (upstream `selectTaskSubjectById`).
pub fn select_task_subject_by_id(state: &TaskState, id: i64) -> Option<&str> {
    state
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map(|t| t.subject.as_str())
}

/// Overlay layout decision (upstream `selectOverlayLayout`): encapsulates
/// the "drop completed first, then truncate non-completed tail" rule.
/// `budget` is the body-slot count (the caller passes
/// `maxWidgetLines - 1` to reserve the heading row); on overflow the
/// selector reserves one more slot internally for the summary row. The
/// overlay wiring consumes this with TE35; the pure rule lands here.
#[derive(Clone, Debug, Default)]
pub struct OverlayLayout<'a> {
    pub visible: Vec<&'a Task>,
    pub hidden_completed: usize,
    pub truncated_tail: usize,
}

pub fn select_overlay_layout(state: &TaskState, budget: usize) -> OverlayLayout<'_> {
    let all = select_visible_tasks(state);
    if all.len() <= budget {
        return OverlayLayout {
            visible: all,
            hidden_completed: 0,
            truncated_tail: 0,
        };
    }
    let inner_budget = budget.saturating_sub(1);
    let non_completed: Vec<&Task> = all
        .iter()
        .copied()
        .filter(|task| task.status != TaskStatus::Completed)
        .collect();
    let total_completed = all.len() - non_completed.len();
    if non_completed.len() <= inner_budget {
        // Keep non-completed, then pad with completed in list order until
        // the inner budget fills (upstream Set-of-Tasks walk over `all`).
        let mut kept: Vec<bool> = all
            .iter()
            .map(|task| task.status != TaskStatus::Completed)
            .collect();
        let mut kept_count = non_completed.len();
        for (index, task) in all.iter().enumerate() {
            if kept_count >= inner_budget {
                break;
            }
            if task.status == TaskStatus::Completed && !kept[index] {
                kept[index] = true;
                kept_count += 1;
            }
        }
        let visible: Vec<&Task> = all
            .iter()
            .zip(kept)
            .filter(|(_, keep)| *keep)
            .map(|(task, _)| *task)
            .collect();
        let shown_completed = visible
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count();
        return OverlayLayout {
            visible,
            hidden_completed: total_completed - shown_completed,
            truncated_tail: 0,
        };
    }
    let visible: Vec<&Task> = non_completed[..inner_budget].to_vec();
    let truncated_tail = non_completed.len() - inner_budget;
    OverlayLayout {
        visible,
        hidden_completed: total_completed,
        truncated_tail,
    }
}

/// Helper: whether any visible task is `pending` or `in_progress`. The
/// overlay uses this to pick the heading icon (upstream
/// `selectHasActive`).
pub fn select_has_active(state: &TaskState) -> bool {
    select_visible_tasks(state)
        .into_iter()
        .any(|task| task.status == TaskStatus::Pending || task.status == TaskStatus::InProgress)
}

/// The active statuses (`ACTIVE_STATUSES`).
pub const ACTIVE_STATUSES: [TaskStatus; 2] = [TaskStatus::Pending, TaskStatus::InProgress];

#[cfg(test)]
mod tests {
    //! Selector unit tests (upstream has no dedicated selectors suite —
    //! these pin the design §2 derivations the overlay/command paths
    //! consume; TE35 builds on them).

    use super::*;
    use crate::tool::types::{Task, TaskStatus};

    fn task(id: i64, subject: &str, status: TaskStatus) -> Task {
        Task {
            id,
            subject: subject.to_owned(),
            status,
            description: None,
            active_form: None,
            blocked_by: None,
            owner: None,
            metadata: None,
        }
    }

    fn state(tasks: Vec<Task>) -> TaskState {
        let next_id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        TaskState { tasks, next_id }
    }

    #[test]
    fn visible_tasks_exclude_tombstones() {
        let state = state(vec![
            task(1, "a", TaskStatus::Pending),
            task(2, "b", TaskStatus::Deleted),
            task(3, "c", TaskStatus::Completed),
        ]);
        let visible = select_visible_tasks(&state);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].id, 1);
        assert_eq!(visible[1].id, 3);
    }

    #[test]
    fn grouping_splits_by_status() {
        let state = state(vec![
            task(1, "p", TaskStatus::Pending),
            task(2, "i", TaskStatus::InProgress),
            task(3, "c", TaskStatus::Completed),
            task(4, "p2", TaskStatus::Pending),
        ]);
        let groups = select_tasks_by_status(&state);
        assert_eq!(groups.pending.len(), 2);
        assert_eq!(groups.in_progress.len(), 1);
        assert_eq!(groups.completed.len(), 1);
        let counts = select_todo_counts(&state);
        assert_eq!(counts.total, 4);
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.in_progress, 1);
        assert_eq!(counts.completed, 1);
    }

    #[test]
    fn show_task_ids_only_with_a_visible_blocked_by() {
        let mut with_dep = task(1, "a", TaskStatus::Pending);
        with_dep.blocked_by = Some(vec![2]);
        assert!(select_show_task_ids(&state(vec![
            with_dep,
            task(2, "b", TaskStatus::Pending),
        ])));
        assert!(!select_show_task_ids(&state(vec![task(
            1,
            "a",
            TaskStatus::Pending
        )])));
        // A tombstoned dep holder is invisible → no ids shown.
        let mut deleted_dep = task(1, "a", TaskStatus::Deleted);
        deleted_dep.blocked_by = Some(vec![2]);
        assert!(!select_show_task_ids(&state(vec![
            deleted_dep,
            task(2, "b", TaskStatus::Pending),
        ])));
    }

    #[test]
    fn subject_by_id_resolves_or_none() {
        let state = state(vec![task(7, "lucky", TaskStatus::Pending)]);
        assert_eq!(select_task_subject_by_id(&state, 7), Some("lucky"));
        assert_eq!(select_task_subject_by_id(&state, 8), None);
    }

    #[test]
    fn has_active_tracks_pending_and_in_progress() {
        assert!(select_has_active(&state(vec![task(
            1,
            "a",
            TaskStatus::Pending
        )])));
        assert!(select_has_active(&state(vec![task(
            1,
            "a",
            TaskStatus::InProgress
        )])));
        assert!(!select_has_active(&state(vec![task(
            1,
            "a",
            TaskStatus::Completed
        )])));
    }

    #[test]
    fn overlay_layout_within_budget_shows_all() {
        let state = state(vec![
            task(1, "a", TaskStatus::Pending),
            task(2, "b", TaskStatus::Completed),
        ]);
        let layout = select_overlay_layout(&state, 2);
        assert_eq!(layout.visible.len(), 2);
        assert_eq!(layout.hidden_completed, 0);
        assert_eq!(layout.truncated_tail, 0);
    }

    #[test]
    fn overlay_layout_drops_completed_first_newest_completed_first() {
        // budget 3 → inner 2 (one reserved for the summary row): the
        // single non-completed stays, then completed fill in list order
        // (oldest first — the newest completed drop first).
        let state = state(vec![
            task(1, "c1", TaskStatus::Completed),
            task(2, "c2", TaskStatus::Completed),
            task(3, "c3", TaskStatus::Completed),
            task(4, "p", TaskStatus::Pending),
        ]);
        let layout = select_overlay_layout(&state, 3);
        assert_eq!(layout.visible.len(), 2);
        assert_eq!(layout.visible[0].id, 1);
        assert_eq!(layout.visible[1].id, 4);
        assert_eq!(layout.hidden_completed, 2);
        assert_eq!(layout.truncated_tail, 0);
    }

    #[test]
    fn overlay_layout_truncates_non_completed_tail_on_overflow() {
        let state = state(vec![
            task(1, "p1", TaskStatus::Pending),
            task(2, "p2", TaskStatus::Pending),
            task(3, "p3", TaskStatus::Pending),
            task(4, "p4", TaskStatus::Pending),
        ]);
        let layout = select_overlay_layout(&state, 2);
        assert_eq!(layout.visible.len(), 1);
        assert_eq!(layout.visible[0].id, 1);
        assert_eq!(layout.truncated_tail, 3);
        assert_eq!(layout.hidden_completed, 0);
    }
}
