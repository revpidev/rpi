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
use crate::tool::types::{STATE_ENTRY_TYPE, TOOL_NAME, Task};

/// `Number.isSafeInteger` bound (`2^53 - 1`). Replay must reject ids a JS
/// host would reject; rpi's `i64` range is wider and an unbounded `nextId`
/// would overflow on the next create.
const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn is_safe_integer(value: &Value) -> bool {
    if let Some(number) = value.as_i64() {
        return (-JS_MAX_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&number);
    }
    value.as_f64().is_some_and(|number| {
        number.is_finite()
            && number.fract() == 0.0
            && (-(JS_MAX_SAFE_INTEGER as f64)..=(JS_MAX_SAFE_INTEGER as f64)).contains(&number)
    })
}

/// Normalize an integral JSON float (`0.0`) to an integer so `serde` can
/// decode a snapshot a JS host accepts (`Number.isSafeInteger(0.0)` is true).
fn normalize_integer(value: &Value) -> Option<Value> {
    if value.is_i64() || value.is_u64() {
        return Some(value.clone());
    }
    let number = value.as_f64()?;
    if number.is_finite() && number.fract() == 0.0 {
        Some(Value::from(number as i64))
    } else {
        None
    }
}

fn normalize_task_row(row: &Value) -> Option<Value> {
    let mut task = row.as_object()?.clone();
    if let Some(id) = task.get("id") {
        task.insert("id".to_string(), normalize_integer(id)?);
    }
    if let Some(blocked_by) = task.get("blockedBy").and_then(Value::as_array) {
        let mut ids = Vec::with_capacity(blocked_by.len());
        for id in blocked_by {
            ids.push(normalize_integer(id)?);
        }
        task.insert("blockedBy".to_string(), Value::Array(ids));
    }
    Some(Value::Object(task))
}

/// One persisted task, validated down to every field the renderer reads
/// (`isTask`, replay.ts:8-25 @ 2.15.0, #230). A snapshot another extension
/// truncated with a marker string must be skipped whole, so the last valid
/// list stands. Optional fields accept only absence or their declared type:
/// an explicit `null` is not `undefined` upstream and skips the snapshot.
fn is_task(value: &Value) -> bool {
    let Some(task) = value.as_object() else {
        return false;
    };
    if !task.get("id").is_some_and(is_safe_integer) {
        return false;
    }
    if !task.get("subject").is_some_and(Value::is_string) {
        return false;
    }
    if !matches!(
        task.get("status").and_then(Value::as_str),
        Some("pending" | "in_progress" | "completed" | "deleted")
    ) {
        return false;
    }
    let optional_string = |key: &str| task.get(key).is_none_or(Value::is_string);
    if !optional_string("description")
        || !optional_string("activeForm")
        || !optional_string("owner")
    {
        return false;
    }
    if let Some(blocked_by) = task.get("blockedBy") {
        let Some(ids) = blocked_by.as_array() else {
            return false;
        };
        if !ids.iter().all(is_safe_integer) {
            return false;
        }
    }
    if let Some(metadata) = task.get("metadata")
        && !metadata.is_object()
    {
        return false;
    }
    true
}

/// Discriminator for `details` envelopes that match the persisted
/// `TaskDetails` shape, down to every task (upstream `isTaskDetails`
/// @ 2.15.0, #230). Defensive — a snapshot from an older or corrupt session
/// is skipped whole, so replay keeps the last valid one.
pub fn is_task_details(value: &Value) -> bool {
    if !value.is_object() {
        return false;
    }
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return false;
    };
    let next_id = value.get("nextId").is_some_and(is_safe_integer);
    next_id && tasks.iter().all(is_task)
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
        tasks.push(serde_json::from_value::<Task>(normalize_task_row(row)?).ok()?);
    }
    let next_id = normalize_integer(details.get("nextId")?)?.as_i64()?;
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

/// Walk filtered `custom` entries (root → leaf as returned by
/// `ctx.sessionEntries`) and return the last valid `TaskDetails` snapshot.
/// `None` when the branch carries none — a malformed state entry does not
/// count, and the caller then falls back to `toolResult` snapshots
/// (upstream `replayFromBranch` @ v2.13.8 treats only a valid
/// `rpiv-todo-state` entry as authoritative; #290).
pub fn replay_from_session_entries(entries: &[Value]) -> Option<TaskState> {
    let mut result: Option<TaskState> = None;
    for entry in entries {
        if entry.get("customType").and_then(Value::as_str) != Some(STATE_ENTRY_TYPE) {
            continue;
        }
        let Some(data) = entry.get("data") else {
            continue;
        };
        if !is_task_details(data) {
            continue;
        }
        if let Some(state) = decode_snapshot(data) {
            result = Some(state);
        }
    }
    result
}

/// Host-call wrapper: the branch's `rpiv-todo-state` custom entries are read
/// first and, when any valid snapshot exists, are authoritative; otherwise
/// the pre-entry [`replay_from_entries`] toolResult walk still serves
/// sessions written before this change. Read failures surface to the callers
/// (upstream `replayAndRefresh` semantics).
pub fn replay_via_host(host: &dyn crate::HostCall) -> Result<TaskState, crate::HostError> {
    let entries_response = host.call(
        "ctx.sessionEntries",
        serde_json::json!({ "customType": STATE_ENTRY_TYPE }),
    )?;
    let entries = entries_response.as_array().cloned().unwrap_or_default();
    if let Some(state) = replay_from_session_entries(&entries) {
        return Ok(state);
    }
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

    /// `ctx.sessionEntries` projection builder (ADR-0027 shape).
    fn state_entry(data: Value) -> Value {
        json!({
            "id": "c1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "customType": "rpiv-todo-state",
            "data": data,
        })
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
    fn guard_rejects_a_snapshot_whose_tasks_hold_a_malformed_entry() {
        let truncated = json!("[output-policy: array truncated, dropped 3 item(s)]");
        assert!(!is_task_details(
            &json!({"tasks": [task_fixture(1, "kept"), truncated], "nextId": 58})
        ));
        assert!(!is_task_details(
            &json!({"tasks": [{"id": 1, "status": "pending"}], "nextId": 2})
        ));
        let mut done = task_fixture(1, "x");
        done["status"] = json!("done");
        assert!(!is_task_details(&json!({"tasks": [done], "nextId": 2})));
    }

    #[test]
    fn guard_rejects_optional_fields_the_renderer_would_choke_on() {
        for extra in [
            json!({"activeForm": 7}),
            json!({"blockedBy": "#2"}),
            json!({"blockedBy": ["2"]}),
            json!({"owner": {}}),
            json!({"metadata": "m"}),
        ] {
            let task = json!({
                "id": 1,
                "subject": "x",
                "status": "pending",
            });
            let mut task = task;
            for (key, value) in extra.as_object().unwrap() {
                task[key] = value.clone();
            }
            assert!(
                !is_task_details(&json!({"tasks": [task], "nextId": 2})),
                "{}",
                extra
            );
        }
    }

    #[test]
    fn guard_accepts_a_task_carrying_every_optional_field() {
        let full = json!({
            "id": 2,
            "subject": "full",
            "status": "in_progress",
            "description": "d",
            "activeForm": "doing",
            "blockedBy": [1],
            "owner": "me",
            "metadata": {"k": 1}
        });
        assert!(is_task_details(&json!({
            "tasks": [task_fixture(1, "a"), full],
            "nextId": 3
        })));
    }

    #[test]
    fn a_malformed_snapshot_keeps_the_last_valid_state_on_both_sources() {
        let truncated = json!({
            "tasks": [task_fixture(1, "A"), "[output-policy: array truncated]"],
            "nextId": 9
        });
        let entries = vec![
            state_entry(json!({"tasks": [task_fixture(1, "A")], "nextId": 2})),
            state_entry(truncated.clone()),
        ];
        let state = replay_from_session_entries(&entries).expect("valid snapshot");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.next_id, 2);
        let results = vec![
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": [task_fixture(1, "A")], "nextId": 2
            })),
            tool_result(json!({
                "action": "create", "params": {},
                "tasks": truncated["tasks"].clone(), "nextId": 9
            })),
        ];
        let state = replay_from_entries(&results);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.next_id, 2);
    }

    #[test]
    fn guard_rejects_explicit_null_optional_fields() {
        for key in [
            "description",
            "activeForm",
            "owner",
            "blockedBy",
            "metadata",
        ] {
            let mut task = task_fixture(1, "x");
            task[key] = Value::Null;
            assert!(
                !is_task_details(&json!({"tasks": [task], "nextId": 2})),
                "{key}: null must skip the whole snapshot"
            );
        }
    }

    #[test]
    fn guard_enforces_the_js_safe_integer_bound() {
        let max = 9_007_199_254_740_991i64;
        let over = 9_007_199_254_740_992i64;
        assert!(is_task_details(&json!({
            "tasks": [{"id": max, "subject": "x", "status": "pending"}],
            "nextId": max
        })));
        assert!(!is_task_details(&json!({
            "tasks": [{"id": over, "subject": "x", "status": "pending"}],
            "nextId": 2
        })));
        assert!(!is_task_details(&json!({
            "tasks": [{"id": 1, "subject": "x", "status": "pending", "blockedBy": [over]}],
            "nextId": 2
        })));
        assert!(!is_task_details(&json!({
            "tasks": [{"id": 1, "subject": "x", "status": "pending"}],
            "nextId": over
        })));
    }

    #[test]
    fn guard_accepts_integral_json_floats_like_number_is_safe_integer() {
        let state = replay_from_entries(&[tool_result(json!({
            "action": "create", "params": {},
            "tasks": [{"id": 1.0, "subject": "x", "status": "pending"}],
            "nextId": 2.0
        }))]);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].id, 1);
        assert_eq!(state.next_id, 2);
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

    /// #290 (v2.13.5): the branch's `rpiv-todo-state` custom entries replay
    /// last-write-wins and take precedence over toolResult snapshots.
    #[test]
    fn session_entries_are_authoritative_and_last_write_wins() {
        let entries = vec![
            state_entry(json!({"tasks": [task_fixture(1, "entry")], "nextId": 2})),
            state_entry(json!({
                "tasks": [task_fixture(1, "entry"), task_fixture(2, "newer")], "nextId": 3
            })),
        ];
        let state = replay_from_session_entries(&entries).expect("valid state entries");
        assert_eq!(state.tasks.len(), 2);
        assert_eq!(state.next_id, 3);
    }

    /// A malformed or foreign custom entry never counts as a state entry —
    /// the caller falls back to toolResult snapshots.
    #[test]
    fn malformed_state_entries_do_not_count() {
        let entries = vec![
            state_entry(json!({"tasks": "not-an-array", "nextId": 1})),
            json!({"id": "c2", "customType": "other", "data": {"tasks": [], "nextId": 1}}),
        ];
        assert!(replay_from_session_entries(&entries).is_none());
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
