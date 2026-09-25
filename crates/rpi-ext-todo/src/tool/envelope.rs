//! Response envelope: LLM-facing content text + replay `details` snapshot.
//!
//! Port of upstream `packages/rpiv-todo/tool/response-envelope.ts` @
//! `0fdf4f8`. Pure formatter over `(op, state)`; the strings on each
//! branch are byte-equivalent to the upstream switch output.

use serde_json::{json, Value};

use crate::state::reducer::Op;
use crate::state::task_graph::derive_blocks;
use crate::state::TaskState;
use crate::tool::sanitize::sanitize_terminal_text;
use crate::tool::types::{Task, TaskAction, TaskDetails};

/// Format a single task as a `[status] #id subject [(activeForm)] [⛓ #dep,…]`
/// line. Used by the `list` content branch only — the overlay and `/todos`
/// formatting paths (TE35) use richer presentations.
fn format_list_line(task: &Task) -> String {
    let block = task
        .blocked_by
        .as_ref()
        .filter(|deps| !deps.is_empty())
        .map(|deps| {
            format!(
                " ⛓ {}",
                deps.iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .unwrap_or_default();
    let form = if task.status == crate::tool::types::TaskStatus::InProgress {
        task.active_form
            .as_deref()
            .filter(|label| !label.is_empty())
            .map(|label| format!(" ({})", sanitize_terminal_text(label)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "[{}] #{} {}{}{}",
        task.status.as_str(),
        task.id,
        sanitize_terminal_text(&task.subject),
        form,
        block
    )
}

/// Multi-line presentation for the `get` action. Order of rows is pinned
/// by pre-refactor `todo.ts:354-376` — description, activeForm, blockedBy,
/// blocks, owner — so envelope-level snapshot tests stay byte-equivalent.
fn format_get_lines(task: &Task, state: &TaskState) -> String {
    let blocks = derive_blocks(&state.tasks)
        .get(&task.id)
        .cloned()
        .unwrap_or_default();
    let mut lines = vec![format!(
        "#{} [{}] {}",
        task.id,
        task.status.as_str(),
        sanitize_terminal_text(&task.subject)
    )];
    if let Some(description) = task.description.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!(
            "  description: {}",
            sanitize_terminal_text(description)
        ));
    }
    if let Some(active_form) = task
        .active_form
        .as_deref()
        .filter(|label| !label.is_empty())
    {
        lines.push(format!(
            "  activeForm: {}",
            sanitize_terminal_text(active_form)
        ));
    }
    if let Some(deps) = task.blocked_by.as_ref().filter(|deps| !deps.is_empty()) {
        lines.push(format!(
            "  blockedBy: {}",
            deps.iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !blocks.is_empty() {
        lines.push(format!(
            "  blocks: {}",
            blocks
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(owner) = task.owner.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!("  owner: {}", sanitize_terminal_text(owner)));
    }
    lines.join("\n")
}

/// Pure formatter: `(op, state) → string` (upstream `formatContent`).
/// Closed match on the op kind — the compiler enforces a branch for every
/// [`Op`] variant.
pub fn format_content(op: &Op, state: &TaskState) -> String {
    match op {
        Op::Create { task_id } => {
            let Some(task) = state.tasks.iter().find(|task| task.id == *task_id) else {
                // Defensive — `task_id` always resolves on the success path.
                return format!("Created #{task_id}");
            };
            format!(
                "Created #{}: {} (pending)",
                task.id,
                sanitize_terminal_text(&task.subject)
            )
        }
        Op::Update {
            id,
            from_status,
            to_status,
            changed,
        } => {
            if !changed {
                return format!(
                    "No change: #{id} already matches the requested values (status: {})",
                    to_status.as_str()
                );
            }
            let transition = if from_status != to_status {
                format!(" ({} → {})", from_status.as_str(), to_status.as_str())
            } else {
                String::new()
            };
            format!("Updated #{id}{transition}")
        }
        Op::Delete { id, subject } => {
            format!("Deleted #{id}: {}", sanitize_terminal_text(subject))
        }
        Op::Clear { count } => format!("Cleared {count} tasks"),
        Op::List {
            status_filter,
            include_deleted,
        } => {
            let mut view: Vec<&Task> = state.tasks.iter().collect();
            if !include_deleted {
                view.retain(|task| task.status != crate::tool::types::TaskStatus::Deleted);
            }
            if let Some(filter) = status_filter {
                view.retain(|task| Some(task.status.as_str()) == filter.as_str());
            }
            if view.is_empty() {
                "No tasks".to_owned()
            } else {
                view.iter()
                    .map(|task| format_list_line(task))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Op::Get { task } => format_get_lines(task, state),
        Op::Error { message } => format!("Error: {message}"),
    }
}

/// Build the LLM-facing tool envelope after the store has committed the
/// reducer's new state (upstream `buildToolResult`). `details` is the
/// persistence + replay snapshot — [`crate::state::replay`] consumes this
/// exact shape on session lifecycle events. Field order and field names
/// are pinned by cross-version replay compatibility; `params` rides
/// through verbatim (`serde_json` `preserve_order` keeps the caller's key
/// order — workspace `serde_json` feature).
pub fn build_tool_result(action: TaskAction, params: &Value, state: &TaskState, op: &Op) -> Value {
    let text = format_content(op, state);
    let mut details = TaskDetails {
        action,
        params: params.as_object().cloned().unwrap_or_default(),
        tasks: state.tasks.clone(),
        next_id: state.next_id,
        error: None,
    };
    if let Op::Error { message } = op {
        details.error = Some(message.clone());
    }
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": details,
    })
}

#[cfg(test)]
mod tests {
    //! Port of upstream `tool/response-envelope.test.ts` @ `0fdf4f8`.

    use super::*;
    use crate::state::reducer::Op;
    use crate::tool::types::{Task, TaskStatus};

    fn state_with(tasks: Vec<Task>) -> TaskState {
        let next_id = tasks.iter().map(|task| task.id).max().unwrap_or(0) + 1;
        TaskState { tasks, next_id }
    }

    fn t(id: i64, subject: &str) -> Task {
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

    fn task_with(id: i64, subject: &str, f: impl FnOnce(&mut Task)) -> Task {
        let mut task = t(id, subject);
        f(&mut task);
        task
    }

    // ------------------------------------------------------------------
    // formatContent
    // ------------------------------------------------------------------

    #[test]
    fn create_formats_created_line() {
        let state = state_with(vec![t(1, "alpha")]);
        assert_eq!(
            format_content(&Op::Create { task_id: 1 }, &state),
            "Created #1: alpha (pending)"
        );
    }

    #[test]
    fn update_emits_transition_tuple_when_statuses_differ() {
        let state = state_with(vec![task_with(1, "x", |task| {
            task.status = TaskStatus::InProgress
        })]);
        let op = Op::Update {
            id: 1,
            from_status: TaskStatus::Pending,
            to_status: TaskStatus::InProgress,
            changed: true,
        };
        assert_eq!(
            format_content(&op, &state),
            "Updated #1 (pending → in_progress)"
        );
    }

    #[test]
    fn update_omits_transition_when_statuses_equal() {
        let state = state_with(vec![t(1, "x")]);
        let op = Op::Update {
            id: 1,
            from_status: TaskStatus::Pending,
            to_status: TaskStatus::Pending,
            changed: true,
        };
        assert_eq!(format_content(&op, &state), "Updated #1");
    }

    #[test]
    fn update_reports_no_change_when_unchanged() {
        let state = state_with(vec![t(1, "x")]);
        let op = Op::Update {
            id: 1,
            from_status: TaskStatus::Pending,
            to_status: TaskStatus::Pending,
            changed: false,
        };
        assert_eq!(
            format_content(&op, &state),
            "No change: #1 already matches the requested values (status: pending)"
        );
    }

    #[test]
    fn delete_formats_deleted_line() {
        assert_eq!(
            format_content(
                &Op::Delete {
                    id: 1,
                    subject: "ship".to_owned()
                },
                &state_with(Vec::new())
            ),
            "Deleted #1: ship"
        );
    }

    #[test]
    fn clear_emits_prior_count() {
        assert_eq!(
            format_content(&Op::Clear { count: 4 }, &state_with(Vec::new())),
            "Cleared 4 tasks"
        );
    }

    #[test]
    fn list_no_tasks_when_filtered_view_empty() {
        let state = state_with(vec![task_with(1, "x", |task| {
            task.status = TaskStatus::Deleted
        })]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: false
                },
                &state
            ),
            "No tasks"
        );
    }

    #[test]
    fn list_joins_per_task_lines() {
        let state = state_with(vec![
            t(1, "a"),
            task_with(2, "b", |task| {
                task.status = TaskStatus::InProgress;
                task.active_form = Some("Building".to_owned());
            }),
        ]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: false
                },
                &state
            ),
            "[pending] #1 a\n[in_progress] #2 b (Building)"
        );
    }

    #[test]
    fn get_multi_line_block_with_description_blocked_by_owner() {
        let state = state_with(vec![
            t(1, "root"),
            task_with(2, "leaf", |task| {
                task.description = Some("details".to_owned());
                task.blocked_by = Some(vec![1]);
                task.owner = Some("Sergii".to_owned());
            }),
        ]);
        let op = Op::Get {
            task: state.tasks[1].clone(),
        };
        assert_eq!(
            format_content(&op, &state),
            "#2 [pending] leaf\n  description: details\n  blockedBy: #1\n  owner: Sergii"
        );
    }

    #[test]
    fn get_emits_blocks_reverse_edge_line() {
        let state = state_with(vec![
            task_with(1, "ship", |task| task.blocked_by = Some(vec![2, 3])),
            t(2, "test"),
            t(3, "lint"),
        ]);
        let op = Op::Get {
            task: state.tasks[1].clone(),
        };
        assert_eq!(
            format_content(&op, &state),
            "#2 [pending] test\n  blocks: #1"
        );
    }

    #[test]
    fn get_emits_active_form_line_for_in_progress_task() {
        let state = state_with(vec![task_with(1, "build", |task| {
            task.status = TaskStatus::InProgress;
            task.active_form = Some("Building".to_owned());
        })]);
        let op = Op::Get {
            task: state.tasks[0].clone(),
        };
        assert_eq!(
            format_content(&op, &state),
            "#1 [in_progress] build\n  activeForm: Building"
        );
    }

    #[test]
    fn list_status_filter_narrows_to_a_single_status() {
        let state = state_with(vec![
            t(1, "a"),
            task_with(2, "b", |task| {
                task.status = TaskStatus::InProgress;
                task.active_form = Some("Working".to_owned());
            }),
            task_with(3, "c", |task| task.status = TaskStatus::Completed),
        ]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: Some(json!("in_progress")),
                    include_deleted: false,
                },
                &state
            ),
            "[in_progress] #2 b (Working)"
        );
    }

    #[test]
    fn list_include_deleted_surfaces_tombstoned_rows() {
        let state = state_with(vec![task_with(1, "x", |task| {
            task.status = TaskStatus::Deleted
        })]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: true
                },
                &state
            ),
            "[deleted] #1 x"
        );
    }

    #[test]
    fn list_chain_suffix_appears_when_task_has_blocked_by() {
        let state = state_with(vec![
            t(1, "leaf"),
            task_with(2, "task", |task| task.blocked_by = Some(vec![1])),
        ]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: false
                },
                &state
            ),
            "[pending] #1 leaf\n[pending] #2 task ⛓ #1"
        );
    }

    #[test]
    fn create_defensive_fallback_for_unknown_task_id() {
        assert_eq!(
            format_content(&Op::Create { task_id: 999 }, &state_with(Vec::new())),
            "Created #999"
        );
    }

    #[test]
    fn empty_string_fields_render_no_lines_or_parenthetical() {
        // Upstream truthiness gates: an update may STORE "" (defined,
        // `!== undefined`), but the formatters' `if (task.x)` skips it
        // (F3).
        let state = state_with(vec![task_with(1, "x", |task| {
            task.description = Some(String::new());
            task.active_form = Some(String::new());
            task.owner = Some(String::new());
        })]);
        assert_eq!(
            format_content(
                &Op::Get {
                    task: state.tasks[0].clone()
                },
                &state
            ),
            "#1 [pending] x"
        );
        let state = state_with(vec![task_with(1, "x", |task| {
            task.status = TaskStatus::InProgress;
            task.active_form = Some(String::new());
        })]);
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: false
                },
                &state
            ),
            "[in_progress] #1 x"
        );
    }

    #[test]
    fn error_prefixes_message() {
        assert_eq!(
            format_content(
                &Op::Error {
                    message: "subject required for create".to_owned()
                },
                &state_with(Vec::new())
            ),
            "Error: subject required for create"
        );
    }

    // ------------------------------------------------------------------
    // buildToolResult
    // ------------------------------------------------------------------

    #[test]
    fn details_mirror_the_canonical_shape_on_success() {
        let state = state_with(vec![t(1, "alpha")]);
        let envelope = build_tool_result(
            crate::tool::types::TaskAction::Create,
            &json!({"subject": "alpha"}),
            &state,
            &Op::Create { task_id: 1 },
        );
        assert_eq!(
            envelope,
            json!({
                "content": [{ "type": "text", "text": "Created #1: alpha (pending)" }],
                "details": {
                    "action": "create",
                    "params": { "subject": "alpha" },
                    "tasks": state.tasks,
                    "nextId": state.next_id,
                }
            })
        );
    }

    #[test]
    fn details_carry_error_message_on_error_op() {
        let envelope = build_tool_result(
            crate::tool::types::TaskAction::Create,
            &json!({"subject": ""}),
            &state_with(Vec::new()),
            &Op::Error {
                message: "subject required for create".to_owned(),
            },
        );
        assert_eq!(
            envelope["details"]["error"],
            json!("subject required for create")
        );
        assert_eq!(
            envelope["content"][0]["text"],
            json!("Error: subject required for create")
        );
    }

    #[test]
    fn details_field_order_is_pinned() {
        // Cross-version replay compatibility: action, params, tasks,
        // nextId, [error]; task rows id, subject, status, then optional
        // fields (upstream insertion order — see types.rs).
        let state = state_with(vec![task_with(1, "alpha", |task| {
            task.description = Some("d".to_owned());
        })]);
        let envelope = build_tool_result(
            crate::tool::types::TaskAction::Create,
            &json!({"subject": "alpha"}),
            &state,
            &Op::Create { task_id: 1 },
        );
        let details = envelope["details"].as_object().unwrap();
        let keys: Vec<&str> = details.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["action", "params", "tasks", "nextId"]);
        let row = envelope["details"]["tasks"][0].as_object().unwrap();
        let row_keys: Vec<&str> = row.keys().map(String::as_str).collect();
        assert_eq!(row_keys, vec!["id", "subject", "status", "description"]);
    }

    // ------------------------------------------------------------------
    // Control characters in model-controlled fields
    // ------------------------------------------------------------------

    #[test]
    fn get_strips_escape_sequences_from_subject_description_owner() {
        let state = state_with(vec![task_with(
            1,
            "safe\u{001b}[2J\u{001b}[Hsubject",
            |task| {
                task.description = Some("line1\nline2".to_owned());
                task.owner = Some("who\u{009b}31mami".to_owned());
            },
        )]);
        let op = Op::Get {
            task: state.tasks[0].clone(),
        };
        assert_eq!(
            format_content(&op, &state),
            "#1 [pending] safesubject\n  description: line1 line2\n  owner: whoami"
        );
    }

    #[test]
    fn create_list_strip_escape_sequences_from_subject_and_active_form() {
        let state = state_with(vec![task_with(1, "evil\u{001b}[31m", |task| {
            task.status = TaskStatus::InProgress;
            task.active_form = Some("clear\u{001b}[2Jing".to_owned());
        })]);
        // The create branch prints the literal "(pending)" even when the
        // stored status differs (upstream hardcodes it; F5).
        assert_eq!(
            format_content(&Op::Create { task_id: 1 }, &state),
            "Created #1: evil (pending)"
        );
        assert_eq!(
            format_content(
                &Op::List {
                    status_filter: None,
                    include_deleted: false
                },
                &state
            ),
            "[in_progress] #1 evil (clearing)"
        );
    }
}
