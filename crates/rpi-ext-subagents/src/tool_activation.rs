//! Tool activation restore (V19-03 S25/S41).
//!
//! rpi declares `subagent` directly (there is no `subagents_enable` loader),
//! so the auto-mode model-switch re-decision has no equivalent decision to
//! make. The observable half that does apply is restoration: an extension
//! unload/reload or a compaction checkpoint can leave the registered
//! `subagent` tool out of the active set; a reload/compaction restores it
//! when it was active before. An explicit user removal is not tracked as a
//! reload, so the host's own loadout remains the source of truth there.

use serde_json::{Value, json};

use crate::AsyncHostCalls;

fn host_call(calls: &AsyncHostCalls, method: &str, args: Value) -> Option<Value> {
    let response = crate::host_call_static(calls, method, args);
    if response.get("error").is_some() {
        return None;
    }
    response.get("ok").cloned()
}

fn active_tools(calls: &AsyncHostCalls) -> Option<Vec<String>> {
    host_call(calls, "getActiveTools", json!({})).and_then(|value| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    })
}

fn registered_tools(calls: &AsyncHostCalls) -> Option<Vec<String>> {
    host_call(calls, "getAllTools", json!({})).and_then(|value| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("name")
                        .and_then(Value::as_str)
                        .or_else(|| item.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
    })
}

/// Restore the subagent tool when it is registered but no longer active.
/// Returns true when the active set was changed.
pub fn restore_if_registered(calls: &AsyncHostCalls, reason: &str) -> bool {
    if !matches!(reason, "reload" | "compact") {
        return false;
    }
    let Some(active) = active_tools(calls) else {
        return false;
    };
    if active.iter().any(|name| name == "subagent") {
        return false;
    }
    let Some(registered) = registered_tools(calls) else {
        return false;
    };
    if !registered.iter().any(|name| name == "subagent") {
        return false;
    }
    let mut next = active;
    next.push("subagent".to_string());
    // The host reads `toolNames` (host_call.rs `setActiveTools`); sending any
    // other key makes the host fall back to an empty list and clear every
    // active tool.
    let _ = host_call(calls, "setActiveTools", json!({ "toolNames": next }));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static STATE: RefCell<(Vec<String>, Vec<String>)> = const { RefCell::new((Vec::new(), Vec::new())) };
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
                "getActiveTools" => json!({ "ok": state.0.clone() }),
                "getAllTools" => json!({ "ok": state.1.clone() }),
                "setActiveTools" => {
                    state.0 = request["args"]["toolNames"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "ok": true })
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

    #[test]
    fn restores_a_registered_inactive_tool_on_reload() {
        STATE.with(|state| *state.borrow_mut() = (vec![], vec!["subagent".into()]));
        assert!(restore_if_registered(&calls(), "reload"));
        STATE.with(|state| assert_eq!(state.borrow().0, vec!["subagent".to_string()]));
    }

    #[test]
    fn restore_preserves_the_existing_active_set() {
        STATE.with(|state| {
            *state.borrow_mut() = (
                vec!["read".into(), "bash".into()],
                vec!["read".into(), "bash".into(), "subagent".into()],
            );
        });
        assert!(restore_if_registered(&calls(), "compact"));
        STATE.with(|state| {
            assert_eq!(
                state.borrow().0,
                vec![
                    "read".to_string(),
                    "bash".to_string(),
                    "subagent".to_string()
                ]
            )
        });
    }

    #[test]
    fn restore_is_a_no_op_when_the_tool_is_already_active() {
        STATE.with(|state| {
            *state.borrow_mut() = (
                vec!["read".into(), "subagent".into()],
                vec!["subagent".into()],
            );
        });
        assert!(!restore_if_registered(&calls(), "reload"));
        STATE.with(|state| {
            assert_eq!(
                state.borrow().0,
                vec!["read".to_string(), "subagent".to_string()]
            )
        });
    }

    #[test]
    fn does_not_restore_on_a_normal_start_or_when_not_registered() {
        STATE.with(|state| *state.borrow_mut() = (vec![], vec!["subagent".into()]));
        assert!(!restore_if_registered(&calls(), "startup"));
        STATE.with(|state| *state.borrow_mut() = (vec![], vec![]));
        assert!(!restore_if_registered(&calls(), "reload"));
    }
}
