//! V13-07 S2: the bridge-ready retry loop must NOT push the status bar
//! while the host reports no UI — every such push lands on the null bridge
//! and is dropped (pure waste). Once the host reports a UI the push happens
//! and the loop exits. Dedicated binary: `STATE` is a OnceLock per test
//! binary, so this install cannot share with other install tests.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::install_for_test;
use serde_json::{json, Value};

const KEY: &str = "mcp";

/// In-memory host: `ctx.hasUI` is a flip-able flag; `ui.setStatus` calls
/// are counted.
struct FakeHost {
    has_ui: std::sync::Mutex<bool>,
    status_pushes: std::sync::Mutex<Vec<String>>,
}

impl FakeHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            has_ui: std::sync::Mutex::new(false),
            status_pushes: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn push_count(&self) -> usize {
        self.status_pushes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // SAFETY: the Arc handed to install_for_test stays alive for the whole
    // process (per-test FakeHost instances are never freed).
    let host = unsafe { Arc::from_raw(host_ptr as *const FakeHost) };
    let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = request.get("call").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "ctx.cwd" => json!({"ok": "/tmp"}),
        "ctx.hasUI" => {
            json!({"ok": *host.has_ui.lock().unwrap_or_else(|e| e.into_inner())})
        }
        "ui.setStatus" => {
            let key = args.get("key").and_then(Value::as_str).unwrap_or("");
            if key == KEY {
                host.status_pushes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(
                        args.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
            }
            json!({"ok": true})
        }
        "on"
        | "registerTool"
        | "registerCommand"
        | "registerMessageRenderer"
        | "registerFlag"
        | "getFlag"
        | "getActiveTools"
        | "getAllTools"
        | "ui.setWidget"
        | "ui.setFooter"
        | "ui.setStatusText"
        | "sendMessage"
        | "ctx.model" => {
            json!({"ok": null})
        }
        _ => json!({"error": {"message": method}}),
    };
    let _ = std::mem::ManuallyDrop::new(host); // avoid double-free of the Arc from_raw borrow
    RVec::from(serde_json::to_vec(&reply).unwrap_or_default())
}

#[test]
fn bridge_retry_skips_status_bar_without_ui_and_pushes_after() {
    // Shrink the 1.5s production cadence so the test stays fast.
    rpi_ext_mcp_adapter::BRIDGE_RETRY_MS_TEST.store(40, Ordering::Relaxed);

    let host = FakeHost::new();
    let response = install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host.clone()) as PluginCookie,
    );
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)), "{response}");

    // No UI for the first ~5 intervals: zero status pushes despite the
    // failed attempts (a pre-V13-07 loop pushed 15 wasted bars).
    std::thread::sleep(Duration::from_millis(3 * 40 + 30));
    assert_eq!(
        host.push_count(),
        0,
        "status bar must not be pushed while the host reports no UI"
    );

    // Flip the UI bit: the next interval must push exactly once and exit.
    *host.has_ui.lock().unwrap_or_else(|e| e.into_inner()) = true;
    std::thread::sleep(Duration::from_millis(5 * 40 + 30));
    assert_eq!(
        host.push_count(),
        1,
        "one push after the UI appears (loop exits)"
    );
}

/// V13-07 S1: `!command` secret resolution runs on the blocking pool
/// (spawn_blocking) instead of a tokio worker — functional round-trip: a
/// `!echo` secret resolves to its output through the async seam, and the
/// shared blocking pool keeps the runtime responsive (the future itself is
/// async, so a blocking-inline regression would starve this single-thread
/// test's other work).
#[tokio::test(flavor = "current_thread")]
async fn resolve_env_async_runs_on_blocking_pool() {
    use rpi_ext_mcp_adapter::metadata::ServerEntry;
    use rpi_ext_mcp_adapter::protocol::stdio::resolve_env_async;

    let definition = ServerEntry(
        json!({
            "command": "echo",
            "env": { "TOKEN": "!echo TOPSECRET" },
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    // Two parallel resolutions over the shared blocking pool — each spawns
    // the shell; both must complete (a sync loop wedging the executor would
    // deadlock a current_thread runtime).
    let (a, b) = tokio::join!(
        resolve_env_async(&definition),
        resolve_env_async(&definition)
    );
    for env in [a.expect("a resolves"), b.expect("b resolves")] {
        let token = env
            .iter()
            .find(|(key, _)| key == "TOKEN")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(token, "TOPSECRET", "env: {env:?}");
    }
}
