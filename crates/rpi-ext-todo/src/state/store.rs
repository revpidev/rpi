//! Per-session live state store + ctx-less render pointer.
//!
//! Port of upstream `packages/rpiv-todo/state/store.ts` @ `0fdf4f8`.
//!
//! Live state is a map partitioned by session id, so a detached/child
//! session (distinct sid) can never read or clobber another session's
//! tasks. The map is the single mutation seam — only `commit_state` /
//! `replace_state` / `evict_session` write it; the reducer
//! ([`crate::state::reducer`]) stays pure. The foreground render pointer
//! (`active_render_session`) and lifecycle generation are distinct
//! concepts from the three task-state mutation seams — they are not 4th
//! writers of task state.
//!
//! Upstream keeps this as a module-level singleton; the Rust port holds the
//! same singleton in a [`OnceLock`] (process-global, one instance per
//! loaded plugin library — the host loads the cdylib per extension
//! instance).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use crate::state::TaskState;

/// Session-partitioned task state + foreground render pointer + lifecycle
/// generation (upstream module state of `store.ts` + `lifecycleGeneration`
/// of `index.ts`).
pub struct TodoStore {
    sessions: RwLock<HashMap<String, TaskState>>,
    /// Which slot the ctx-free readers (overlay render hook, the tool's
    /// renderCall — both TE35) render. Set when the first UI session claims
    /// the foreground, before the overlay exists (creator-ownership).
    active_render_session: RwLock<String>,
    /// Foreground lifecycle generation (`lifecycleGeneration`): bumped on
    /// foreground claim and foreground teardown so late async work from a
    /// replaced session cannot act on the fresh foreground (P0 keeps the
    /// counter; the overlay consumers land with TE35).
    lifecycle_generation: AtomicU64,
}

impl TodoStore {
    fn new() -> Self {
        TodoStore {
            sessions: RwLock::new(HashMap::new()),
            active_render_session: RwLock::new(String::new()),
            lifecycle_generation: AtomicU64::new(0),
        }
    }

    /// Live tasks accessor for a session (upstream `getTodos`): the
    /// committed slot, or a fresh empty-state copy (not stored) when the
    /// slot is absent.
    pub fn get_todos(&self, session_id: &str) -> Vec<crate::tool::types::Task> {
        self.state_for(session_id).tasks
    }

    /// `getNextId`.
    pub fn get_next_id(&self, session_id: &str) -> i64 {
        self.state_for(session_id).next_id
    }

    /// Snapshot accessor used by reducer callers to pass canonical state
    /// in (upstream `getState`). Absent slot → fresh empty copy.
    pub fn state_for(&self, session_id: &str) -> TaskState {
        self.sessions
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .cloned()
            .unwrap_or_else(TaskState::empty)
    }

    /// Replay seam (upstream `replaceState`): lifecycle handlers write the
    /// replayed snapshot into the session's slot wholesale.
    pub fn replace_state(&self, session_id: &str, next: TaskState) {
        self.sessions
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(session_id.to_owned(), next);
    }

    /// Post-reducer commit seam (upstream `commitState`): publish the
    /// reducer's state output to live readers, keyed to the calling
    /// session. Interchangeable with `replace_state` over the same slot.
    pub fn commit_state(&self, session_id: &str, next: TaskState) {
        self.replace_state(session_id, next);
    }

    /// Drop a session's slot on `session_shutdown`. No-op if the slot is
    /// absent (upstream `evictSession`).
    pub fn evict_session(&self, session_id: &str) {
        self.sessions
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }

    /// Ctx-less render reader (upstream `getRenderState`): the foreground
    /// slot, or a fresh empty copy when no foreground has been set.
    pub fn render_state(&self) -> TaskState {
        let active = self
            .active_render_session
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        self.state_for(&active)
    }

    /// Set the ctx-less render pointer when the first UI session claims
    /// the foreground (upstream `setActiveRenderSession`). Returns the
    /// claimed generation.
    pub fn set_active_render_session(&self, session_id: &str) -> u64 {
        *self
            .active_render_session
            .write()
            .unwrap_or_else(|error| error.into_inner()) = session_id.to_owned();
        self.lifecycle_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Read the foreground render pointer — the sid the sid-gate compares
    /// against (upstream `getActiveRenderSession`).
    pub fn active_render_session(&self) -> String {
        self.active_render_session
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Foreground teardown (upstream `clearActiveRenderSession` + the
    /// `lifecycleGeneration++` in the shutdown handler): resets the pointer
    /// to `""` so the next `hasUI` session start reclaims the foreground,
    /// and invalidates in-flight work of the torn-down generation.
    pub fn clear_active_render_session(&self) {
        *self
            .active_render_session
            .write()
            .unwrap_or_else(|error| error.into_inner()) = String::new();
        self.lifecycle_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Current lifecycle generation (observable for tests; overlay wiring
    /// consumes it with TE35).
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation.load(Ordering::SeqCst)
    }

    /// Test-setup reset (upstream `__resetState`): clears BOTH the session
    /// map and the render pointer (+ generation) so replay/isolation tests
    /// start from a clean state.
    pub fn reset(&self) {
        self.sessions
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        *self
            .active_render_session
            .write()
            .unwrap_or_else(|error| error.into_inner()) = String::new();
        self.lifecycle_generation.store(0, Ordering::SeqCst);
    }
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-global store singleton (upstream module-level `sessions` map).
pub fn store() -> &'static TodoStore {
    static STORE: OnceLock<TodoStore> = OnceLock::new();
    STORE.get_or_init(TodoStore::new)
}

/// Test-setup hook re-exporting the store reset (upstream `__resetState`
/// import path).
pub fn reset_store() {
    store().reset();
}

#[cfg(test)]
mod tests {
    //! Port of upstream `state/store.test.ts` @ `0fdf4f8`.

    use super::*;
    use crate::tool::types::{Task, TaskStatus};

    const SID: &str = "s1";

    /// The store is a process-global singleton (upstream module state) —
    /// tests must not interleave. Every test in this module takes the
    /// crate-level lock for its whole body (the vitest suite runs
    /// serially; lib wiring tests share the same lock).
    fn locked() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn make_task(id: i64, subject: &str) -> Task {
        Task {
            id,
            subject: subject.to_owned(),
            status: TaskStatus::Pending,
            description: None,
            active_form: None,
            blocked_by: None,
            owner: None,
            metadata: None,
        }
    }

    fn state(tasks: Vec<Task>, next_id: i64) -> TaskState {
        TaskState { tasks, next_id }
    }

    // ------------------------------------------------------------------
    // Accessors and seams (per-session)
    // ------------------------------------------------------------------

    #[test]
    fn reset_restores_empty_state_shape() {
        let _guard = locked();
        store().reset();
        assert!(store().get_todos(SID).is_empty());
        assert_eq!(store().get_next_id(SID), 1);
    }

    #[test]
    fn get_todos_returns_the_committed_slot() {
        let _guard = locked();
        store().reset();
        store().commit_state(SID, state(vec![make_task(1, "t1")], 2));
        assert_eq!(store().get_todos(SID), vec![make_task(1, "t1")]);
    }

    #[test]
    fn get_next_id_reflects_the_current_slot() {
        let _guard = locked();
        store().reset();
        store().commit_state(SID, state(Vec::new(), 42));
        assert_eq!(store().get_next_id(SID), 42);
    }

    #[test]
    fn state_for_reads_the_same_slot_the_accessors_read() {
        let _guard = locked();
        store().reset();
        store().commit_state(SID, state(vec![make_task(7, "lucky")], 8));
        let snapshot = store().state_for(SID);
        assert_eq!(snapshot.tasks, vec![make_task(7, "lucky")]);
        assert_eq!(snapshot.next_id, store().get_next_id(SID));
    }

    #[test]
    fn replace_state_publishes_a_new_slot_wholesale() {
        let _guard = locked();
        store().reset();
        let replayed = state(
            vec![make_task(10, "from-branch"), make_task(11, "from-branch-2")],
            12,
        );
        store().replace_state(SID, replayed);
        assert_eq!(store().state_for(SID).tasks.len(), 2);
        assert_eq!(store().get_next_id(SID), 12);
    }

    #[test]
    fn commit_and_replace_are_interchangeable_seams() {
        let _guard = locked();
        store().reset();
        store().commit_state(SID, state(vec![make_task(1, "t1")], 2));
        assert_eq!(store().get_next_id(SID), 2);
        store().replace_state(SID, state(Vec::new(), 99));
        assert!(store().get_todos(SID).is_empty());
        assert_eq!(store().get_next_id(SID), 99);
    }

    #[test]
    fn reset_after_commit_clears_the_slot() {
        let _guard = locked();
        store().commit_state(SID, state(vec![make_task(1, "t1")], 2));
        store().reset();
        assert!(store().get_todos(SID).is_empty());
        assert_eq!(store().get_next_id(SID), 1);
    }

    // ------------------------------------------------------------------
    // Per-session isolation
    // ------------------------------------------------------------------

    #[test]
    fn commits_to_one_session_never_affect_another() {
        let _guard = locked();
        store().reset();
        store().commit_state("s1", state(vec![make_task(1, "s1-task")], 2));
        store().commit_state("s2", state(vec![make_task(1, "s2-task")], 5));
        assert_eq!(store().state_for("s1").tasks, vec![make_task(1, "s1-task")]);
        assert_eq!(store().state_for("s2").tasks, vec![make_task(1, "s2-task")]);

        store().replace_state("s1", state(vec![make_task(9, "new-s1")], 10));
        assert_eq!(store().get_next_id("s2"), 5);
        store().commit_state("s2", state(Vec::new(), 77));
        assert_eq!(store().get_todos("s1"), vec![make_task(9, "new-s1")]);
        assert_eq!(store().get_next_id("s1"), 10);
    }

    #[test]
    fn a_missing_slot_returns_a_fresh_empty_state_copy() {
        let _guard = locked();
        store().reset();
        let slot = store().state_for("never-seen");
        assert!(slot.tasks.is_empty());
        assert_eq!(slot.next_id, 1);
        // Fresh copies never alias one another.
        assert!(store().get_todos("absent").is_empty());
        assert_eq!(store().get_next_id("absent"), 1);
    }

    // ------------------------------------------------------------------
    // evictSession
    // ------------------------------------------------------------------

    #[test]
    fn evict_frees_the_slot_and_later_reads_return_empty() {
        let _guard = locked();
        store().reset();
        store().commit_state(SID, state(vec![make_task(1, "t1")], 2));
        assert_eq!(store().state_for(SID).tasks.len(), 1);
        store().evict_session(SID);
        let after = store().state_for(SID);
        assert!(after.tasks.is_empty());
        assert_eq!(after.next_id, 1);
    }

    #[test]
    fn evict_on_an_absent_slot_is_a_no_op() {
        let _guard = locked();
        store().reset();
        store().evict_session("absent");
        assert!(store().get_todos("absent").is_empty());
    }

    // ------------------------------------------------------------------
    // Ctx-less render pointer
    // ------------------------------------------------------------------

    #[test]
    fn render_state_is_empty_before_any_pointer_is_set() {
        let _guard = locked();
        store().reset();
        let rendered = store().render_state();
        assert!(rendered.tasks.is_empty());
        assert_eq!(rendered.next_id, 1);
    }

    #[test]
    fn set_active_makes_render_state_read_that_session_slot() {
        let _guard = locked();
        store().reset();
        store().commit_state("rendered", state(vec![make_task(3, "shown")], 4));
        store().set_active_render_session("rendered");
        let rendered = store().render_state();
        assert_eq!(rendered.tasks, vec![make_task(3, "shown")]);
        assert_eq!(rendered.next_id, 4);
    }

    #[test]
    fn set_active_re_points_the_render_slot() {
        let _guard = locked();
        store().reset();
        store().commit_state("a", state(vec![make_task(1, "a")], 2));
        store().commit_state("b", state(vec![make_task(1, "b")], 2));
        store().set_active_render_session("a");
        assert_eq!(store().render_state().tasks[0].subject, "a");
        store().set_active_render_session("b");
        assert_eq!(store().render_state().tasks[0].subject, "b");
    }

    #[test]
    fn reset_clears_both_the_map_and_the_pointer() {
        let _guard = locked();
        store().commit_state("a", state(vec![make_task(1, "t1")], 2));
        store().set_active_render_session("a");
        assert_eq!(store().render_state().tasks.len(), 1);
        store().reset();
        assert!(store().state_for("a").tasks.is_empty());
        let rendered = store().render_state();
        assert!(rendered.tasks.is_empty());
    }

    #[test]
    fn active_render_session_accessors() {
        let _guard = locked();
        store().reset();
        assert_eq!(store().active_render_session(), "");
        store().set_active_render_session("s1");
        assert_eq!(store().active_render_session(), "s1");
        store().clear_active_render_session();
        assert_eq!(store().active_render_session(), "");
    }

    #[test]
    fn clear_bumps_the_lifecycle_generation() {
        let _guard = locked();
        store().reset();
        let before = store().lifecycle_generation();
        store().set_active_render_session("s1");
        assert!(store().lifecycle_generation() > before);
        store().clear_active_render_session();
        assert!(store().lifecycle_generation() > before + 1);
    }
}
