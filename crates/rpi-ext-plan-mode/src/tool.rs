//! The `write_plan` tool (TE43 FR-D/FR-E; plugin 02 §5–§6).
//!
//! `{ content: string }` is the only parameter: the path is always
//! derived from the bound session (structurally immune to path
//! injection). A successful write opens the review dialog; the result
//! text routes the branch back to the model.
//!
//! The tool registers at `exposure: "direct"` with `defaultActive: false`
//! (V16-14 §8-3, pinned here): outside Plan mode it is not active and thus
//! neither declared nor callable; entering Plan mode activates it through
//! the whitelist active set.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::HostCall;
use crate::config;
use crate::host;
use crate::i18n;
use crate::mode;
use crate::plan_file;
use crate::review;
use crate::view;

/// Tool name (wire + prompt surface).
pub const TOOL_NAME: &str = config::WRITE_PLAN_TOOL;

/// Command name registered for `/plan` (host routes `command` dispatches
/// by name).
pub const COMMAND_NAME: &str = "plan";

/// Maximum injected plan-summary size (chars).
pub const SUMMARY_LIMIT: usize = 4000;

/// The `registerTool` payload.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "label": TOOL_NAME,
        "description": i18n::TOOL_DESCRIPTION,
        "promptSnippet": i18n::TOOL_PROMPT_SNIPPET,
        "parameters": {
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The complete plan in Markdown. The session plan file path is fixed; do not pass a path."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        },
        "exposure": "direct",
        "defaultActive": false,
        "renderCall": true,
        "renderResult": true,
    })
}

/// The `/plan` command registration payload.
pub fn command_definition() -> Value {
    json!({
        "name": COMMAND_NAME,
        "description": "Toggle plan mode (read-only research + plan file + approval); /plan status | /plan file | /plan edit",
    })
}

/// Execute one `write_plan` call.
pub fn execute(host: &dyn HostCall, params: &Value) -> Value {
    // Defense in depth (v0.1.6 review P3): the tool is only active inside
    // Plan mode, but a nested `executeTool`/manual dispatch must not write a
    // plan or leave Plan mode outside it.
    if host::get_mode(host).as_deref() != Some("plan") {
        return error_result("write_plan: only available in plan mode".to_owned());
    }
    let Some(content) = params.get("content").and_then(Value::as_str) else {
        return error_result("write_plan: 'content' (string) is required".to_owned());
    };
    let path = match write_plan_content(host, content) {
        Ok(path) => path,
        Err(error) => {
            return error_result(format!("write_plan: cannot write the plan file: {error}"));
        }
    };
    let path = path.to_string_lossy().into_owned();
    let bytes = content.len();

    match review::request(host, &path, content) {
        review::ReviewOutcome::Approved => {
            leave_plan_mode(host);
            let summary = review::truncate_plan(content, SUMMARY_LIMIT);
            let message = format!("{summary}\n\n{}", i18n::EXECUTION_INSTRUCTION);
            if let Err(error) = host.call(
                "sendUserMessage",
                json!({
                    "content": message,
                    "options": { "deliverAs": "followUp" },
                }),
            ) {
                tracing::warn!(%error, "rpi-plan-mode: approved summary injection failed");
            }
            success_result(
                format!(
                    "Plan saved to {path} ({bytes} bytes). {}",
                    i18n::APPROVED_NOTE
                ),
                json!({ "path": path, "bytes": bytes, "outcome": "approved" }),
            )
        }
        review::ReviewOutcome::Revise(feedback) => success_result(
            format!(
                "Plan saved to {path}.\n{} {feedback}\nUpdate the plan with write_plan and request approval again.",
                i18n::REVISION_PREFIX
            ),
            json!({ "path": path, "bytes": bytes, "outcome": "revised" }),
        ),
        review::ReviewOutcome::Abandoned => {
            leave_plan_mode(host);
            success_result(
                format!("Plan saved to {path}. {}", i18n::ABANDONED_NOTE),
                json!({ "path": path, "bytes": bytes, "outcome": "abandoned" }),
            )
        }
        review::ReviewOutcome::Unavailable => success_result(
            format!("Plan saved to {path}. {}", i18n::NO_DIALOG_NOTE),
            json!({ "path": path, "bytes": bytes, "outcome": "pending" }),
        ),
    }
}

/// Leave Plan mode through the host authority and apply the boundary
/// clear synchronously (the spawned `mode_change` reconcile runs again
/// and finds nothing to do).
fn leave_plan_mode(host: &dyn HostCall) {
    if let Err(error) = host.call("setMode", json!({ "mode": "default" })) {
        tracing::warn!(%error, "rpi-plan-mode: setMode(default) failed");
    }
    mode::reconcile(host);
}

/// Write `content` to the session's current plan file, allocating the next
/// `<session>-<n>.md` on the first write of the Plan entry.
pub fn write_plan_content(host: &dyn HostCall, content: &str) -> std::io::Result<PathBuf> {
    let session = host::sid_of(host);
    let key = plan_file::session_key(&session);
    if let Some(path) = mode::cached_plan_path(host) {
        plan_file::write_file(&path, content)?;
        return Ok(path);
    }
    let cwd = host::cwd_of(host).ok_or_else(|| std::io::Error::other("ctx.cwd is unavailable"))?;
    let dir = plan_file::plan_dir(Path::new(&cwd), config::load_config().plan_dir.as_deref());
    let (path, _) = plan_file::allocate_and_write(&dir, &key, content)?;
    mode::remember_plan_path(host, path.clone());
    Ok(path)
}

fn success_result(text: String, details: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": details,
    })
}

fn error_result(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// `renderCall` dispatch (`{"kind":"render","what":"toolCall",...}`).
pub fn render_call_dispatch(message: &Value) -> Value {
    let context = message.get("context").cloned().unwrap_or(Value::Null);
    view::render_call(&context)
}

/// `renderResult` dispatch (`{"kind":"render","what":"toolResult",...}`).
pub fn render_result_dispatch(message: &Value) -> Value {
    let result = message.get("result").cloned().unwrap_or(Value::Null);
    view::render_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_pins_the_contract_surface() {
        let definition = tool_definition();
        assert_eq!(definition["name"], "write_plan");
        assert_eq!(definition["exposure"], "direct");
        assert_eq!(definition["defaultActive"], false);
        assert_eq!(definition["renderCall"], true);
        assert_eq!(definition["renderResult"], true);
        let parameters = &definition["parameters"];
        assert_eq!(parameters["required"], json!(["content"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(
            parameters["properties"].get("path").is_none(),
            "no path property exists"
        );
    }

    #[test]
    fn command_definition_is_named_plan() {
        assert_eq!(command_definition()["name"], "plan");
    }

    #[test]
    fn summary_limit_is_bounded() {
        let long = "x".repeat(SUMMARY_LIMIT + 10);
        let truncated = review::truncate_plan(&long, SUMMARY_LIMIT);
        assert!(truncated.len() > SUMMARY_LIMIT, "marker is appended");
        assert!(truncated.contains(i18n::SUMMARY_TRUNCATED));
    }
}
#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::i18n;
    use crate::review::ReviewOutcome;
    use crate::test_host::{SessionFakeHost, TestDir};
    use serde_json::json;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// A fake host with the tool registered, plan mode on, and the plan
    /// directory pinned to `dir`.
    fn plan_host(dir: &TestDir) -> SessionFakeHost {
        crate::config::set_test_config(Some(Some(format!(
            "planDir = \"{}\"",
            dir.path().display()
        ))));
        let host = SessionFakeHost::new();
        host.register_tool(TOOL_NAME, "direct", false);
        host.set_mode("plan");
        host
    }

    #[test]
    fn approve_writes_the_file_leaves_plan_mode_and_injects_the_summary() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("approve");
        let host = plan_host(&dir);
        host.queue_select(json!(i18n::REVIEW_APPROVE));
        let value = execute(&host, &json!({"content": "# Plan\nstep one"}));
        assert_eq!(value["details"]["outcome"], "approved", "{value}");
        let path = value["details"]["path"].as_str().expect("path");
        assert!(path.ends_with("s-1-1.md"), "{path}");
        assert_eq!(path, dir.path().join("s-1-1.md").to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(path).expect("read"),
            "# Plan\nstep one"
        );
        assert_eq!(host.view().mode, "default", "approval leaves Plan mode");
        assert_eq!(
            host.view().exposures,
            vec![
                ("read".to_owned(), "direct".to_owned()),
                ("edit".to_owned(), "direct".to_owned()),
                ("write".to_owned(), "direct".to_owned()),
                ("bash".to_owned(), "direct".to_owned()),
                ("write_plan".to_owned(), "direct".to_owned()),
            ],
            "the boundary is cleared synchronously"
        );
        let (follow_up, message) = host
            .view()
            .user_message_options
            .first()
            .cloned()
            .expect("summary injection");
        assert!(follow_up, "the summary is queued as a follow-up");
        assert!(message.contains(i18n::EXECUTION_INSTRUCTION));
        assert!(message.contains("step one"));
    }

    #[test]
    fn revise_routes_the_feedback_and_keeps_plan_mode() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("revise");
        let host = plan_host(&dir);
        host.queue_select(json!(i18n::REVIEW_REVISE));
        host.queue_input(json!("add a test step"));
        let value = execute(&host, &json!({"content": "v1"}));
        assert_eq!(value["details"]["outcome"], "revised", "{value}");
        let text = value["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("add a test step"), "{text}");
        assert_eq!(host.view().mode, "plan", "revision stays in Plan mode");
        assert!(host.view().user_messages.is_empty());

        // The model revises and writes again: the same file is overwritten.
        host.queue_select(json!(i18n::REVIEW_APPROVE));
        let value = execute(&host, &json!({"content": "v2 with a test step"}));
        assert_eq!(value["details"]["outcome"], "approved", "{value}");
        let path = value["details"]["path"].as_str().expect("path");
        assert!(path.ends_with("s-1-1.md"), "same file: {path}");
        assert_eq!(
            std::fs::read_to_string(path).expect("read"),
            "v2 with a test step"
        );
    }

    #[test]
    fn abandon_leaves_plan_mode_without_injection() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("abandon");
        let host = plan_host(&dir);
        host.queue_select(serde_json::Value::Null);
        let value = execute(&host, &json!({"content": "discarded"}));
        assert_eq!(value["details"]["outcome"], "abandoned", "{value}");
        assert_eq!(host.view().mode, "default");
        assert!(host.view().user_messages.is_empty());
    }

    #[test]
    fn no_dialog_degrades_to_a_text_note_and_stays_in_plan_mode() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("pending");
        let host = plan_host(&dir);
        host.set_has_ui(false);
        let value = execute(&host, &json!({"content": "for later"}));
        assert_eq!(value["details"]["outcome"], "pending", "{value}");
        assert_eq!(host.view().mode, "plan");
        assert!(!host.methods().iter().any(|method| method == "ui.select"));
    }

    #[test]
    fn missing_content_is_an_error_result() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("missing");
        let host = plan_host(&dir);
        let value = execute(&host, &json!({}));
        assert_eq!(value["isError"], true, "{value}");
        assert!(!host.methods().iter().any(|method| method == "ui.select"));
    }

    #[test]
    fn a_path_parameter_is_ignored_structurally() {
        let _guard = serialized();
        crate::__reset_state();
        let dir = TestDir::new("injection");
        let host = plan_host(&dir);
        host.queue_select(serde_json::Value::Null);
        let value = execute(
            &host,
            &json!({"content": "safe", "path": "/tmp/rpi-plan-mode-evil.md"}),
        );
        assert_eq!(value["details"]["outcome"], "abandoned", "{value}");
        let path = value["details"]["path"].as_str().expect("path");
        assert!(
            path.starts_with(&dir.path().to_string_lossy().into_owned()),
            "the injected path is never used: {path}"
        );
        assert!(!std::path::Path::new("/tmp/rpi-plan-mode-evil.md").exists());
    }

    #[test]
    fn review_outcome_type_is_the_contract() {
        let outcomes = [
            ReviewOutcome::Approved,
            ReviewOutcome::Revise("x".to_owned()),
            ReviewOutcome::Abandoned,
            ReviewOutcome::Unavailable,
        ];
        assert_eq!(outcomes.len(), 4);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_call_dispatch_reads_the_context_args() {
        let message = json!({
            "kind": "render",
            "what": "toolCall",
            "toolName": "write_plan",
            "context": {"args": {"content": "abc"}},
        });
        assert_eq!(
            render_call_dispatch(&message)["props"]["text"],
            "✎ write_plan · 3 B plan"
        );
    }

    #[test]
    fn render_result_dispatch_reads_the_result_details() {
        let message = json!({
            "kind": "render",
            "what": "toolResult",
            "toolName": "write_plan",
            "result": {"details": {"path": "p.md", "bytes": 10, "outcome": "approved"}},
        });
        assert_eq!(
            render_result_dispatch(&message)["props"]["text"],
            "✎ p.md · 10 B · approved"
        );
    }

    #[test]
    fn render_dispatches_tolerate_missing_payloads() {
        assert_eq!(
            render_call_dispatch(&json!({}))["props"]["text"],
            "✎ write_plan · 0 B plan"
        );
        assert_eq!(
            render_result_dispatch(&json!({}))["props"]["text"],
            "✎ plan file · saved"
        );
    }
}
