//! Plan approval flow (TE43 FR-E; plugin 02 §6).
//!
//! Trigger: a successful `write_plan` execution asks the user with the
//! declarative dialog primitives (`ui.select` + `ui.input` on revision).
//! Branches:
//!
//! - **Approve and execute** — the caller leaves Plan mode and injects
//!   the plan summary + execution instruction as a follow-up message.
//! - **Continue revising** — the collected feedback goes back to the
//!   model through the tool result; the model revises and calls
//!   `write_plan` again.
//! - **Abandon** — the caller leaves Plan mode without an injection.
//! - Esc (a `null` selection) counts as abandon (01 §6 R-PM-5.2).
//!
//! Degradation (no interactive UI, or an `rpc` host — both follow the
//! interactive-UI ABI R-U10 precedent): no dialog; the caller reports a
//! text note and the session stays in Plan mode (plugin 02 §6).

use serde_json::{Value, json};

use crate::HostCall;
use crate::host;
use crate::i18n;

/// The review dialog outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// Approved: leave Plan mode and inject the plan summary.
    Approved,
    /// Revision feedback to route back to the model.
    Revise(String),
    /// Abandoned (explicit choice or Esc).
    Abandoned,
    /// No dialog surface; report the text note and stay in Plan mode.
    Unavailable,
}

/// Whether the host can show the review dialog: a UI is attached and the
/// mode is not `rpc` (design §6 pins the RPC degradation).
pub fn dialog_available(host: &dyn HostCall) -> bool {
    host::has_ui(host) && host::mode_of(host).as_deref() != Some("rpc")
}

/// Truncate a plan for the dialog title / injected summary at a char
/// boundary, appending a marker when cut.
pub fn truncate_plan(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_owned();
    }
    let mut out: String = content.chars().take(max_chars).collect();
    out.push_str(i18n::SUMMARY_TRUNCATED);
    out
}

/// The review dialog title: the action plus a short plan preview.
pub fn dialog_title(path: &str, content: &str) -> String {
    let preview = truncate_plan(content, 400);
    format!("{} — {path}\n\n{preview}", i18n::REVIEW_TITLE)
}

/// Run the review dialog. Returns [`ReviewOutcome`] without applying any
/// mode/message side effect.
pub fn request(host: &dyn HostCall, path: &str, content: &str) -> ReviewOutcome {
    if !dialog_available(host) {
        return ReviewOutcome::Unavailable;
    }
    let selection = host.call(
        "ui.select",
        json!({
            "title": dialog_title(path, content),
            "options": [
                i18n::REVIEW_APPROVE,
                i18n::REVIEW_REVISE,
                i18n::REVIEW_ABANDON,
            ],
        }),
    );
    match selection {
        Ok(Value::String(choice)) if choice == i18n::REVIEW_APPROVE => ReviewOutcome::Approved,
        Ok(Value::String(choice)) if choice == i18n::REVIEW_REVISE => {
            match host.call(
                "ui.input",
                json!({
                    "title": i18n::REVISE_PROMPT,
                    "placeholder": i18n::REVISE_PLACEHOLDER,
                }),
            ) {
                Ok(Value::String(feedback)) if !feedback.trim().is_empty() => {
                    ReviewOutcome::Revise(feedback)
                }
                // Esc / empty input cancels the revision: abandon, like Esc
                // on the primary dialog.
                _ => ReviewOutcome::Abandoned,
            }
        }
        // Esc / timeout answer is `null`; anything unrecognized is treated
        // as abandon too (only our three labels can come back from the
        // host selector).
        Ok(_) => ReviewOutcome::Abandoned,
        Err(error) => {
            tracing::warn!(%error, "rpi-plan-mode: review dialog unavailable");
            ReviewOutcome::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        let long = "计划".repeat(10);
        let cut = truncate_plan(&long, 4);
        assert!(cut.starts_with("计划计划"));
        assert!(cut.ends_with(i18n::SUMMARY_TRUNCATED));
        assert_eq!(truncate_plan("short", 10), "short");
    }

    #[test]
    fn title_carries_the_path_and_a_preview() {
        let title = dialog_title("/w/.rpi/plans/s-1.md", "line one\nline two");
        assert!(title.contains(i18n::REVIEW_TITLE));
        assert!(title.contains("/w/.rpi/plans/s-1.md"));
        assert!(title.contains("line one"));
    }

    #[test]
    fn option_labels_are_the_contract_strings() {
        assert_eq!(i18n::REVIEW_APPROVE, "Approve and execute");
        assert_eq!(i18n::REVIEW_REVISE, "Continue revising");
        assert_eq!(i18n::REVIEW_ABANDON, "Abandon");
    }
}
#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::test_host::{FakeHost, SessionFakeHost, fake_reply};
    use serde_json::json;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn approve_answers_the_primary_option() {
        let _guard = serialized();
        crate::__reset_state();
        let host = SessionFakeHost::new();
        host.queue_select(json!(i18n::REVIEW_APPROVE));
        assert_eq!(
            request(&host, "/p/s-1-1.md", "the plan"),
            ReviewOutcome::Approved
        );
    }

    #[test]
    fn revise_collects_the_feedback() {
        let _guard = serialized();
        crate::__reset_state();
        let host = SessionFakeHost::new();
        host.queue_select(json!(i18n::REVIEW_REVISE));
        host.queue_input(json!("split step two"));
        assert_eq!(
            request(&host, "/p/s-1-1.md", "the plan"),
            ReviewOutcome::Revise("split step two".to_owned())
        );
    }

    #[test]
    fn cancel_and_escape_paths_abandon() {
        let _guard = serialized();
        crate::__reset_state();
        let host = SessionFakeHost::new();
        // Esc on the primary dialog.
        host.queue_select(Value::Null);
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Abandoned);
        // Revise then Esc on the feedback input.
        host.queue_select(json!(i18n::REVIEW_REVISE));
        host.queue_input(Value::Null);
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Abandoned);
        // Revise then empty feedback.
        host.queue_select(json!(i18n::REVIEW_REVISE));
        host.queue_input(json!("   "));
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Abandoned);
    }

    #[test]
    fn no_ui_or_rpc_degrades_without_dialog_calls() {
        let _guard = serialized();
        crate::__reset_state();
        let host = SessionFakeHost::new();
        host.set_has_ui(false);
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Unavailable);
        assert!(!host.methods().iter().any(|method| method == "ui.select"));
        host.set_has_ui(true);
        host.set_ctx_mode("rpc");
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Unavailable);
        assert!(!host.methods().iter().any(|method| method == "ui.select"));
    }

    #[test]
    fn a_failing_select_degrades() {
        let _guard = serialized();
        crate::__reset_state();
        let host = FakeHost::new();
        host.set("ctx.hasUI", fake_reply(json!(true)));
        host.set("ctx.mode", fake_reply(json!("tui")));
        host.push(
            "ui.select",
            Err(crate::HostError {
                kind: "unknownMethod".to_owned(),
                message: "no dialogs here".to_owned(),
            }),
        );
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Unavailable);
    }

    #[test]
    fn an_unknown_label_abandons() {
        let _guard = serialized();
        crate::__reset_state();
        let host = SessionFakeHost::new();
        host.queue_select(json!("Something else"));
        assert_eq!(request(&host, "/p", "plan"), ReviewOutcome::Abandoned);
    }
}
