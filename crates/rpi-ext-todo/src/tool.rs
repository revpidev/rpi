//! Tool definition + `execute` orchestration.
//!
//! Port of upstream `packages/rpiv-todo/todo.ts` @ `0fdf4f8` for the P0
//! surface: the registration payload (schema/description/promptSnippet/
//! promptGuidelines — the eight built-in guidelines; config overrides land
//! with TE35/config.rs) and the fixed `execute` order
//! (state → reducer → commit → envelope).
//!
//! The `renderCall`/`renderResult` registration flags and renderers are
//! the TE35 surface (`view/format.rs`); P0 registers without them.

use serde_json::{json, Value};

pub mod envelope;
pub mod sanitize;
pub mod types;

use crate::config;
use crate::i18n::I18n;
use crate::state::reducer::apply_task_mutation;
use crate::state::selectors;
use crate::state::store;
use crate::tool::types::{
    todo_params_schema, TaskAction, COMMAND_NAME, DEFAULT_PROMPT_SNIPPET, DEFAULT_TOOL_DESCRIPTION,
    TOOL_LABEL, TOOL_NAME,
};

/// Build the `registerTool` payload (upstream `registerTodoTool`): the
/// guidance config overrides apply at registration time (a change needs
/// a restart — upstream reads `loadConfig().guidance` once at factory
/// scope), and the `renderCall`/`renderResult` flags advertise the
/// transcript renderers (v0.1.4 C1 render-slot surface).
pub fn tool_definition() -> Value {
    tool_definition_with_guidance(load_config_guidance().as_ref())
}

/// Test/production seam over an explicit raw `guidance` value.
pub fn tool_definition_with_guidance(guidance_value: Option<&Value>) -> Value {
    let guidance = config::validate_guidance_fields(guidance_value);
    let prompt_snippet = guidance
        .prompt_snippet
        .unwrap_or_else(|| DEFAULT_PROMPT_SNIPPET.to_owned());
    let prompt_guidelines = guidance
        .prompt_guidelines
        .unwrap_or_else(crate::tool::types::default_prompt_guidelines);
    json!({
        "name": TOOL_NAME,
        "label": TOOL_LABEL,
        "description": DEFAULT_TOOL_DESCRIPTION,
        "promptSnippet": prompt_snippet,
        "promptGuidelines": prompt_guidelines,
        "parameters": todo_params_schema(),
        "renderCall": true,
        "renderResult": true,
    })
}

/// The raw `guidance` object of the current config (registration-time
/// read; test seam injects a value).
fn load_config_guidance() -> Option<Value> {
    config::load_config().get("guidance").cloned()
}

/// Content-only error envelope for pre-reducer input failures (missing or
/// unknown `action`, non-object params). Upstream has no reachable
/// equivalent (the TypeBox-typed `params.action` switch falls through to
/// `undefined` — a Pi-side error surface, not a plugin envelope); rpi
/// answers structurally. The envelope carries NO `details`, so branch
/// replay skips it (no replayable snapshot exists for a call that never
/// reached the reducer) — task-file §7 implementation ruling.
fn invalid_params_result(message: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("Error: {message}") }],
    })
}

/// `execute` (upstream tool body): resolve the calling session's slot,
/// apply the mutation, commit, and wrap in the response envelope.
pub fn execute(host: &dyn crate::HostCall, params: &Value) -> Value {
    let sid = crate::sid_of(host);
    let Some(action_value) = params.get("action") else {
        return invalid_params_result("action is required".to_owned());
    };
    let Some(action) = action_value.as_str().and_then(TaskAction::parse) else {
        return invalid_params_result(format!(
            "unknown action {}",
            crate::state::reducer::js_string(action_value)
        ));
    };
    let Some(map) = params.as_object() else {
        return invalid_params_result("params must be an object".to_owned());
    };

    let result = apply_task_mutation(store::store().state_for(&sid), action, map);
    store::store().commit_state(&sid, result.state.clone());
    envelope::build_tool_result(action, params, &result.state, &result.op)
}

// ---------------------------------------------------------------------------
// /todos slash command (upstream `registerTodosCommand`)
// ---------------------------------------------------------------------------

/// `ctx.ui.notify` — level `"info" | "warning" | "error"`.
fn notify(host: &dyn crate::HostCall, message: &str, level: &str) {
    if let Err(error) = host.call(
        "ui.notify",
        json!({ "message": message, "notifyType": level }),
    ) {
        tracing::warn!(%error, "rpiv-todo: ui.notify rejected");
    }
}

/// The `/todos` command body (upstream `registerTodosCommand` handler):
/// error without UI, info notice when nothing is visible, else the
/// grouped output (header counts line + pending/in_progress/completed
/// sections, i18n keys driving the chrome strings). Reads the CALLING
/// session's slot — the command ctx carries session identity, unlike
/// the render hooks.
pub fn handle_todos_command(host: &dyn crate::HostCall, i18n: &I18n) {
    if !crate::has_ui(host) {
        notify(
            host,
            i18n.t(
                "command.requires_interactive",
                "/todos requires interactive mode",
            ),
            "error",
        );
        return;
    }
    let sid = crate::sid_of(host);
    let state = store::store().state_for(&sid);
    if selectors::select_visible_tasks(&state).is_empty() {
        notify(
            host,
            i18n.t(
                "command.no_todos",
                "No todos yet. Ask the agent to add some!",
            ),
            "info",
        );
        return;
    }
    let groups = selectors::select_tasks_by_status(&state);
    let counts = selectors::select_todo_counts(&state);

    let mut header: Vec<String> = Vec::new();
    if counts.completed > 0 {
        header.push(format!(
            "{}/{} {}",
            counts.completed,
            counts.total,
            i18n.format_status_label(crate::tool::types::TaskStatus::Completed)
        ));
    }
    if counts.in_progress > 0 {
        header.push(format!(
            "{} {}",
            counts.in_progress,
            i18n.format_status_label(crate::tool::types::TaskStatus::InProgress)
        ));
    }
    if counts.pending > 0 {
        header.push(format!(
            "{} {}",
            counts.pending,
            i18n.format_status_label(crate::tool::types::TaskStatus::Pending)
        ));
    }

    let mut lines = vec![header.join(" · ")];
    if !groups.pending.is_empty() {
        lines.push(
            i18n.t("command.section.pending", "── Pending ──")
                .to_owned(),
        );
        for task in &groups.pending {
            lines.push(crate::view::format_command_task_line(task, "○"));
        }
    }
    if !groups.in_progress.is_empty() {
        lines.push(
            i18n.t("command.section.in_progress", "── In Progress ──")
                .to_owned(),
        );
        for task in &groups.in_progress {
            lines.push(crate::view::format_command_task_line(task, "◐"));
        }
    }
    if !groups.completed.is_empty() {
        lines.push(
            i18n.t("command.section.completed", "── Completed ──")
                .to_owned(),
        );
        for task in &groups.completed {
            lines.push(crate::view::format_command_task_line(task, "✓"));
        }
    }

    notify(host, &lines.join("\n"), "info");
}

/// The command registration payload.
pub fn todos_command_definition() -> Value {
    json!({
        "name": COMMAND_NAME,
        "description": "Show all todos on the current branch, grouped by status",
    })
}

#[cfg(test)]
mod tests {
    //! Registration-surface golden + built-in guidance snapshot.
    //!
    //! Upstream sources: `todo.register.test.ts` (registration shape) and
    //! `todo.guidance.test.ts` (built-in defaults case) @ `0fdf4f8`; the
    //! expected literals below are transcribed verbatim from
    //! `todo.ts:69-96` and `tool/types.ts:78-128` @ `0fdf4f8` so the test
    //! is an independent copy, not a reference to the implementation.

    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // registerTodoTool — registration shape (guidance config overrides
    // land with TE35; P0 pins the built-in defaults).
    // ------------------------------------------------------------------

    #[test]
    fn registers_under_the_tool_name_todo_with_expected_label() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        let definition = tool_definition();
        assert_eq!(definition["name"], json!(TOOL_NAME));
        assert_eq!(definition["name"], json!("todo"));
        assert_eq!(definition["label"], json!("Todo"));
    }

    #[test]
    fn prompt_snippet_is_the_default() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        assert_eq!(
            tool_definition()["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
    }

    #[test]
    fn description_is_the_upstream_literal() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        assert_eq!(tool_definition()["description"], json!(
            "Manage a task list for tracking multi-step progress. Actions: create (new task), update (change status/fields/dependencies), list (all tasks, optionally filtered by status), get (single task details), delete (tombstone), clear (reset all). Status: pending → in_progress → completed, plus deleted tombstone. Use this to plan and track multi-step work like research, design, and implementation."
        ));
    }

    /// The eight built-in guidelines, transcribed from
    /// `todo.ts:75-83` @ `0fdf4f8`.
    const UPSTREAM_GUIDELINES: [&str; 8] = [
        "Use `todo` for complex work with 3+ steps, when the user gives you a list of tasks, or immediately after receiving new instructions to capture requirements. Skip it for single trivial tasks and purely conversational requests.",
        "When starting a task from the todo list, mark it in_progress BEFORE beginning work. Mark it completed IMMEDIATELY when done — never batch completions. Exactly one task in_progress at a time.",
        "Never mark a task completed if tests are failing, the implementation is partial, or you hit unresolved errors — keep it in_progress and create a new task for the blocker instead.",
        "Task status is a 4-state machine: pending → in_progress → completed, plus deleted as a tombstone. Pass activeForm (present-continuous label, e.g. 'researching existing tool') when marking in_progress.",
        "To change a task's status, call update with the task id and the target status, e.g. {\"action\":\"update\",\"id\":3,\"status\":\"completed\"} or {\"action\":\"update\",\"id\":3,\"status\":\"in_progress\",\"activeForm\":\"writing tests\"}. status is the field that changes the task; an update without a mutable field (status or another) is rejected.",
        "Use blockedBy to express dependencies (A is blocked by B). On create, pass blockedBy as the initial set. On update, use addBlockedBy / removeBlockedBy (additive merge — do not resend the full array). Cycles are rejected.",
        "list hides tombstoned (deleted) tasks by default; pass includeDeleted:true to see them. Pass status to filter by a single status.",
        "Subject must be short and imperative (e.g. 'Research existing tool'); description is for long-form detail. activeForm is a present-continuous label shown while in_progress.",
    ];

    #[test]
    fn built_in_guidelines_snapshot() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        let guidelines = crate::tool::types::default_prompt_guidelines();
        assert_eq!(guidelines.len(), 8);
        for (index, expected) in UPSTREAM_GUIDELINES.iter().enumerate() {
            assert_eq!(&guidelines[index], expected, "guideline #{index}");
        }
        assert_eq!(
            tool_definition()["promptGuidelines"],
            json!(UPSTREAM_GUIDELINES)
        );
    }

    #[test]
    fn parameters_schema_declares_the_six_actions() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        let schema = tool_definition()["parameters"].clone();
        let raw = schema.to_string();
        for action in ["create", "update", "list", "get", "delete", "clear"] {
            assert!(raw.contains(action), "missing action {action}");
        }
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["create", "update", "list", "get", "delete", "clear"])
        );
    }

    #[test]
    fn parameters_schema_field_descriptions_are_verbatim() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        let properties = &tool_definition()["parameters"]["properties"];
        assert_eq!(
            properties["subject"]["description"],
            json!("Task subject line (required for create)")
        );
        assert_eq!(
            properties["description"]["description"],
            json!("Long-form task description")
        );
        assert_eq!(
            properties["activeForm"]["description"],
            json!("Present-continuous spinner label shown while status is in_progress (e.g. 'writing tests')")
        );
        assert_eq!(
            properties["status"]["description"],
            json!("Set this task's status (update): one of pending, in_progress, completed, deleted. When action is list, filters returned tasks by this status.")
        );
        assert_eq!(
            properties["blockedBy"]["description"],
            json!("Initial blockedBy ids (create only)")
        );
        assert_eq!(
            properties["addBlockedBy"]["description"],
            json!("Task ids to add to blockedBy (update only, additive merge)")
        );
        assert_eq!(
            properties["removeBlockedBy"]["description"],
            json!("Task ids to remove from blockedBy (update only, additive merge)")
        );
        assert_eq!(
            properties["owner"]["description"],
            json!("Agent/owner assigned to this task")
        );
        assert_eq!(
            properties["metadata"]["description"],
            json!("Arbitrary metadata; pass null value for a key to delete that key on update")
        );
        assert_eq!(
            properties["id"]["description"],
            json!("Task id (required for update, get, delete)")
        );
        assert_eq!(
            properties["includeDeleted"]["description"],
            json!(
                "If true, list action returns deleted (tombstoned) tasks as well. Default: false."
            )
        );
        // Schema property order matches the upstream TypeBox declaration
        // order (pinned by the registration golden).
        let keys: Vec<&str> = properties
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "action",
                "subject",
                "description",
                "activeForm",
                "status",
                "blockedBy",
                "addBlockedBy",
                "removeBlockedBy",
                "owner",
                "metadata",
                "id",
                "includeDeleted",
            ]
        );
    }

    #[test]
    fn schema_status_enum_matches_the_state_machine() {
        // Pin to the empty-config override (review P1-2): these tests
        // read the config through load_config() but touch no store state.
        // The TEST_LOCK serializes the global override against the
        // hint/theme overlay tests that write richer configs (review R1).
        let _guard = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::config::set_test_config(Some(serde_json::json!({})));
        let properties = &tool_definition()["parameters"]["properties"];
        assert_eq!(
            properties["status"]["enum"],
            json!(["pending", "in_progress", "completed", "deleted"])
        );
    }

    // ------------------------------------------------------------------
    // renderCall/renderResult registration flags (v0.1.4 C1 render slot;
    // the upstream tool registers `renderCall`/`renderResult` closures)
    // ------------------------------------------------------------------

    #[test]
    fn tool_definition_advertises_the_render_hooks() {
        let definition = tool_definition_with_guidance(None);
        assert_eq!(definition["renderCall"], json!(true));
        assert_eq!(definition["renderResult"], json!(true));
    }

    // ------------------------------------------------------------------
    // Guidance overrides (upstream todo.guidance.test.ts — the config
    // cases; the built-in snapshot is pinned above)
    // ------------------------------------------------------------------

    fn definition_with(guidance: Value) -> Value {
        tool_definition_with_guidance(Some(&guidance))
    }

    #[test]
    fn guidance_overrides_the_prompt_snippet() {
        let definition = definition_with(json!({"promptSnippet": "Custom todo snippet"}));
        assert_eq!(definition["promptSnippet"], json!("Custom todo snippet"));
        // Guidelines stay at the built-in eight.
        assert_eq!(
            definition["promptGuidelines"].as_array().map(Vec::len),
            Some(8)
        );
    }

    #[test]
    fn guidance_overrides_the_prompt_guidelines() {
        let definition = definition_with(json!({"promptGuidelines": ["Rule one", "Rule two"]}));
        assert_eq!(
            definition["promptGuidelines"],
            json!(["Rule one", "Rule two"])
        );
        assert_eq!(
            definition["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
    }

    #[test]
    fn guidance_overrides_both_fields() {
        let definition =
            definition_with(json!({"promptSnippet": "Custom", "promptGuidelines": ["Rule"]}));
        assert_eq!(definition["promptSnippet"], json!("Custom"));
        assert_eq!(definition["promptGuidelines"], json!(["Rule"]));
    }

    #[test]
    fn guidance_falls_back_on_empty_snippet() {
        let definition = definition_with(json!({"promptSnippet": ""}));
        assert_eq!(
            definition["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
    }

    #[test]
    fn guidance_falls_back_on_wrong_types() {
        let definition =
            definition_with(json!({"promptSnippet": 123, "promptGuidelines": "not-array"}));
        assert_eq!(
            definition["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
        assert_eq!(
            definition["promptGuidelines"].as_array().map(Vec::len),
            Some(8)
        );
    }

    #[test]
    fn guidance_falls_back_on_an_empty_guideline_item() {
        let definition = definition_with(json!({"promptGuidelines": ["valid", ""]}));
        assert_eq!(
            definition["promptGuidelines"].as_array().map(Vec::len),
            Some(8)
        );
    }

    #[test]
    fn guidance_absent_keeps_the_built_ins() {
        let definition = tool_definition_with_guidance(Some(&json!({"otherField": true})));
        assert_eq!(
            definition["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
        assert_eq!(
            definition["promptGuidelines"].as_array().map(Vec::len),
            Some(8)
        );
        let definition = tool_definition_with_guidance(None);
        assert_eq!(
            definition["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
    }

    // ------------------------------------------------------------------
    // /todos command (upstream todo.command.test.ts)
    // ------------------------------------------------------------------

    /// Recording host for the command path: captures `ui.notify` calls,
    /// answers ctx questions from a fixed shape.
    struct CommandHost {
        session_id: &'static str,
        has_ui: bool,
        notifies: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl CommandHost {
        fn interactive(session_id: &'static str) -> Self {
            CommandHost {
                session_id,
                has_ui: true,
                notifies: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn headless() -> Self {
            CommandHost {
                session_id: "s1",
                has_ui: false,
                notifies: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn output(&self) -> String {
            self.notifies
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .first()
                .map(|(message, _)| message.clone())
                .unwrap_or_default()
        }

        fn level(&self) -> String {
            self.notifies
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .first()
                .map(|(_, level)| level.clone())
                .unwrap_or_default()
        }
    }

    impl crate::HostCall for CommandHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, crate::HostError> {
            match method {
                "ctx.sessionFile" => Ok(json!({"path": null, "id": self.session_id})),
                "ctx.hasUI" => Ok(json!(self.has_ui)),
                "ui.notify" => {
                    self.notifies
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push((
                            args.get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            args.get("notifyType")
                                .and_then(Value::as_str)
                                .unwrap_or("info")
                                .to_owned(),
                        ));
                    Ok(Value::Null)
                }
                _ => Ok(Value::Null),
            }
        }
    }

    fn run_command(host: &CommandHost) {
        crate::tool::handle_todos_command(host, &crate::i18n::I18n::for_locale("en"));
    }

    /// Lock the process-global store for the whole test body (the
    /// upstream vitest suite runs serially; every rpi test that touches
    /// the store holds this lock).
    fn locked() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn seeded(headless: bool, actions: &[Value]) -> CommandHost {
        let host = if headless {
            CommandHost::headless()
        } else {
            CommandHost::interactive("s1")
        };
        for params in actions {
            let _ = crate::tool::execute(&host, params);
        }
        host
    }

    #[test]
    fn todos_command_definition_registers_the_name_and_description() {
        let definition = crate::tool::todos_command_definition();
        assert_eq!(definition["name"], json!("todos"));
        assert!(definition["description"]
            .as_str()
            .is_some_and(|d| d.contains("todos")));
    }

    #[test]
    fn command_notifies_an_error_when_the_session_has_no_ui() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(true, &[]);
        run_command(&host);
        let notifies = host
            .notifies
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0].1, "error");
        assert!(notifies[0].0.contains("interactive"));
    }

    #[test]
    fn command_notifies_info_when_there_are_no_visible_tasks() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(false, &[]);
        run_command(&host);
        assert_eq!(host.level(), "info");
        assert!(host.output().contains("No todos"));
    }

    #[test]
    fn command_treats_all_deleted_tasks_as_empty() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "a"}),
                json!({"action": "update", "id": 1, "status": "deleted"}),
            ],
        );
        run_command(&host);
        assert!(host.output().contains("No todos"));
    }

    #[test]
    fn command_renders_the_pending_group() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(false, &[json!({"action": "create", "subject": "research"})]);
        run_command(&host);
        let out = host.output();
        assert!(out.contains("── Pending ──"), "{out}");
        assert!(out.contains("○ #1 research"), "{out}");
        assert!(out.contains("1 pending"), "{out}");
    }

    #[test]
    fn command_renders_the_in_progress_group_with_active_form() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "build", "activeForm": "Building"}),
                json!({"action": "update", "id": 1, "status": "in_progress"}),
            ],
        );
        run_command(&host);
        let out = host.output();
        assert!(out.contains("── In Progress ──"), "{out}");
        assert!(out.contains("◐ #1 build (Building)"), "{out}");
        assert!(out.contains("1 in progress"), "{out}");
    }

    #[test]
    fn command_renders_the_completed_group_with_ratio_header() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "ship"}),
                json!({"action": "update", "id": 1, "status": "completed"}),
            ],
        );
        run_command(&host);
        let out = host.output();
        assert!(out.contains("── Completed ──"), "{out}");
        assert!(out.contains("✓ #1 ship"), "{out}");
        assert!(out.contains("1/1 completed"), "{out}");
    }

    #[test]
    fn command_header_parts_are_ordered_completed_progress_pending() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "p"}),
                json!({"action": "create", "subject": "ip"}),
                json!({"action": "update", "id": 2, "status": "in_progress"}),
                json!({"action": "create", "subject": "done"}),
                json!({"action": "update", "id": 3, "status": "completed"}),
            ],
        );
        run_command(&host);
        let output = host.output();
        let header = output.split('\n').next().unwrap_or("");
        let i_c = header.find("completed");
        let i_ip = header.find("in progress");
        let i_p = header.find("pending");
        assert!(i_c.is_some() && i_ip.unwrap_or(0) > i_c.unwrap_or(0));
        assert!(i_p.unwrap_or(0) > i_ip.unwrap_or(0));
    }

    #[test]
    fn command_appends_the_chain_suffix_for_blocked_tasks() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "base"}),
                json!({"action": "create", "subject": "follow-up", "blockedBy": [1]}),
            ],
        );
        run_command(&host);
        assert!(host.output().contains("⛓ #1"));
    }

    #[test]
    fn command_omits_deleted_tombstones() {
        let _guard = locked();
        crate::__reset_state();
        let host = seeded(
            false,
            &[
                json!({"action": "create", "subject": "keep"}),
                json!({"action": "create", "subject": "drop"}),
                json!({"action": "update", "id": 2, "status": "deleted"}),
            ],
        );
        run_command(&host);
        let out = host.output();
        assert!(out.contains("keep"));
        assert!(!out.contains("drop"));
    }
}
