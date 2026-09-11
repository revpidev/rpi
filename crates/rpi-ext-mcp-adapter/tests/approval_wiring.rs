//! TE21/TE33 plugin-level wiring: session-approval restore + persistence
//! sink (R7.2.2.3/.4/.5). One install per test binary (`STATE` is a
//! `OnceLock`).
//!
//! Drives the real `install` → `dispatch` path with an in-process fake host:
//! - A6/A7 (TE21) / A3 (TE33 fallback): `session_start` restores the
//!   file-tip branch; `session_tree` rebuilds the set for the target
//!   `newLeafId` — both via the JSONL fallback when the host answers
//!   `unknownMethod` for `ctx.sessionEntries` (old host);
//! - TE33 A1/A2/A5/A6/A8/A9: the same events against a V14-25+ host whose
//!   `ctx.sessionEntries` answers the active branch from memory — including
//!   the in-memory (`--no-session`) case that closes TE-D41;
//! - A8: a session grant persists exactly one `mcp-approval-v1` entry
//!   carrying names + hashes only (no raw arguments);
//! - A11: the fallback session path comes from `ctx.sessionFile`, never a
//!   directory heuristic, and nothing under `~/.pi`/`.pi` is touched.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::metadata::ToolMetadata;
use rpi_ext_mcp_adapter::session_approvals::{
    entry_to_value, get_tool_approval_identity, MCP_APPROVAL_CUSTOM_TYPE,
};
use rpi_ext_mcp_adapter::{dispatch, dispatcher_for_test, install_for_test};
use serde_json::{json, Value};

/// What the fake host answers for `ctx.sessionEntries` (TE33).
enum SessionEntriesReply {
    /// Pre-V14-25 host: the method is absent → `unknownMethod` (probe
    /// signal for the JSONL fallback).
    Unsupported,
    /// V14-25+ host: answer this array as the active branch's custom
    /// entries (already `customType`-filtered by the host, per ADR-0027).
    Entries(Vec<Value>),
    /// V14-25+ host misbehaving: `ok: null` (protocol anomaly).
    NullOk,
    /// Host error with this kind (anything but `unknownMethod`).
    Error(&'static str),
}

struct FakeHost {
    cwd: Mutex<String>,
    session_file: Mutex<Option<String>>,
    session_file_calls: AtomicUsize,
    session_entries: Mutex<SessionEntriesReply>,
    session_entries_calls: AtomicUsize,
    append_entries: Mutex<Vec<Value>>,
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // SAFETY: the Arc handed to install_for_test is kept alive by the test
    // for the whole process; from_raw + forget only re-borrows it.
    let host = unsafe { Arc::from_raw(host_ptr as *const FakeHost) };
    let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = request.get("call").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "ctx.cwd" => json!({"ok": *host.cwd.lock().unwrap_or_else(|e| e.into_inner())}),
        "ctx.hasUI" => json!({"ok": true}),
        "ctx.mode" => json!({"ok": "tui"}),
        "getFlag" => json!({"ok": null}),
        "on" | "registerFlag" | "registerTool" | "unregisterTool" | "setActiveTools" => {
            json!({"ok": true})
        }
        "getActiveTools" | "getAllTools" => json!({"ok": []}),
        "ctx.sessionFile" => {
            host.session_file_calls.fetch_add(1, Ordering::SeqCst);
            let path = host
                .session_file
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            json!({
                "ok": {
                    "path": path,
                    "id": "s1",
                }
            })
        }
        "ctx.sessionEntries" => {
            host.session_entries_calls.fetch_add(1, Ordering::SeqCst);
            match &*host
                .session_entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
            {
                SessionEntriesReply::Unsupported => json!({"error": {
                    "kind": "unknownMethod",
                    "message": "unknown host call: ctx.sessionEntries",
                }}),
                SessionEntriesReply::Entries(entries) => json!({"ok": entries}),
                SessionEntriesReply::NullOk => json!({"ok": null}),
                SessionEntriesReply::Error(kind) => json!({"error": {
                    "kind": kind,
                    "message": "scripted sessionEntries failure",
                }}),
            }
        }
        "appendEntry" => {
            host.append_entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(json!({
                    "customType": args.get("customType").cloned().unwrap_or(Value::Null),
                    "data": args.get("data").cloned().unwrap_or(Value::Null),
                }));
            json!({"ok": null})
        }
        m if m.starts_with("ui.") => json!({"ok": null}),
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-approval-wiring-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Dispatch on a dedicated thread: `PluginRuntime::block_on` must not run on a
/// thread already driving a tokio runtime (mirrors the host's dispatch
/// thread; same pattern as `command_wiring.rs`).
fn dispatch_event(event: &str, payload: Value) -> Value {
    let event = event.to_string();
    std::thread::spawn(move || {
        let message = json!({"kind": "event", "event": event, "payload": payload});
        let bytes = serde_json::to_vec(&message).expect("json");
        let response = dispatch(std::ptr::null(), RVec::from(bytes));
        serde_json::from_slice(&response[..]).expect("event result JSON")
    })
    .join()
    .expect("dispatch thread")
}

fn approval_tool() -> ToolMetadata {
    ToolMetadata {
        name: "demo_search".to_string(),
        original_name: "search".to_string(),
        description: String::new(),
        resource_uri: None,
        input_schema: Some(json!({"type": "object"})),
    }
}

fn custom_entry(id: &str, parent: Option<&str>, data: Value) -> Value {
    json!({
        "type": "custom",
        "customType": MCP_APPROVAL_CUSTOM_TYPE,
        "data": data,
        "id": id,
        "parentId": parent,
        "timestamp": "2026-09-09T00:00:00.000Z",
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_approvals_restore_rebuild_and_persist() {
    let dir = temp_dir("restore");
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(dir.join(".mcp.json"), json!({"mcpServers": {}}).to_string()).expect("config");
    let saved_agent_dir = std::env::var_os("RPI_CODING_AGENT_DIR");
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);

    let tool = approval_tool();
    let identity_a = get_tool_approval_identity("demo", &tool, &json!({"query": "a"}));
    let identity_b = get_tool_approval_identity("demo", &tool, &json!({"query": "b"}));

    // Session branch: e1 (args a) → e2 (args b); the file tip is e2.
    let session_path = dir.join("session.jsonl");
    let session_content = [
        json!({"type": "session", "id": "s1", "version": 3}).to_string(),
        custom_entry(
            "e1",
            None,
            entry_to_value(
                &rpi_ext_mcp_adapter::session_approvals::SessionApprovalEntry::allow_for_session(
                    "demo",
                    "search",
                    &identity_a.definition_hash,
                    &identity_a.args_hash,
                ),
            ),
        )
        .to_string(),
        custom_entry(
            "e2",
            Some("e1"),
            entry_to_value(
                &rpi_ext_mcp_adapter::session_approvals::SessionApprovalEntry::allow_for_session(
                    "demo",
                    "search",
                    &identity_b.definition_hash,
                    &identity_b.args_hash,
                ),
            ),
        )
        .to_string(),
    ]
    .join("\n");
    std::fs::write(&session_path, format!("{session_content}\n")).expect("session file");

    let host = Arc::new(FakeHost {
        cwd: Mutex::new(dir.to_string_lossy().into_owned()),
        session_file: Mutex::new(Some(session_path.to_string_lossy().into_owned())),
        session_file_calls: AtomicUsize::new(0),
        // The pre-TE33 sections pin the old-host (pre-V14-25) behavior: the
        // JSONL fallback is the only read path.
        session_entries: Mutex::new(SessionEntriesReply::Unsupported),
        session_entries_calls: AtomicUsize::new(0),
        append_entries: Mutex::new(Vec::new()),
    });
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, Arc::into_raw(host.clone()) as PluginCookie);
    assert_eq!(installed, json!({"ok": true}), "install succeeds");

    // session_start → init → on_ready restore from the file tip (e1+e2).
    dispatch_event(
        "session_start",
        json!({"type": "session_start", "reason": "startup"}),
    );
    let dispatcher = dispatcher_for_test().expect("dispatcher");
    let runtime = {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(runtime) = dispatcher.try_runtime() {
                break runtime;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime init timed out"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    // A6: file-tip branch restored.
    assert!(
        runtime.approval.is_approved(&identity_a.cache_key),
        "e1 grant restored"
    );
    assert!(
        runtime.approval.is_approved(&identity_b.cache_key),
        "e2 grant restored"
    );
    assert_eq!(runtime.approval.len(), 2);
    assert!(
        host.session_file_calls.load(Ordering::SeqCst) > 0,
        "A11: restore must read the authoritative ctx.sessionFile path"
    );

    // A7: navigating to e1 rebuilds the set for that branch only.
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e1", "oldLeafId": "e2"}),
    );
    assert!(runtime.approval.is_approved(&identity_a.cache_key));
    assert!(
        !runtime.approval.is_approved(&identity_b.cache_key),
        "e2 grant is not on the e1 branch"
    );
    assert_eq!(runtime.approval.len(), 1);

    // A7: navigating to a null leaf clears the set.
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": null, "oldLeafId": "e1"}),
    );
    assert!(runtime.approval.is_empty());

    // Back to the e2 branch.
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": null}),
    );
    assert_eq!(runtime.approval.len(), 2);

    // A8: a session grant persists exactly one strict entry (names + hashes).
    let identity_c = get_tool_approval_identity("demo", &tool, &json!({"query": "c"}));
    assert!(runtime
        .approval
        .grant_session("demo", "search", &identity_c));
    // A cache hit does not re-emit.
    assert!(!runtime
        .approval
        .grant_session("demo", "search", &identity_c));

    let appended = host
        .append_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(appended.len(), 1, "one entry per new grant");
    assert_eq!(appended[0]["customType"], json!(MCP_APPROVAL_CUSTOM_TYPE));
    let data = &appended[0]["data"];
    assert_eq!(
        data,
        &json!({
            "version": 1,
            "kind": "tool",
            "decision": "allow_for_session",
            "serverName": "demo",
            "originalToolName": "search",
            "definitionHash": identity_c.definition_hash,
            "argsHash": identity_c.args_hash,
        })
    );
    // G4: no raw-argument field can appear in the persisted payload.
    assert!(data.get("args").is_none());
    assert!(data.get("query").is_none());

    // I-1 / TE-D41 (fallback limitation): in-memory sessions
    // (`--no-session`, `path: null`) have no JSONL to replay, so a restore
    // against an OLD host (no `ctx.sessionEntries`) clears the set
    // (fail-closed). Pinned here because upstream rebuilds from the
    // in-memory branch (`sessionManager.getBranch()`, index.ts:217-229
    // @ 928c30c); the ABI path below closes TE-D41 for V14-25+ hosts.
    *host.session_file.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let identity_d = get_tool_approval_identity("demo", &tool, &json!({"query": "d"}));
    assert!(runtime
        .approval
        .grant_session("demo", "search", &identity_d));
    assert_eq!(runtime.approval.len(), 4);
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e1", "oldLeafId": "e2"}),
    );
    assert!(
        runtime.approval.is_empty(),
        "in-memory restore is fail-closed on old hosts (TE-D41 fallback)"
    );

    // ---- TE33: ABI-first restore (`ctx.sessionEntries`, ADR-0027) ----

    let entry_value = |identity: &rpi_ext_mcp_adapter::session_approvals::ToolApprovalIdentity| {
        entry_to_value(
            &rpi_ext_mcp_adapter::session_approvals::SessionApprovalEntry::allow_for_session(
                "demo",
                "search",
                &identity.definition_hash,
                &identity.args_hash,
            ),
        )
    };

    // A2 (TE-D41 core close): the host upgrades to V14-25+ and answers the
    // active branch from memory. The session is STILL in-memory
    // (`session_file = None`), so the grants can only come from the ABI.
    let file_calls = host.session_file_calls.load(Ordering::SeqCst);
    let entries_calls = host.session_entries_calls.load(Ordering::SeqCst);
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        SessionEntriesReply::Entries(vec![custom_entry("e1", None, entry_value(&identity_a))]);
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e1", "oldLeafId": "e2"}),
    );
    assert!(
        runtime.approval.is_approved(&identity_a.cache_key),
        "A2: in-memory branch rebuilds from the ABI (TE-D41 closed)"
    );
    assert_eq!(runtime.approval.len(), 1);
    assert_eq!(
        host.session_file_calls.load(Ordering::SeqCst),
        file_calls,
        "ABI path: the JSONL fallback never runs"
    );
    assert!(
        host.session_entries_calls.load(Ordering::SeqCst) > entries_calls,
        "A2: the restore probed ctx.sessionEntries"
    );

    // A1/A9: with a file session restored, the set follows the HOST's
    // active branch, not the file tail — the file tip is still e2 (both
    // grants), but the host reports the e1 branch (one grant).
    let file_calls = host.session_file_calls.load(Ordering::SeqCst);
    *host.session_file.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(session_path.to_string_lossy().into_owned());
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert_eq!(
        runtime.approval.len(),
        1,
        "A9: host memory wins over the JSONL tail"
    );
    assert!(runtime.approval.is_approved(&identity_a.cache_key));
    assert!(!runtime.approval.is_approved(&identity_b.cache_key));
    assert_eq!(
        host.session_file_calls.load(Ordering::SeqCst),
        file_calls,
        "A1: the file session never falls back while the ABI serves"
    );

    // A5: an empty branch answers `[]` — a clear, not an error path.
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::Entries(Vec::new());
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e1", "oldLeafId": "e2"}),
    );
    assert!(
        runtime.approval.is_empty(),
        "A5: empty ABI branch clears without an error"
    );

    // A6: strict validation on the ABI path — foreign custom types and
    // smuggled payloads are skipped, valid grants survive.
    let mut bad_payload = entry_value(&identity_b);
    bad_payload["args"] = json!({"query": "b"});
    let mut foreign = custom_entry("e9", Some("e1"), entry_value(&identity_b));
    foreign["customType"] = json!("other-custom");
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::Entries(vec![
        custom_entry("e2", Some("e1"), entry_value(&identity_b)),
        custom_entry("e8", Some("e1"), bad_payload),
        foreign,
    ]);
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert_eq!(runtime.approval.len(), 1, "A6: invalid entries are skipped");
    assert!(runtime.approval.is_approved(&identity_b.cache_key));

    // Non-`unknownMethod` ABI failures are fail-closed clears — the JSONL
    // fallback must not run for them (the read is authoritative-or-nothing).
    let file_calls = host.session_file_calls.load(Ordering::SeqCst);
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::Error("unavailable");
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert!(
        runtime.approval.is_empty(),
        "an ABI error other than unknownMethod clears fail-closed"
    );
    assert_eq!(
        host.session_file_calls.load(Ordering::SeqCst),
        file_calls,
        "non-unknownMethod errors do not fall back to JSONL"
    );

    // A malformed reply (`ok: null`) is equally fail-closed.
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::NullOk;
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert!(runtime.approval.is_empty(), "malformed replies clear");

    // A8 (probe is not sticky, both directions): the same host downgrades
    // back to `unknownMethod` → the JSONL fallback serves again …
    let file_calls = host.session_file_calls.load(Ordering::SeqCst);
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::Unsupported;
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert_eq!(
        runtime.approval.len(),
        2,
        "A8: fallback re-engages per call"
    );
    assert!(
        host.session_file_calls.load(Ordering::SeqCst) > file_calls,
        "A8: the old-host JSONL path is read again"
    );
    // … and upgrades once more → the ABI serves on the very next restore
    // (nothing caches "host lacks ABI").
    let file_calls = host.session_file_calls.load(Ordering::SeqCst);
    let entries_calls = host.session_entries_calls.load(Ordering::SeqCst);
    *host
        .session_entries
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SessionEntriesReply::Entries(vec![
        custom_entry("e1", None, entry_value(&identity_a)),
        custom_entry("e2", Some("e1"), entry_value(&identity_b)),
    ]);
    dispatch_event(
        "session_tree",
        json!({"type": "session_tree", "newLeafId": "e2", "oldLeafId": "e1"}),
    );
    assert_eq!(runtime.approval.len(), 2, "A8: the ABI re-engages per call");
    assert!(host.session_entries_calls.load(Ordering::SeqCst) > entries_calls);
    assert_eq!(
        host.session_file_calls.load(Ordering::SeqCst),
        file_calls,
        "A8: no cached unsupported verdict survives"
    );

    std::env::remove_var("RPI_CODING_AGENT_DIR");
    match saved_agent_dir {
        Some(value) => std::env::set_var("RPI_CODING_AGENT_DIR", value),
        None => std::env::remove_var("RPI_CODING_AGENT_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
