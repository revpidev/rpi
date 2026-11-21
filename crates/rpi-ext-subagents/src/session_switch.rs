//! Session-switch guard (V19-03 S12; upstream `session-switch.ts` @ v0.88.1).
//!
//! `/new`, `/resume` and `/fork` ask what happens to this session's
//! outstanding background work when the host has a UI: stay, take the work
//! into the new session, or leave it with the old one. Without a UI (print /
//! RPC) the new session inherits the work as before. Taking only moves the
//! trusted owner generation (the parent session id); the old fork never keeps
//! delivering the same run.

use std::sync::Mutex;

use serde_json::{Value, json};

use crate::AsyncHostCalls;
use crate::runner::background;

const STAY: &str = "Stay here";
const TAKE: &str = "Switch and take the work";
const LEAVE: &str = "Switch and leave the work here";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Stay,
    Take,
    Leave,
}

#[derive(Debug, Default)]
struct SwitchState {
    /// A switch attempt in progress: the source session and the chosen fate.
    pending: Option<(String, Choice)>,
}

static STATE: std::sync::LazyLock<Mutex<SwitchState>> =
    std::sync::LazyLock::new(|| Mutex::new(SwitchState::default()));

fn lock() -> std::sync::MutexGuard<'static, SwitchState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn host_call(calls: &AsyncHostCalls, method: &str, args: Value) -> Option<Value> {
    let response = crate::host_call_static(calls, method, args);
    if response.get("error").is_some() {
        return None;
    }
    response.get("ok").cloned().filter(|value| !value.is_null())
}

fn current_session(calls: &AsyncHostCalls) -> Option<String> {
    host_call(calls, "ctx.sessionFile", json!({}))
        .and_then(|session| {
            session
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty())
}

fn has_ui(calls: &AsyncHostCalls) -> bool {
    host_call(calls, "ctx.hasUI", json!({}))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Outstanding work summary for one session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Outstanding {
    pub active_runs: usize,
    pub undelivered_results: usize,
    pub unanswered_questions: usize,
}

impl Outstanding {
    pub fn any(&self) -> bool {
        self.active_runs + self.undelivered_results + self.unanswered_questions > 0
    }
}

pub fn outstanding_for(session_id: &str) -> Outstanding {
    let active_runs = background::list_runs()
        .into_iter()
        .filter(|run| {
            run["sessionId"].as_str() == Some(session_id)
                && matches!(
                    run["state"].as_str(),
                    Some("queued") | Some("running") | Some("paused")
                )
        })
        .count();
    let undelivered_results = background::pending_result_count(Some(session_id));
    let unanswered_questions =
        crate::p1::supervisor::blocking_requests_for_session(session_id).len();
    Outstanding {
        active_runs,
        undelivered_results,
        unanswered_questions,
    }
}

fn prompt(calls: &AsyncHostCalls, outstanding: Outstanding) -> Choice {
    let title = [
        "rpi-subagents: switch sessions?",
        "",
        "Background subagent work in this session:",
        &if outstanding.active_runs > 0 {
            format!("  {} run(s) queued or running", outstanding.active_runs)
        } else {
            String::new()
        },
        &if outstanding.undelivered_results > 0 {
            "  Undelivered results".to_string()
        } else {
            String::new()
        },
        &if outstanding.unanswered_questions > 0 {
            "  Questions awaiting a reply".to_string()
        } else {
            String::new()
        },
    ]
    .into_iter()
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    let selected = host_call(
        calls,
        "ui.select",
        json!({
            "title": title,
            "options": [STAY, TAKE, LEAVE],
        }),
    )
    .and_then(|value| value.as_str().map(str::to_string));
    match selected.as_deref() {
        Some(TAKE) => Choice::Take,
        Some(LEAVE) => Choice::Leave,
        _ => Choice::Stay,
    }
}

/// Handle `session_before_switch` / `session_before_fork`. Returns the event
/// result (`{cancel:true}` when the user stays).
pub fn before_switch(calls: &AsyncHostCalls, reason: &str) -> Value {
    let Some(session_id) = current_session(calls) else {
        return Value::Null;
    };
    let outstanding = outstanding_for(&session_id);
    if !outstanding.any() {
        return Value::Null;
    }
    // Without a UI the switch proceeds and the next session inherits the
    // work (upstream's automatic handoff).
    let choice = if has_ui(calls) {
        prompt(calls, outstanding)
    } else {
        Choice::Take
    };
    if choice == Choice::Stay {
        return json!({ "cancel": true });
    }
    lock().pending = Some((session_id, choice));
    let _ = reason;
    Value::Null
}

/// Apply a committed switch once the new session id is known.
pub fn session_start(calls: &AsyncHostCalls, _reason: &str) {
    let Some(new_session) = current_session(calls) else {
        return;
    };
    let pending = lock().pending.take();
    if let Some((old_session, choice)) = pending {
        match choice {
            Choice::Take => {
                background::reassign_owner(&old_session, &new_session);
                crate::recovery::reassign_owner(&old_session, &new_session);
                crate::p1::supervisor::reassign_owner(&old_session, &new_session);
            }
            Choice::Leave => {
                // Leave keeps the old owner: no trusted record or in-memory
                // run is reassigned, so the source session still owns it.
            }
            Choice::Stay => {}
        }
    }
}

/// Drop a pending switch choice at shutdown; a committed leave is already
/// represented by the unchanged owner generation.
pub fn session_shutdown(reason: &str) {
    let _ = reason;
    lock().pending = None;
}

/// Test seam: reset the process-global switch state.
#[allow(dead_code)]
pub fn reset_for_test() {
    let mut state = lock();
    state.pending = None;
}
