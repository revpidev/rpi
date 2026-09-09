//! Mid-session lifecycle reconciliation (`before_agent_start`).
//!
//! Port of upstream `packages/rpiv-ask-user-question/reconcile.ts` @
//! `338b264c`. Strips or re-adds `ask_user_question` to the active set so it
//! is invisible to the LLM in non-interactive runs (no UI) and present in
//! interactive ones. The only gating signal is `ctx.hasUI`; RPC hosts
//! (`ctx.mode == "rpc"`) deliberately keep the tool — `hasUI` is true there
//! and the TE29 dialog walker renders it.
//!
//! Idempotent: when the tool is already in the right state the active set
//! (and sibling tools) is left untouched (the pure function returns `None`).

use serde_json::json;

use crate::{HostCall, HostError};

/// The tool name this module reconciles (upstream `ASK_USER_QUESTION_TOOL_NAME`).
pub const ASK_USER_QUESTION_TOOL_NAME: &str = "ask_user_question";

/// Pure reconcile decision: `None` = no change, `Some(names)` = new active set
/// in the same order as the input (appended tool goes last).
pub fn reconcile_active_tools(
    active: &[String],
    has_ui: bool,
    tool_name: &str,
) -> Option<Vec<String>> {
    let has_tool = active.iter().any(|name| name == tool_name);
    if !has_ui && has_tool {
        Some(
            active
                .iter()
                .filter(|name| name.as_str() != tool_name)
                .cloned()
                .collect(),
        )
    } else if has_ui && !has_tool {
        let mut next = active.to_vec();
        next.push(tool_name.to_owned());
        Some(next)
    } else {
        None
    }
}

/// Register the `before_agent_start` handler (`registerAskUserQuestionReconciler`).
pub fn register(host: &dyn HostCall) -> Result<(), HostError> {
    host.call("on", json!({ "event": "before_agent_start" }))
        .map(|_| ())
}

/// Handler body: read `ctx.hasUI` + `getActiveTools`, apply the pure decision
/// and call `setActiveTools` only when it changes something.
///
/// Host-call failures degrade to "no UI / empty active set": the entry guard
/// in `tool::execute` still returns `no_ui` for non-interactive runs, so a
/// transiently stale host cannot leak the tool into an LLM turn (backstop
/// documented in the upstream reconcile header).
pub fn handle_before_agent_start(host: &dyn HostCall) -> Result<(), HostError> {
    let has_ui = host
        .call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let active: Vec<String> = host
        .call("getActiveTools", json!({}))
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    if let Some(next) = reconcile_active_tools(&active, has_ui, ASK_USER_QUESTION_TOOL_NAME) {
        host.call("setActiveTools", json!({ "toolNames": next }))
            .map(|_| ())?;
    }
    Ok(())
}

/// Test seam: apply the reconcile decision to a host without registering.
pub fn reconcile_once(host: &dyn HostCall) -> Result<Option<Vec<String>>, HostError> {
    let has_ui = host
        .call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let active: Vec<String> = host
        .call("getActiveTools", json!({}))
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let next = reconcile_active_tools(&active, has_ui, ASK_USER_QUESTION_TOOL_NAME);
    if let Some(next) = &next {
        host.call("setActiveTools", json!({ "toolNames": next }))?;
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn reconcile_strips_and_restores_without_touching_siblings() {
        let active = names(&["read", ASK_USER_QUESTION_TOOL_NAME, "bash"]);
        assert_eq!(
            reconcile_active_tools(&active, false, ASK_USER_QUESTION_TOOL_NAME),
            Some(names(&["read", "bash"])),
            "strip preserves sibling order"
        );
        let active = names(&["read", "bash"]);
        assert_eq!(
            reconcile_active_tools(&active, true, ASK_USER_QUESTION_TOOL_NAME),
            Some(names(&["read", "bash", ASK_USER_QUESTION_TOOL_NAME])),
            "restore appends"
        );
    }

    #[test]
    fn reconcile_is_idempotent_when_already_in_state() {
        assert_eq!(
            reconcile_active_tools(&names(&["read"]), false, ASK_USER_QUESTION_TOOL_NAME),
            None,
            "no UI + tool absent -> no change"
        );
        assert_eq!(
            reconcile_active_tools(
                &names(&[ASK_USER_QUESTION_TOOL_NAME]),
                true,
                ASK_USER_QUESTION_TOOL_NAME
            ),
            None,
            "has UI + tool present -> no change"
        );
    }

    #[test]
    fn reconcile_rpc_hosts_keep_the_tool() {
        // RPC hosts report hasUI = true, so the tool stays / is restored.
        assert_eq!(
            reconcile_active_tools(&names(&["read"]), true, ASK_USER_QUESTION_TOOL_NAME),
            Some(names(&["read", ASK_USER_QUESTION_TOOL_NAME]))
        );
        assert_eq!(
            reconcile_active_tools(
                &names(&[ASK_USER_QUESTION_TOOL_NAME]),
                true,
                ASK_USER_QUESTION_TOOL_NAME
            ),
            None
        );
    }
}
