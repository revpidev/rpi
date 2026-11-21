//! Manual-compaction parent wake (V19-03 S45; upstream `parent-wake.ts` /
//! `compaction-interruption.ts` @ v0.88.1).
//!
//! A background result that arrives while a manual compaction is in progress
//! is only appended (`triggerTurn:false`); it must not start a parent run
//! mid-compaction. After the compaction ends the parent is woken at most once:
//! for a deferred notice, or when a successful manual compaction cut off a
//! working parent that still has background work. An idle parent compacted by
//! `/compact` is never woken, and an Esc-stopped parent stays stopped.

use std::sync::Mutex;

use serde_json::{Value, json};

use crate::AsyncHostCalls;

#[derive(Debug, Default)]
struct WakeState {
    session_id: Option<String>,
    /// A parent run is active (agent_start .. agent_end).
    parent_active: bool,
    /// A manual compaction is in progress.
    manual_compaction: bool,
    /// The manual compaction aborted an active parent run.
    interrupted_parent: bool,
    /// A turn-triggering notice was appended during the compaction.
    deferred: bool,
    /// A wake is owed once the parent is idle and no compaction is running.
    wake_owed: bool,
}

static STATE: std::sync::LazyLock<Mutex<WakeState>> =
    std::sync::LazyLock::new(|| Mutex::new(WakeState::default()));

fn lock() -> std::sync::MutexGuard<'static, WakeState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn host_call(calls: &AsyncHostCalls, method: &str, args: Value) -> Option<Value> {
    let response = crate::host_call_static(calls, method, args);
    if response.get("error").is_some() {
        return None;
    }
    response.get("ok").cloned().filter(|value| !value.is_null())
}

fn is_idle(calls: &AsyncHostCalls) -> bool {
    host_call(calls, "ctx.isIdle", json!({}))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

fn active_background_work(session_id: &str) -> bool {
    crate::runner::background::list_runs().iter().any(|run| {
        run["sessionId"].as_str() == Some(session_id)
            && matches!(
                run["state"].as_str(),
                Some("queued") | Some("running") | Some("paused")
            )
    })
}

/// Bind the current session at `session_start`; a replacement drops old state.
pub fn bind_session(session_id: Option<&str>) {
    let mut state = lock();
    if state.session_id.as_deref() != session_id {
        *state = WakeState {
            session_id: session_id.map(str::to_string),
            ..WakeState::default()
        };
    }
}

pub fn on_agent_start() {
    let mut state = lock();
    state.parent_active = true;
    // A new run supersedes a pending wake; the run itself picks up appended
    // notices.
    state.wake_owed = false;
    state.deferred = false;
}

/// Returns true when a wake was flushed on this agent end.
pub fn on_agent_end(calls: &AsyncHostCalls) -> bool {
    {
        let mut state = lock();
        state.parent_active = false;
    }
    flush_if_ready(calls)
}

pub fn on_before_compact(reason: &str, interrupted: bool) {
    let mut state = lock();
    if reason != "manual" {
        return;
    }
    state.manual_compaction = true;
    // The host captured `is_streaming()` before `compact()` aborted the run;
    // `agent_end` has already cleared the extension-side flag by now.
    state.interrupted_parent = interrupted || state.parent_active;
    state.parent_active = false;
}

/// Returns true when a wake was flushed when the compaction ended.
pub fn on_compact_end(calls: &AsyncHostCalls, reason: &str, completed: bool) -> bool {
    let (interrupted, deferred) = {
        let mut state = lock();
        if reason != "manual" {
            return false;
        }
        let interrupted = completed && state.interrupted_parent;
        let deferred = state.deferred;
        state.manual_compaction = false;
        state.interrupted_parent = false;
        state.deferred = false;
        if interrupted || deferred {
            state.wake_owed = true;
        }
        (interrupted, deferred)
    };
    let session_id = lock().session_id.clone();
    let has_work = session_id.as_deref().is_some_and(active_background_work);
    if !has_work && !deferred {
        lock().wake_owed = false;
        return false;
    }
    let _ = interrupted;
    flush_if_ready(calls)
}

/// While a manual compaction is in progress, completion notices are appended
/// only and a wake is owed afterwards.
pub fn defer_wake(session_id: Option<&str>) -> bool {
    let mut state = lock();
    if state.session_id.as_deref() != session_id {
        return false;
    }
    if !state.manual_compaction {
        return false;
    }
    state.deferred = true;
    true
}

fn flush_if_ready(calls: &AsyncHostCalls) -> bool {
    let (owed, busy) = {
        let state = lock();
        (
            state.wake_owed,
            state.parent_active || state.manual_compaction,
        )
    };
    if !owed || busy || !is_idle(calls) {
        return false;
    }
    {
        let mut state = lock();
        if !state.wake_owed {
            return false;
        }
        state.wake_owed = false;
    }
    let _ = crate::host_call_static(
        calls,
        "sendMessage",
        json!({
            "message": {
                "customType": "subagent-compaction-resume",
                "content": "Compaction is complete. Act on the background subagent notices above, or resume the parent task now.",
                "display": false,
            },
            "options": { "triggerTurn": true },
        }),
    );
    true
}

pub fn session_shutdown(reason: &str) {
    let mut state = lock();
    if reason == "reload" {
        // Reload replaces the runtime: drop any pending wake/timer state and
        // keep only the session binding (the new runtime re-binds anyway).
        state.parent_active = false;
        state.manual_compaction = false;
        state.interrupted_parent = false;
        state.deferred = false;
        state.wake_owed = false;
        return;
    }
    *state = WakeState::default();
}

/// Test seam: reset the process-global wake state.
#[allow(dead_code)]
pub fn reset_for_test() {
    *lock() = WakeState::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    thread_local! {
        static STATE: RefCell<(bool, Vec<Value>)> = const { RefCell::new((true, Vec::new())) };
    }

    extern "C" fn host(
        _cookie: rpi_ext_host::native::PluginCookie,
        request: abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8> {
        let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
        let method = request["call"].as_str().unwrap_or("");
        let reply = STATE.with(|state| {
            let mut state = state.borrow_mut();
            match method {
                "ctx.isIdle" => json!({ "ok": state.0 }),
                "sendMessage" => {
                    state.1.push(request["args"].clone());
                    json!({ "ok": null })
                }
                _ => json!({ "error": { "message": "unknown" } }),
            }
        });
        abi_stable::std_types::RVec::from(serde_json::to_vec(&reply).unwrap_or_default())
    }

    fn calls() -> AsyncHostCalls {
        AsyncHostCalls {
            call: host,
            cookie: 0,
        }
    }

    fn reset(idle: bool) {
        reset_for_test();
        STATE.with(|state| *state.borrow_mut() = (idle, Vec::new()));
        bind_session(Some("s"));
    }

    #[test]
    fn idle_manual_compaction_does_not_wake_the_parent() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset(true);
        on_before_compact("manual", false);
        assert!(!on_compact_end(&calls(), "manual", true));
        STATE.with(|state| assert!(state.borrow().1.is_empty()));
    }

    #[test]
    fn interrupted_working_parent_with_work_wakes_once() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset(true);
        let run_id = format!("wake-{}", crate::runner::budget::random_run_id());
        let body = crate::runner::background::AsyncBody::Single {
            spec: Box::new(crate::p1::launch_child::ChildSpec::default()),
        };
        let _handle = crate::runner::background::start_run(&run_id, Some("s"), &body);
        on_agent_start();
        on_before_compact("manual", true);
        assert!(on_compact_end(&calls(), "manual", true));
        STATE.with(|state| {
            let messages = &state.borrow().1;
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["options"]["triggerTurn"], json!(true));
        });
        crate::runner::background::ASYNC_RUNS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id);
        let _ = std::fs::remove_dir_all(crate::runner::background::async_runs_dir().join(&run_id));
        let _ = std::fs::remove_dir_all(crate::recovery::launch_dir(&run_id));
    }

    #[test]
    fn deferred_notice_wakes_after_the_compaction() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset(true);
        on_before_compact("manual", false);
        assert!(defer_wake(Some("s")));
        assert!(on_compact_end(&calls(), "manual", false));
        STATE.with(|state| assert_eq!(state.borrow().1.len(), 1));
    }
}
