//! Integration tests over the L0 carrier seam: install against a fake
//! host (an `extern "C"` call recorder), drive events through the real
//! `dispatch`, and assert the host-call traffic (mcp-adapter
//! `install_for_test` precedent).
//!
//! All scenarios share one process-global plugin install (the state is a
//! `OnceLock`), so they run sequentially inside a single test — the
//! subagents e2e style. `RPI_CODING_AGENT_DIR` points at a temp dir whose
//! settings.json each scenario rewrites; the refresh loop re-reads it on
//! every tick, so config edits apply without reinstall.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{json, Value};

/// Recorded host calls: `(method, args)` in order.
static RECORDS: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();

/// Canned `{"ok": ...}` payloads per method (missing → `null`).
static CANNED: OnceLock<Mutex<std::collections::HashMap<String, Value>>> = OnceLock::new();

fn records() -> Vec<(String, Value)> {
    RECORDS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn set_canned(method: &str, ok: Value) {
    CANNED
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(method.to_owned(), ok);
}

extern "C" fn fake_host_call(_cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    let message: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = message
        .get("call")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    RECORDS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push((method.clone(), args));
    let ok = CANNED
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&method)
        .cloned()
        .unwrap_or(Value::Null);
    RVec::from(serde_json::to_vec(&json!({"ok": ok})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec()))
}

/// Default session facts the fake host serves.
fn install_fake_ctx() {
    set_canned("ctx.hasUI", json!(true));
    set_canned("ctx.cwd", json!("/tmp"));
    set_canned(
        "ctx.model",
        json!({"id": "test-model", "name": "Test Model", "contextWindow": 200_000}),
    );
    set_canned(
        "ctx.getContextUsage",
        json!({"tokens": 12_345, "contextWindow": 200_000, "percent": 6.2}),
    );
    set_canned("getThinkingLevel", json!("high"));
    set_canned("getSessionName", Value::Null);
}

const COOKIE: PluginCookie = std::ptr::null();

fn send_event(event: &str, payload: Value) {
    let message = json!({"kind": "event", "event": event, "payload": payload});
    rpi_ext_statusline::dispatch(
        COOKIE,
        RVec::from(serde_json::to_vec(&message).expect("serialize event")),
    );
}

fn poll_until(
    description: &str,
    predicate: impl Fn(&[(String, Value)]) -> bool,
) -> Vec<(String, Value)> {
    let start = Instant::now();
    loop {
        let current = records();
        if predicate(&current) {
            return current;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "timed out waiting for: {description}\nrecords: {current:#?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_settings(value: Value) {
    let dir = agent_dir();
    std::fs::create_dir_all(&dir).expect("mkdir agent dir");
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&value).expect("serialize settings"),
    )
    .expect("write settings");
}

fn agent_dir() -> std::path::PathBuf {
    std::env::var("RPI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .expect("RPI_CODING_AGENT_DIR set by the test wrapper")
}

fn calls_of<'a>(records: &'a [(String, Value)], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|(recorded, _)| recorded == method)
        .map(|(_, args)| args)
        .collect()
}

#[test]
fn statusline_lifecycle_over_the_carrier_seam() {
    // Temp agent dir so settings.json is fully test-owned.
    let agent = std::env::temp_dir().join(format!("rpi-statusline-it-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent);
    write_settings(json!({}));

    install_fake_ctx();
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let receipt = rpi_ext_statusline::install_for_test(calls, COOKIE);
    assert!(receipt.get("error").is_none(), "install: {receipt:#?}");

    // ── 1. Subscription set (the TE12 event table). ────────────────────
    let subscribed: Vec<String> = records()
        .iter()
        .filter(|(method, _)| method == "on")
        .map(|(_, args)| {
            args.get("event")
                .and_then(Value::as_str)
                .expect("event name")
                .to_owned()
        })
        .collect();
    for event in [
        "message_end",
        "session_start",
        "session_compact",
        "session_info_changed",
        "model_select",
        "thinking_level_select",
        "tool_execution_end",
        "session_shutdown",
    ] {
        assert!(
            subscribed.iter().any(|s| s == event),
            "missing subscription: {event}"
        );
    }

    // ── 2. No statusLine key → no UI traffic, no script. ───────────────
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 100, "output": 20, "cacheRead": 1000, "cacheWrite": 0,
                      "cost": {"total": 0.01}},
        }}),
    );
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        calls_of(&records(), "ui.setFooter").is_empty()
            && calls_of(&records(), "ui.setStatus").is_empty(),
        "no config → no UI pushes"
    );

    // ── 3. replace placement: echo script renders the footer, stdin
    //        carries the CC contract. ────────────────────────────────────
    let stdin_capture = agent.join("last-stdin.json");
    let _ = std::fs::remove_file(&stdin_capture);
    write_settings(json!({"statusLine": {
        "type": "command",
        "command": format!("cat > {}; echo render-ok", stdin_capture.display()),
    }}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 200, "output": 40, "cacheRead": 2000, "cacheWrite": 100,
                      "cost": {"total": 0.02}},
        }}),
    );
    let now = poll_until("ui.setFooter with render-ok", |records| {
        calls_of(records, "ui.setFooter").iter().any(|args| {
            args.pointer("/component/children/0/props/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("render-ok"))
        })
    });
    let footer_calls = calls_of(&now, "ui.setFooter");
    let footer_args = footer_calls.last().expect("setFooter recorded");
    assert_eq!(
        footer_args
            .pointer("/component/type")
            .and_then(Value::as_str),
        Some("column")
    );
    assert_eq!(
        footer_args.pointer("/component/children/0/props/truncate"),
        Some(&json!(true))
    );
    assert!(footer_args
        .pointer("/component/children/0/props")
        .expect("props")
        .get("fg")
        .is_none());
    // stdin payload: CC field names, plugin-accumulated values.
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_capture).expect("captured stdin"))
            .expect("stdin json");
    assert_eq!(stdin["hook_event_name"], "Status");
    assert_eq!(stdin["model"]["display_name"], "Test Model");
    assert_eq!(stdin["cwd"], "/tmp");
    assert_eq!(stdin["effort"]["level"], "high");
    assert_eq!(stdin["context_window"]["total_input_tokens"], 12_345);
    assert_eq!(
        stdin["cost"]["total_cost_usd"], 0.03,
        "accumulated 0.01+0.02"
    );
    assert_eq!(
        stdin["context_window"]["current_usage"]["input_tokens"], 200,
        "last message usage"
    );
    assert_eq!(stdin["rpi"]["session_totals"]["input"], 300);

    // ── 4. Switch to status placement: old channel restored first. ────
    write_settings(json!({"statusLine": {
        "type": "command",
        "command": "echo status-mode",
        "placement": "status",
    }}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                      "cost": {"total": 0.0}},
        }}),
    );
    let now = poll_until("ui.setStatus with status-mode", |records| {
        calls_of(records, "ui.setStatus").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text").and_then(Value::as_str) == Some("status-mode")
        })
    });
    // The replace channel was restored (component: null) during the switch.
    assert!(
        calls_of(&now, "ui.setFooter")
            .iter()
            .any(|args| args.get("component") == Some(&Value::Null)),
        "placement switch restores the built-in footer"
    );

    // ── 5. Config removed → status entry cleared. ──────────────────────
    write_settings(json!({}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 5, "output": 5, "cacheRead": 0, "cacheWrite": 0,
                      "cost": {"total": 0.0}},
        }}),
    );
    poll_until("ui.setStatus cleared", |records| {
        calls_of(records, "ui.setStatus").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text") == Some(&Value::Null)
        })
    });

    // ── 5b. Widget placement: full ComponentTree below the editor, the
    //        built-in footer (and other plugins' status rows) untouched;
    //        config removal clears the widget. ──────────────────────────
    let footer_calls_before_widget = calls_of(&records(), "ui.setFooter").len();
    write_settings(json!({"statusLine": {
        "type": "command",
        "command": "echo widget-line-1; echo widget-line-2",
        "placement": "widget",
    }}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 7, "output": 7, "cacheRead": 0, "cacheWrite": 0,
                      "cost": {"total": 0.0}},
        }}),
    );
    let now = poll_until("ui.setWidget with the two-line tree", |records| {
        calls_of(records, "ui.setWidget").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("placement").and_then(Value::as_str) == Some("belowEditor")
                && args.pointer("/content/type").and_then(Value::as_str) == Some("column")
                && args.pointer("/content/children/0/props/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("widget-line-1"))
                && args.pointer("/content/children/1/props/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("widget-line-2"))
        })
    });
    // The switch from the status channel cleared the old status entry.
    assert!(
        calls_of(&now, "ui.setStatus")
            .iter()
            .any(|args| args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text") == Some(&Value::Null)),
        "widget switch clears the old status entry"
    );
    // No footer takeover in widget mode (no new setFooter calls at all —
    // the status→widget switch restores through setStatus, not setFooter).
    assert_eq!(
        calls_of(&now, "ui.setFooter").len(),
        footer_calls_before_widget,
        "widget mode must not touch the footer channel"
    );
    write_settings(json!({}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 8, "output": 8, "cacheRead": 0, "cacheWrite": 0,
                      "cost": {"total": 0.0}},
        }}),
    );
    poll_until("ui.setWidget cleared", |records| {
        calls_of(records, "ui.setWidget").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("content") == Some(&Value::Null)
        })
    });

    // ── 6. Failing script keeps the last render (no new pushes). ───────
    write_settings(json!({"statusLine": {
        "type": "command",
        "command": "echo should-fail; exit 7",
        "placement": "status",
    }}));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant",
            "usage": {"input": 9, "output": 9, "cacheRead": 0, "cacheWrite": 0,
                      "cost": {"total": 0.0}},
        }}),
    );
    std::thread::sleep(Duration::from_millis(900));
    let pushes_after_failure = calls_of(&records(), "ui.setStatus")
        .into_iter()
        .filter(|args| args.get("text").is_some_and(|t| !t.is_null()))
        .count();
    assert_eq!(
        pushes_after_failure, 1,
        "failing script must not push a new render"
    );

    // ── 7. Shutdown stops the loop without panicking. ──────────────────
    send_event("session_shutdown", json!({"type": "session_shutdown"}));
    std::thread::sleep(Duration::from_millis(300));

    std::fs::remove_dir_all(&agent).ok();
}
