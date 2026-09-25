//! Branch replay: reconstruct `TaskState` from the session branch.
//!
//! Port of upstream `packages/rpiv-todo/state/replay.ts` @ `0fdf4f8`,
//! routed through `ctx.sessionToolResults {"toolName":"todo"}` (ADR-0030,
//! host side V15-14) instead of the upstream `ctx.sessionManager
//! .getBranch()` walk. The ABI already filters `type:"message"` →
//! `role:"toolResult"` → exact `toolName` (extension_context.rs
//! `get_session_tool_results` @ pin), so the plugin-side walk reduces to:
//! take the LAST entry whose `details` shape matches `TaskDetails`
//! (last-write-wins), clone its tasks, and rebuild.
//!
//! `isError` is NOT a filter — upstream `replay.ts` never inspects it:
//! error envelopes still carry the full pre-mutation snapshot under
//! `details`, so replaying them restores the unchanged state (see the
//! task-file §7 erratum note on the pre-written expectation).

use serde_json::Value;

use crate::state::TaskState;
use crate::tool::types::{Task, TOOL_NAME};

/// Discriminator for `details` envelopes that match the persisted
/// `TaskDetails` shape. Defensive — branch entries from older or corrupt
/// sessions are skipped silently (upstream `isTaskDetails`).
pub fn is_task_details(value: &Value) -> bool {
    if !value.is_object() {
        return false;
    }
    let tasks = value.get("tasks").is_some_and(Value::is_array);
    let next_id = value.get("nextId").is_some_and(Value::is_number);
    tasks && next_id
}

/// Decode one matched `details` into a fresh `TaskState`. Returns `None`
/// when the row-level decode fails (e.g. `tasks` elements missing
/// required fields, or `nextId` outside i64): the upstream shallow-copies
/// malformed rows through — a crash surface, not a contract — so rpi
/// treats a row-level decode failure like a shape mismatch and keeps
/// walking (task-file §7 implementation ruling).
fn decode_snapshot(details: &Value) -> Option<TaskState> {
    let tasks_value = details.get("tasks")?;
    let mut tasks = Vec::with_capacity(tasks_value.as_array()?.len());
    for row in tasks_value.as_array()? {
        tasks.push(serde_json::from_value::<Task>(row.clone()).ok()?);
    }
    let next_id = details.get("nextId")?.as_i64()?;
    Some(TaskState { tasks, next_id })
}

/// Walk filtered toolResults (root → leaf order as returned by
/// `ctx.sessionToolResults`) and rebuild the latest snapshot
/// (upstream `replayFromBranch` semantics: last-write-wins, whole-state
/// replacement). No matching entry → fresh empty state.
///
/// Each `entry` is the ADR-0030 six-field projection as JSON:
/// `{id, parentId, timestamp, toolName, isError, details}`.
pub fn replay_from_entries(entries: &[Value]) -> TaskState {
    let mut result = TaskState::empty();
    for entry in entries {
        let Some(details) = entry.get("details") else {
            continue;
        };
        if !is_task_details(details) {
            continue;
        }
        if let Some(state) = decode_snapshot(details) {
            result = state;
        }
    }
    result
}

/// Host-call wrapper: read the current branch's filtered toolResults and
/// replay. Host transport failures surface to the caller (the event
/// wiring keeps current state on stale contexts — upstream
/// `isStaleCtxError` semantics).
pub fn replay_via_host(host: &dyn crate::HostCall) -> Result<TaskState, crate::HostError> {
    let response = host.call(
        "ctx.sessionToolResults",
        serde_json::json!({ "toolName": TOOL_NAME }),
    )?;
    let entries = response.as_array().cloned().unwrap_or_default();
    Ok(replay_from_entries(&entries))
}

#[cfg(test)]
mod tests {
    //! Port of upstream `state/replay.test.ts` @ `0fdf4f8`, reshaped for
    //! the ADR-0030 six-field projection input (the ABI pre-filters
    //! `type:"message"` / `role:"toolResult"` / exact `toolName`, so the
    //! non-message and foreign-tool skip cases collapse into the
    //! shape-mismatch guard).

    use super::*;
    use serde_json::json;

    /// Six-field projection builder (test fixture shape).
    pub(crate) fn tool_result(details: Value) -> Value {
        serde_json::json!({
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "toolName": "todo",
            "isError": false,
            "details": details,
        })
    }

    fn task_fixture(id: i64, subject: &str) -> Value {
        json!({ "id": id, "subject": subject, "status": "pending" })
    }

    // ------------------------------------------------------------------
    // isTaskDetails — defensive type guard
    // ------------------------------------------------------------------

    #[test]
    fn guard_rejects_null_and_undefined() {
        assert!(!is_task_details(&Value::Null));
        assert!(!is_task_details(&Value::Bool(false)));
    }

    #[test]
    fn guard_rejects_primitives() {
        assert!(!is_task_details(&json!("oops")));
        assert!(!is_task_details(&json!(42)));
        assert!(!is_task_details(&json!(true)));
    }

    #[test]
    fn guard_rejects_objects_missing_tasks_or_next_id() {
        assert!(!is_task_details(&json!({})));
        assert!(!is_task_details(&json!({"tasks": "x", "nextId": 1})));
        assert!(!is_task_details(&json!({"tasks": [], "nextId": "1"})));
    }

    #[test]
    fn guard_accepts_well_formed_snapshot_envelopes() {
        assert!(is_task_details(&json!({"tasks": [], "nextId": 1})));
        assert!(is_task_details(
            &json!({"action": "create", "params": {}, "tasks": [], "nextId": 1})
        ));
    }

    // ------------------------------------------------------------------
    // replayFromBranch (via the filtered projection)
    // ------------------------------------------------------------------

    #[test]
    fn returns_empty_state_when_branch_has_no_todo_tool_results() {
        let state = replay_from_entries(&[]);
        assert!(state.tasks.is_empty());
        assert_eq!(state.next_id, 1);
    }

    #[test]
    fn replays_the_last_snapshot_last_write_wins() {
        let entries = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "old")], "nextId": 2
            })),
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "old"), task_fixture(2, "new")], "nextId": 3
            })),
        ];
        let state = replay_from_entries(&entries);
        assert_eq!(state.tasks.len(), 2);
        assert_eq!(state.next_id, 3);
    }

    #[test]
    fn clones_tasks_so_mutating_the_fixture_does_not_leak() {
        let entries = vec![tool_result(json!({
            "action": "create", "params": {},
            "tasks": [task_fixture(1, "original")], "nextId": 2
        }))];
        let state = replay_from_entries(&entries);
        // A second replay over a mutated copy of the same fixture yields a
        // fresh decode (value semantics; upstream `not.toBe` identity).
        let mut mutated = entries.clone();
        mutated[0]["details"]["tasks"][0]["subject"] = json!("changed");
        let state2 = replay_from_entries(&mutated);
        assert_eq!(state.tasks[0].subject, "original");
        assert_eq!(state2.tasks[0].subject, "changed");
    }

    #[test]
    fn skips_entries_whose_details_fail_the_guard() {
        // Corrupt toolResult: toolName=todo, malformed details.
        let corrupt = json!({
            "id": "e2", "parentId": null, "timestamp": "t", "toolName": "todo",
            "isError": false, "details": { "tasks": "not-an-array" }
        });
        let entries = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "good")], "nextId": 2
            })),
            corrupt,
        ];
        let state = replay_from_entries(&entries);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].subject, "good");
    }

    #[test]
    fn skips_entries_with_null_details() {
        // The raw record omits `details` rather than storing null — the
        // projection carries `details: null`.
        let null_details = json!({
            "id": "e3", "parentId": null, "timestamp": "t", "toolName": "todo",
            "isError": true, "details": null
        });
        let entries = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "good")], "nextId": 2
            })),
            null_details,
        ];
        let state = replay_from_entries(&entries);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].subject, "good");
    }

    #[test]
    fn returns_a_fresh_empty_state_on_an_empty_branch() {
        let first = vec![tool_result(json!({
            "action": "create", "params": {},
            "tasks": [task_fixture(1, "x")], "nextId": 2
        }))];
        assert_eq!(replay_from_entries(&first).next_id, 2);
        let fresh = replay_from_entries(&[]);
        assert!(fresh.tasks.is_empty());
        assert_eq!(fresh.next_id, 1);
    }

    // ------------------------------------------------------------------
    // isError semantics — pinned to upstream replay.ts
    // ------------------------------------------------------------------

    #[test]
    fn error_envelopes_with_matching_details_still_replay() {
        // Upstream `replay.ts` never inspects isError: an errored call's
        // envelope still carries the full (unchanged) snapshot, and
        // last-write-wins picks it up. (The task file's pre-written
        // "isError=true entries are skipped" expectation is an erratum —
        // see the module doc and the task file §7.)
        let mut errored = tool_result(json!({
            "action": "create", "params": {},
            "tasks": [task_fixture(1, "pre-error")], "nextId": 2,
            "error": "subject required for create"
        }));
        errored["isError"] = json!(true);
        let entries = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "first")], "nextId": 2
            })),
            errored,
        ];
        let state = replay_from_entries(&entries);
        assert_eq!(state.tasks[0].subject, "pre-error");
        assert_eq!(state.next_id, 2);
    }

    #[test]
    fn row_level_decode_failure_falls_through_to_the_previous_snapshot() {
        // `tasks` is an array and `nextId` a number, but a row is
        // malformed — upstream shallow-copies the broken row through (a
        // crash surface, not a contract); rpi skips the entry and keeps
        // the previous valid snapshot (task-file §7 ruling).
        let broken = tool_result(json!({
            "action": "create", "params": {},
            "tasks": [{ "subject": "no-id" }], "nextId": 2
        }));
        let entries = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "good")], "nextId": 2
            })),
            broken,
        ];
        let state = replay_from_entries(&entries);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].subject, "good");
    }
}
