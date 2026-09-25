//! Pure reducer: `(state, action, params) → (state, op)`.
//!
//! Port of upstream `packages/rpiv-todo/state/state-reducer.ts` @ `0fdf4f8`.
//! The response envelope ([`crate::tool::envelope`]) owns formatting, the
//! store ([`crate::state::store`]) owns commit. Validation is in-line:
//! structural guards (`subject required`, `id required`, `at least one
//! mutable field`) plus state-aware checks (transition legality,
//! dangling/deleted blockedBy, self-block, cycles). Decision: validation
//! stays in-reducer.
//!
//! Parameters arrive as the raw open-shape JSON bag (upstream
//! `TaskMutationParams` index signature; pi's tool dispatch performs no
//! schema coercion — `tool-definition-wrapper.ts` passes `params` through
//! verbatim @ pin). Field extraction therefore mirrors the upstream
//! runtime semantics precisely:
//!
//! - `id` / `blockedBy*` elements are compared with JS strict equality
//!   (`t.id === dep`): any JSON number compares numerically (`1 === 1.0`),
//!   strings/bools never match; error strings interpolate the JS
//!   `String(value)` form ([`js_string`]).
//! - string fields taken from a non-string value (schema-violating input
//!   the upstream stores verbatim into task rows only for its deferred
//!   sanitizers to crash on) are treated as absent — see task-file §7
//!   implementation ruling.
//! - `metadata` accepts any object value; a `null` member deletes the key
//!   (documented schema contract).

use serde_json::Value;

use crate::state::invariants::is_transition_valid;
use crate::state::task_graph::detect_cycle;
use crate::state::TaskState;
use crate::tool::types::{Task, TaskAction, TaskStatus};

/// Reducer outcome. Closed tagged union — adding a new action requires
/// extending this union AND the response envelope's `format_content` match
/// (compiler-enforced exhaustive; upstream `Op`).
///
/// `Error` carries the message in-band so callers can match on
/// `op.kind == Error` without a side-channel boolean.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Create {
        task_id: i64,
    },
    Update {
        id: i64,
        from_status: TaskStatus,
        to_status: TaskStatus,
        changed: bool,
    },
    Delete {
        id: i64,
        subject: String,
    },
    List {
        /// `params.status` verbatim when present (upstream keeps the raw
        /// value in `statusFilter`; non-status strings simply filter to an
        /// empty view).
        status_filter: Option<Value>,
        include_deleted: bool,
    },
    Get {
        task: Task,
    },
    Clear {
        count: usize,
    },
    Error {
        message: String,
    },
}

/// Reducer result pair (upstream `ApplyResult`).
pub struct ApplyResult {
    pub state: TaskState,
    pub op: Op,
}

fn error_result(state: TaskState, message: impl Into<String>) -> ApplyResult {
    ApplyResult {
        state,
        op: Op::Error {
            message: message.into(),
        },
    }
}

// ---------------------------------------------------------------------------
// JS value helpers (open-bag parity)
// ---------------------------------------------------------------------------

/// JS `String(value)` for error-message interpolation: numbers render
/// without a trailing `.0` (`1`), floats verbatim (`1.5`), strings raw,
/// `null` as `null`, booleans as themselves, objects as `[object Object]`.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Number(number) => {
            // JS `String(number)`: integral floats render without the
            // fractional part (`String(2.0) === "2"`) — serde's f64 Display
            // would print `2.0` (F6).
            match number.as_f64() {
                Some(float) if float.fract() == 0.0 && float.abs() <= 9.007_199_254_740_992e15 => {
                    format!("{}", float as i64)
                }
                _ => number.to_string(),
            }
        }
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
    }
}

/// JS strict-equality `task.id === value` for a JSON value: only numbers
/// compare (`1` matches `1` and `1.0`), never strings/bools/null.
fn id_matches(task_id: i64, value: &Value) -> bool {
    match value.as_f64() {
        Some(number) => number == task_id as f64,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Param extraction (upstream `TaskMutationParams` open bag)
// ---------------------------------------------------------------------------

/// `params.subject?.trim()` truthiness (create): a string exists and is not
/// whitespace-only. Non-string values take the same rejection path as
/// missing/empty (upstream `3?.trim()` throws; structural rejection here —
/// task-file §7 ruling).
fn subject_present(map: &serde_json::Map<String, Value>) -> Option<&str> {
    map.get("subject")
        .and_then(Value::as_str)
        .filter(|subject| !subject.trim().is_empty())
}

/// Truthy string field (create conditional appends: `if (params.description)`).
fn truthy_str<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// Defined string field (update overwrites: `params.x !== undefined`).
/// Non-string values are treated as absent (task-file §7 ruling).
fn defined_str<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// Array field as raw values (`params.blockedBy` etc.).
fn value_list<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a [Value]> {
    map.get(key).and_then(Value::as_array).map(Vec::as_slice)
}

/// `params.id` verbatim when the key is present (`params.id === undefined`
/// is the only "missing" case — `null` proceeds to the not-found path).
fn id_value(map: &serde_json::Map<String, Value>) -> Option<&Value> {
    map.get("id")
}

// ---------------------------------------------------------------------------
// Change detection (upstream `taskChanged`)
// ---------------------------------------------------------------------------

fn same_number_list(a: Option<&Vec<i64>>, b: Option<&Vec<i64>>) -> bool {
    let x = a.map(Vec::as_slice).unwrap_or(&[]);
    let y = b.map(Vec::as_slice).unwrap_or(&[]);
    x == y
}

/// `sameRecord`: JSON-equality of the metadata maps (absent ↔ absent).
fn same_record(
    a: Option<&serde_json::Map<String, Value>>,
    b: Option<&serde_json::Map<String, Value>>,
) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        (None, None) => true,
        _ => false,
    }
}

/// Did this `update` change anything? Compares the task before/after the
/// params are applied. A no-effect update — `status` set to its current
/// value, or any field re-sent unchanged — returns false, letting the
/// response envelope say "No change" instead of "Updated #N". Without
/// this, a no-op update is indistinguishable from a real mutation, which
/// can drive a model to re-issue the same call in a loop.
///
/// blockedBy is order-sensitive (the reducer preserves insertion order);
/// metadata round-trips through JSON persistence, so structural equality is
/// the operative notion of "changed" (upstream JSON.stringify — key order
/// is inherited from the current row on merge, so the comparison never
/// sees equal-content/different-order pairs).
fn task_changed(before: &Task, after: &Task) -> bool {
    before.subject != after.subject
        || before.status != after.status
        || before.description != after.description
        || before.active_form != after.active_form
        || before.owner != after.owner
        || !same_number_list(before.blocked_by.as_ref(), after.blocked_by.as_ref())
        || !same_record(before.metadata.as_ref(), after.metadata.as_ref())
}

// ---------------------------------------------------------------------------
// The reducer
// ---------------------------------------------------------------------------

pub fn apply_task_mutation(
    state: TaskState,
    action: TaskAction,
    params: &serde_json::Map<String, Value>,
) -> ApplyResult {
    match action {
        TaskAction::Create => {
            let Some(subject) = subject_present(params) else {
                return error_result(state, "subject required for create");
            };
            if let Some(blocked_by) =
                value_list(params, "blockedBy").filter(|list| !list.is_empty())
            {
                for dep in blocked_by {
                    let Some(dep_task) = state.tasks.iter().find(|task| id_matches(task.id, dep))
                    else {
                        return error_result(
                            state,
                            format!("blockedBy: #{} not found", js_string(dep)),
                        );
                    };
                    if dep_task.status == TaskStatus::Deleted {
                        return error_result(
                            state,
                            format!("blockedBy: #{} is deleted", js_string(dep)),
                        );
                    }
                }
            }
            let mut new_task = Task {
                id: state.next_id,
                subject: subject.to_owned(),
                status: TaskStatus::Pending,
                description: None,
                active_form: None,
                blocked_by: None,
                owner: None,
                metadata: None,
            };
            // Truthy conditional appends (upstream `if (params.x)`), in the
            // upstream append order.
            if let Some(description) = truthy_str(params, "description") {
                new_task.description = Some(description.to_owned());
            }
            if let Some(active_form) = truthy_str(params, "activeForm") {
                new_task.active_form = Some(active_form.to_owned());
            }
            if let Some(blocked_by) =
                value_list(params, "blockedBy").filter(|list| !list.is_empty())
            {
                // Validation above guarantees every element numerically
                // matches an existing task id, so the f64→i64 narrowing is
                // lossless (`1.0` narrows to `1`, mirroring the upstream
                // element copy).
                new_task.blocked_by = Some(
                    blocked_by
                        .iter()
                        .filter_map(|dep| dep.as_f64().map(|number| number as i64))
                        .collect::<Vec<_>>(),
                );
            }
            if let Some(owner) = truthy_str(params, "owner") {
                new_task.owner = Some(owner.to_owned());
            }
            if let Some(metadata) = params.get("metadata").and_then(Value::as_object) {
                // Upstream `if (params.metadata)` — any object is truthy in
                // JS, so an empty `{}` is stored verbatim (F2).
                new_task.metadata = Some(metadata.clone());
            }

            let task_id = new_task.id;
            let mut tasks = state.tasks.clone();
            tasks.push(new_task);
            ApplyResult {
                state: TaskState {
                    tasks,
                    next_id: state.next_id + 1,
                },
                op: Op::Create { task_id },
            }
        }

        TaskAction::Update => {
            let Some(id_value) = id_value(params) else {
                return error_result(state, "id required for update");
            };
            let Some(index) = state
                .tasks
                .iter()
                .position(|task| id_matches(task.id, id_value))
            else {
                return error_result(state, format!("#{} not found", js_string(id_value)));
            };
            let current = &state.tasks[index];

            // hasMutation: the upstream field-presence check (a present key
            // counts regardless of value shape).
            let has_mutation = params.contains_key("subject")
                || params.contains_key("description")
                || params.contains_key("activeForm")
                || params.contains_key("status")
                || params.contains_key("owner")
                || params.contains_key("metadata")
                || value_list(params, "addBlockedBy").is_some_and(|list| !list.is_empty())
                || value_list(params, "removeBlockedBy").is_some_and(|list| !list.is_empty());
            if !has_mutation {
                return error_result(
                    state,
                    "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy",
                );
            }

            let mut new_status = current.status;
            if let Some(status_value) = params.get("status") {
                // `isTransitionValid(current.status, params.status)`: a
                // non-status value never hits the table (upstream
                // `VALID_TRANSITIONS[from].has(to)` miss) → illegal.
                let target = status_value.as_str().and_then(TaskStatus::parse);
                let valid = target.is_some_and(|to| is_transition_valid(current.status, to));
                if !valid {
                    let message = format!(
                        "illegal transition {} → {}",
                        current.status.as_str(),
                        js_string(status_value)
                    );
                    return error_result(state, message);
                }
                // Target is Some here: `valid` requires it.
                new_status = target.unwrap_or(current.status);
            }

            let mut new_blocked_by = current.blocked_by.clone().unwrap_or_default();
            if let Some(to_remove) =
                value_list(params, "removeBlockedBy").filter(|list| !list.is_empty())
            {
                new_blocked_by
                    .retain(|dep| !to_remove.iter().any(|removed| id_matches(*dep, removed)));
            }
            if let Some(to_add) = value_list(params, "addBlockedBy").filter(|list| !list.is_empty())
            {
                for dep in to_add {
                    if id_matches(current.id, dep) {
                        let message = format!("cannot block #{} on itself", current.id);
                        return error_result(state, message);
                    }
                    let Some(dep_task) = state.tasks.iter().find(|task| id_matches(task.id, dep))
                    else {
                        let message = format!("addBlockedBy: #{} not found", js_string(dep));
                        return error_result(state, message);
                    };
                    if dep_task.status == TaskStatus::Deleted {
                        let message = format!("addBlockedBy: #{} is deleted", js_string(dep));
                        return error_result(state, message);
                    }
                    let known = new_blocked_by
                        .iter()
                        .any(|existing| id_matches(*existing, dep));
                    if !known {
                        // Presence-checked above (id_matches passed against a
                        // task row), so the narrowing is lossless.
                        if let Some(dep_id) = dep.as_f64().map(|number| number as i64) {
                            new_blocked_by.push(dep_id);
                        }
                    }
                }
                if detect_cycle(&state.tasks, current.id, &new_blocked_by) {
                    return error_result(
                        state,
                        "addBlockedBy would create a cycle in the blockedBy graph",
                    );
                }
            }

            let mut new_metadata = current.metadata.clone();
            if let Some(incoming) = params.get("metadata").and_then(Value::as_object) {
                let merged = new_metadata.get_or_insert_with(serde_json::Map::new);
                for (key, value) in incoming {
                    if value.is_null() {
                        merged.remove(key);
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                if merged.is_empty() {
                    new_metadata = None;
                }
            }

            let mut updated = current.clone();
            updated.status = new_status;
            if let Some(subject) = defined_str(params, "subject") {
                updated.subject = subject.to_owned();
            }
            if let Some(description) = defined_str(params, "description") {
                updated.description = Some(description.to_owned());
            }
            if let Some(active_form) = defined_str(params, "activeForm") {
                updated.active_form = Some(active_form.to_owned());
            }
            if let Some(owner) = defined_str(params, "owner") {
                updated.owner = Some(owner.to_owned());
            }
            if !new_blocked_by.is_empty() {
                updated.blocked_by = Some(new_blocked_by);
            } else {
                updated.blocked_by = None;
            }
            updated.metadata = new_metadata;

            let id = updated.id;
            let from_status = current.status;
            let to_status = updated.status;
            let changed = task_changed(current, &updated);
            let mut tasks = state.tasks.clone();
            tasks[index] = updated;
            ApplyResult {
                state: TaskState {
                    tasks,
                    next_id: state.next_id,
                },
                op: Op::Update {
                    id,
                    from_status,
                    to_status,
                    changed,
                },
            }
        }

        TaskAction::List => ApplyResult {
            state,
            op: Op::List {
                status_filter: params.get("status").cloned(),
                include_deleted: params.get("includeDeleted").and_then(Value::as_bool)
                    == Some(true),
            },
        },

        TaskAction::Get => {
            let Some(id_value) = id_value(params) else {
                return error_result(state, "id required for get");
            };
            let Some(task) = state
                .tasks
                .iter()
                .find(|task| id_matches(task.id, id_value))
            else {
                let message = format!("#{} not found", js_string(id_value));
                return error_result(state, message);
            };
            let task = task.clone();
            ApplyResult {
                state,
                op: Op::Get { task },
            }
        }

        TaskAction::Delete => {
            let Some(id_value) = id_value(params) else {
                return error_result(state, "id required for delete");
            };
            let Some(index) = state
                .tasks
                .iter()
                .position(|task| id_matches(task.id, id_value))
            else {
                return error_result(state, format!("#{} not found", js_string(id_value)));
            };
            let current = &state.tasks[index];
            if current.status == TaskStatus::Deleted {
                let message = format!("#{} is already deleted", current.id);
                return error_result(state, message);
            }
            let mut updated = current.clone();
            updated.status = TaskStatus::Deleted;
            let id = updated.id;
            let subject = updated.subject.clone();
            let mut tasks = state.tasks.clone();
            tasks[index] = updated;
            ApplyResult {
                state: TaskState {
                    tasks,
                    next_id: state.next_id,
                },
                op: Op::Delete { id, subject },
            }
        }

        TaskAction::Clear => {
            let count = state.tasks.len();
            ApplyResult {
                state: TaskState {
                    tasks: Vec::new(),
                    next_id: 1,
                },
                op: Op::Clear { count },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of upstream `state/state-reducer.test.ts` @ `0fdf4f8` (assertion
    //! semantics preserved; JS identity checks map to Rust value semantics).

    use super::*;
    use serde_json::json;

    fn empty_state() -> TaskState {
        TaskState::empty()
    }

    fn state_with(tasks: Vec<Task>) -> TaskState {
        let next_id = tasks.iter().map(|task| task.id).max().unwrap_or(0) + 1;
        TaskState { tasks, next_id }
    }

    fn task(id: i64, subject: &str) -> Task {
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

    fn task_full(id: i64, subject: &str, status: TaskStatus) -> Task {
        Task {
            status,
            ..task(id, subject)
        }
    }

    fn params(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // applyTaskMutation — create
    // ------------------------------------------------------------------

    #[test]
    fn create_rejects_empty_subject() {
        let result = apply_task_mutation(
            empty_state(),
            TaskAction::Create,
            &params(json!({"subject": ""})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "subject required for create".to_owned()
            }
        );
        assert!(result.state.tasks.is_empty());
        assert_eq!(result.state.next_id, 1);
    }

    #[test]
    fn create_rejects_whitespace_only_subject() {
        // Upstream `params.subject?.trim()` — whitespace-only is falsy.
        let result = apply_task_mutation(
            empty_state(),
            TaskAction::Create,
            &params(json!({"subject": "   "})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "subject required for create".to_owned()
            }
        );
    }

    #[test]
    fn create_rejects_dangling_blocked_by() {
        let result = apply_task_mutation(
            empty_state(),
            TaskAction::Create,
            &params(json!({"subject": "x", "blockedBy": [99]})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "blockedBy: #99 not found".to_owned()
            }
        );
        assert_eq!(result.state.next_id, 1);
    }

    #[test]
    fn create_rejects_deleted_blocked_by() {
        let state = state_with(vec![task_full(1, "done", TaskStatus::Deleted)]);
        let result = apply_task_mutation(
            state.clone(),
            TaskAction::Create,
            &params(json!({"subject": "new", "blockedBy": [1]})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "blockedBy: #1 is deleted".to_owned()
            }
        );
        assert_eq!(
            state,
            state_with(vec![task_full(1, "done", TaskStatus::Deleted)])
        );
    }

    #[test]
    fn create_assigns_next_id_and_preserves_immutability() {
        let state = empty_state();
        let result = apply_task_mutation(
            state.clone(),
            TaskAction::Create,
            &params(json!({"subject": "write tests"})),
        );
        assert_eq!(result.state.tasks.len(), 1);
        assert_eq!(result.state.tasks[0].id, 1);
        assert_eq!(result.state.tasks[0].subject, "write tests");
        assert_eq!(result.state.tasks[0].status, TaskStatus::Pending);
        assert_eq!(result.state.next_id, 2);
        // Immutability: the input state is untouched.
        assert!(state.tasks.is_empty());
        assert_eq!(result.op, Op::Create { task_id: 1 });
    }

    #[test]
    fn create_stores_an_empty_metadata_object_verbatim() {
        // Upstream `if (params.metadata)` — any object is truthy in JS, so
        // `metadata: {}` IS stored and appears in the details snapshot
        // (F2).
        let result = apply_task_mutation(
            empty_state(),
            TaskAction::Create,
            &params(json!({"subject": "x", "metadata": {}})),
        );
        assert_eq!(result.state.tasks[0].metadata, Some(serde_json::Map::new()));
    }

    #[test]
    fn create_non_integer_blocked_by_rejects_with_js_string() {
        // Open-bag parity: a legal-schema float id that matches no task row
        // interpolates the JS String() form.
        let result = apply_task_mutation(
            empty_state(),
            TaskAction::Create,
            &params(json!({"subject": "x", "blockedBy": [1.5]})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "blockedBy: #1.5 not found".to_owned()
            }
        );
    }

    // ------------------------------------------------------------------
    // applyTaskMutation — update
    // ------------------------------------------------------------------

    #[test]
    fn update_rejects_id_only() {
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(state, TaskAction::Update, &params(json!({"id": 1})));
        assert_eq!(
            result.op,
            Op::Error {
                message: "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy".to_owned()
            }
        );
    }

    #[test]
    fn update_rejects_illegal_transition_completed_to_in_progress() {
        let state = state_with(vec![task_full(1, "x", TaskStatus::Completed)]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "status": "in_progress"})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "illegal transition completed → in_progress".to_owned()
            }
        );
    }

    #[test]
    fn update_rejects_illegal_transition_with_non_status_value() {
        // `VALID_TRANSITIONS[from].has(to)` miss for a non-status string.
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "status": "weird"})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "illegal transition pending → weird".to_owned()
            }
        );
    }

    #[test]
    fn update_allows_completed_to_deleted() {
        let state = state_with(vec![task_full(1, "x", TaskStatus::Completed)]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "status": "deleted"})),
        );
        assert_eq!(
            result.op,
            Op::Update {
                id: 1,
                from_status: TaskStatus::Completed,
                to_status: TaskStatus::Deleted,
                changed: true,
            }
        );
        assert_eq!(result.state.tasks[0].status, TaskStatus::Deleted);
    }

    #[test]
    fn update_flags_no_effect_status_update_as_unchanged() {
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "status": "pending"})),
        );
        assert_eq!(
            result.op,
            Op::Update {
                id: 1,
                from_status: TaskStatus::Pending,
                to_status: TaskStatus::Pending,
                changed: false,
            }
        );
    }

    #[test]
    fn update_flags_resent_identical_fields_as_unchanged() {
        let state = state_with(vec![Task {
            description: Some("d".to_owned()),
            ..task(1, "x")
        }]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "subject": "x", "description": "d"})),
        );
        assert!(matches!(result.op, Op::Update { changed: false, .. }));
    }

    #[test]
    fn update_flags_blocked_by_only_update_as_changed() {
        let state = state_with(vec![task(1, "a"), task(2, "b")]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "addBlockedBy": [2]})),
        );
        assert_eq!(
            result.op,
            Op::Update {
                id: 1,
                from_status: TaskStatus::Pending,
                to_status: TaskStatus::Pending,
                changed: true,
            }
        );
    }

    #[test]
    fn update_flags_subject_only_update_on_task_with_deps_as_changed() {
        let state = state_with(vec![
            Task {
                blocked_by: Some(vec![2]),
                ..task(1, "old")
            },
            task(2, "dep"),
        ]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "subject": "new"})),
        );
        assert!(matches!(result.op, Op::Update { changed: true, .. }));
        assert_eq!(result.state.tasks[0].blocked_by, Some(vec![2]));
    }

    #[test]
    fn update_flags_dependency_swap_same_length_as_changed() {
        let state = state_with(vec![
            Task {
                blocked_by: Some(vec![2]),
                ..task(1, "a")
            },
            task(2, "b"),
            task(3, "c"),
        ]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "removeBlockedBy": [2], "addBlockedBy": [3]})),
        );
        assert!(matches!(result.op, Op::Update { changed: true, .. }));
        assert_eq!(result.state.tasks[0].blocked_by, Some(vec![3]));
    }

    #[test]
    fn update_rejects_self_block() {
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "addBlockedBy": [1]})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "cannot block #1 on itself".to_owned()
            }
        );
    }

    #[test]
    fn update_rejects_cycle_in_blocked_by_graph() {
        let state = state_with(vec![
            Task {
                blocked_by: Some(vec![2]),
                ..task(1, "a")
            },
            task(2, "b"),
        ]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 2, "addBlockedBy": [1]})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "addBlockedBy would create a cycle in the blockedBy graph".to_owned()
            }
        );
    }

    #[test]
    fn update_drops_blocked_by_when_merged_set_becomes_empty() {
        let state = state_with(vec![
            Task {
                blocked_by: Some(vec![2]),
                ..task(1, "a")
            },
            task(2, "b"),
        ]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "removeBlockedBy": [2]})),
        );
        assert_eq!(result.state.tasks[0].blocked_by, None);
    }

    #[test]
    fn update_drops_metadata_key_when_value_is_null() {
        let state = state_with(vec![Task {
            metadata: Some(json!({"a": 1, "b": 2}).as_object().cloned().unwrap()),
            ..task(1, "x")
        }]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "metadata": {"a": null}})),
        );
        assert_eq!(
            result.state.tasks[0].metadata,
            Some(json!({"b": 2}).as_object().cloned().unwrap())
        );
    }

    #[test]
    fn update_sets_and_overwrites_metadata_keys() {
        let state = state_with(vec![Task {
            metadata: Some(json!({"a": 1, "b": 2}).as_object().cloned().unwrap()),
            ..task(1, "x")
        }]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "metadata": {"a": 99, "c": 3}})),
        );
        assert_eq!(
            result.state.tasks[0].metadata,
            Some(
                json!({"a": 99, "b": 2, "c": 3})
                    .as_object()
                    .cloned()
                    .unwrap()
            )
        );
    }

    #[test]
    fn update_collapses_metadata_when_every_key_deleted() {
        let state = state_with(vec![Task {
            metadata: Some(json!({"a": 1}).as_object().cloned().unwrap()),
            ..task(1, "x")
        }]);
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 1, "metadata": {"a": null}})),
        );
        assert_eq!(result.state.tasks[0].metadata, None);
    }

    // ------------------------------------------------------------------
    // applyTaskMutation — list/get/delete/clear
    // ------------------------------------------------------------------

    #[test]
    fn list_emits_op_with_filters() {
        let state = state_with(vec![task(1, "a"), task_full(2, "b", TaskStatus::Deleted)]);
        let result = apply_task_mutation(
            state.clone(),
            TaskAction::List,
            &params(json!({"includeDeleted": true, "status": "deleted"})),
        );
        assert_eq!(
            result.op,
            Op::List {
                status_filter: Some(json!("deleted")),
                include_deleted: true,
            }
        );
        assert_eq!(result.state, state);
    }

    #[test]
    fn delete_on_already_deleted_task_errors() {
        let state = state_with(vec![task_full(1, "x", TaskStatus::Deleted)]);
        let result = apply_task_mutation(state, TaskAction::Delete, &params(json!({"id": 1})));
        assert_eq!(
            result.op,
            Op::Error {
                message: "#1 is already deleted".to_owned()
            }
        );
    }

    #[test]
    fn delete_emits_op_with_id_and_subject() {
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(state, TaskAction::Delete, &params(json!({"id": 1})));
        assert_eq!(
            result.op,
            Op::Delete {
                id: 1,
                subject: "x".to_owned()
            }
        );
        assert_eq!(result.state.tasks[0].status, TaskStatus::Deleted);
    }

    #[test]
    fn clear_emits_op_with_prior_count_and_resets_next_id() {
        let state = state_with(vec![task(5, "x")]);
        let result = apply_task_mutation(state, TaskAction::Clear, &params(json!({})));
        assert_eq!(result.op, Op::Clear { count: 1 });
        assert!(result.state.tasks.is_empty());
        assert_eq!(result.state.next_id, 1);
    }

    #[test]
    fn get_emits_op_with_resolved_task() {
        let state = state_with(vec![task(1, "alpha")]);
        let result = apply_task_mutation(state.clone(), TaskAction::Get, &params(json!({"id": 1})));
        assert_eq!(
            result.op,
            Op::Get {
                task: state.tasks[0].clone()
            }
        );
    }

    #[test]
    fn update_missing_id_and_unknown_id_errors() {
        let state = state_with(vec![task(1, "x")]);
        let result = apply_task_mutation(
            state.clone(),
            TaskAction::Update,
            &params(json!({"subject": "y"})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "id required for update".to_owned()
            }
        );
        let result = apply_task_mutation(
            state,
            TaskAction::Update,
            &params(json!({"id": 9, "subject": "y"})),
        );
        assert_eq!(
            result.op,
            Op::Error {
                message: "#9 not found".to_owned()
            }
        );
    }

    // ------------------------------------------------------------------
    // isTransitionValid
    // ------------------------------------------------------------------

    #[test]
    fn transition_is_idempotent_on_same_to_same() {
        assert!(is_transition_valid(
            TaskStatus::Completed,
            TaskStatus::Completed
        ));
    }

    #[test]
    fn transition_rejects_completed_to_in_progress() {
        assert!(!is_transition_valid(
            TaskStatus::Completed,
            TaskStatus::InProgress
        ));
    }

    #[test]
    fn transition_allows_completed_to_deleted() {
        assert!(is_transition_valid(
            TaskStatus::Completed,
            TaskStatus::Deleted
        ));
    }

    #[test]
    fn deleted_is_terminal() {
        for to in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
        ] {
            assert!(!is_transition_valid(TaskStatus::Deleted, to));
        }
        assert!(is_transition_valid(
            TaskStatus::Deleted,
            TaskStatus::Deleted
        ));
    }

    // ------------------------------------------------------------------
    // js_string / id_matches open-bag helpers
    // ------------------------------------------------------------------

    #[test]
    fn js_string_matches_js_string_interpolation() {
        assert_eq!(js_string(&json!(1)), "1");
        assert_eq!(js_string(&json!(1.5)), "1.5");
        assert_eq!(js_string(&json!("x")), "x");
        assert_eq!(js_string(&Value::Null), "null");
        assert_eq!(js_string(&json!(true)), "true");
        assert_eq!(js_string(&json!({})), "[object Object]");
    }

    #[test]
    fn js_string_renders_integral_floats_without_fraction() {
        // `String(2.0) === "2"` in JS; serde's f64 Display prints "2.0"
        // (F6).
        assert_eq!(js_string(&json!(2.0)), "2");
        assert_eq!(js_string(&json!(1.5)), "1.5");
        assert_eq!(js_string(&json!(2)), "2");
    }

    #[test]
    fn id_matches_numeric_strict_equality() {
        assert!(id_matches(1, &json!(1)));
        assert!(id_matches(1, &json!(1.0)));
        assert!(!id_matches(1, &json!(1.5)));
        assert!(!id_matches(1, &json!("1")));
        assert!(!id_matches(1, &Value::Null));
    }
}
