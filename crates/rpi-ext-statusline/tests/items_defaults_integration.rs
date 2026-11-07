//! Fresh-install items preset over the L0 carrier seam.
//!
//! A separate test binary (own process, own `OnceLock` install) so the
//! scenario can start with the shared `ItemsConfig::default()` preset and
//! verify the shipped default end to end: `total-tokens` (the session's
//! absolute token total from `ctx.sessionStats`) plus `live-output-rate`
//! (the live tok/s placeholder while idle), with `liveTokens` on. The tiered
//! live-token behavior lives in `items_integration.rs`, which pins an
//! explicit live pair.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

static RECORDS: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
static CANNED: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();

fn records() -> Vec<(String, Value)> {
    RECORDS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn set_canned(method: &str, ok: Value) {
    CANNED
        .get_or_init(|| Mutex::new(HashMap::new()))
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
        .push((method.clone(), args.clone()));
    match method.as_str() {
        // - `on` answers a subscriptionId; - everything else uses CANNED.
        "on" => {
            let id = RECORDS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len() as u64;
            return RVec::from(
                serde_json::to_vec(&json!({"ok": {"subscriptionId": id}})).unwrap_or_default(),
            );
        }
        "off" => return RVec::from(b"{\"ok\":null}".to_vec()),
        _ => {}
    }
    let ok = CANNED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&method)
        .cloned()
        .unwrap_or(Value::Null);
    RVec::from(serde_json::to_vec(&json!({"ok": ok})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec()))
}

fn session_stats(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
    json!({
        "sessionId": "s1",
        "startedAt": "2020-01-01T00:00:00Z",
        "entryCount": 4,
        "userMessages": 2,
        "assistantMessages": 3,
        "tokens": {
            "input": input,
            "output": output,
            "cacheRead": cache_read,
            "cacheWrite": cache_write
        },
        "cost": 0.0123
    })
}

fn install_fake_ctx() {
    set_canned("ctx.hasUI", json!(true));
    set_canned("ctx.cwd", json!("/tmp/project"));
    set_canned(
        "ctx.model",
        json!({"id": "test-model", "name": "Test Model", "contextWindow": 200_000}),
    );
    set_canned(
        "ctx.getContextUsage",
        json!({"tokens": 12_345, "contextWindow": 200_000, "percent": 6.2}),
    );
    set_canned("getThinkingLevel", json!("high"));
    set_canned("getSessionName", json!("te52"));
    set_canned("getMode", json!("plan"));
    set_canned("ctx.isIdle", json!(true));
    set_canned("ctx.sessionFile", json!({"path": null, "id": "s1"}));
    // 10 + 20 + 30 + 40 = 100 session tokens in the default preset.
    set_canned("ctx.sessionStats", session_stats(10, 20, 30, 40));
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
            start.elapsed() < Duration::from_secs(10),
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

fn subscribed(records: &[(String, Value)]) -> Vec<String> {
    records
        .iter()
        .filter(|(method, _)| method == "on")
        .map(|(_, args)| {
            args.get("event")
                .and_then(Value::as_str)
                .expect("event name")
                .to_owned()
        })
        .collect()
}

fn status_texts(records: &[(String, Value)]) -> Vec<String> {
    calls_of(records, "ui.setStatus")
        .iter()
        .filter_map(|args| {
            (args.get("key").and_then(Value::as_str) == Some("rpi-statusline"))
                .then(|| args.get("text").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        })
        .collect()
}

fn last_status_contains(records: &[(String, Value)], needle: &str) -> bool {
    status_texts(records)
        .last()
        .is_some_and(|text| text.contains(needle))
}

#[test]
fn fresh_install_preset_is_session_total_tokens_plus_live_rate() {
    let agent =
        std::env::temp_dir().join(format!("rpi-statusline-defaults-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);

    // No explicit items: the install uses the shared default preset.
    write_settings(json!({"statusLine": {"type": "items"}}));
    install_fake_ctx();
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE,
    );
    assert!(receipt.get("error").is_none(), "install: {receipt:#?}");

    // ── 1. Field-driven subscriptions at install: the base lifecycle, the
    //        session-stats triggers for `total-tokens`, and the live pair
    //        for `live-output-rate`; nothing else is collected.
    let at_install = subscribed(&records());
    for event in [
        "session_start",
        "session_info_changed",
        "model_select",
        "thinking_level_select",
        "session_shutdown",
        "turn_end",
        "agent_settled",
        "session_compact",
        "message_start",
        "message_update",
        "message_end",
    ] {
        assert!(at_install.iter().any(|s| s == event), "missing {event}");
    }
    for event in ["tool_execution_end", "mode_change", "agent_start"] {
        assert!(
            !at_install.iter().any(|s| s == event),
            "the default preset must not subscribe {event}"
        );
    }

    // ── 2. The idle frame: the absolute session total (input + output +
    //        cacheRead + cacheWrite = 100) followed by the live-rate dash
    //        placeholder, on the shared status line (never a widget).
    let idle = poll_until("default session total + rate placeholder", |records| {
        last_status_contains(records, "100") && last_status_contains(records, "- tok/s")
    });
    assert_eq!(
        status_texts(&idle).last().map(String::as_str),
        Some("100 · - tok/s"),
        "default preset frame"
    );
    assert!(
        calls_of(&idle, "ui.setWidget").is_empty(),
        "status placement never mounts a widget"
    );

    // ── 3. `turn_end` re-reads the absolute snapshot: the total row follows
    //        the host's new token counts without any transcript parsing.
    set_canned("ctx.sessionStats", session_stats(20, 20, 30, 50));
    send_event("turn_end", json!({"type": "turn_end"}));
    let refreshed = poll_until("session total after turn_end", |records| {
        last_status_contains(records, "120")
    });
    let text = status_texts(&refreshed).last().cloned().unwrap_or_default();
    assert!(
        text.starts_with("120 · "),
        "the total row is the new snapshot: {text:?}"
    );

    // ── 4. Live behavior still applies to the rate row: a stream produces a
    //        tok/s value next to the unchanged absolute total.
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    send_event(
        "message_update",
        json!({
            "type": "message_update",
            "message": {"role": "assistant"},
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": "hello world"
            },
        }),
    );
    std::thread::sleep(Duration::from_millis(650));
    let streaming = poll_until("default rate row while streaming", |records| {
        status_texts(records).last().is_some_and(|text| {
            text.starts_with("120 · ") && text.contains("tok/s") && !text.contains("- tok/s")
        })
    });
    let text = status_texts(&streaming).last().cloned().unwrap_or_default();
    assert!(
        text.starts_with("120 · "),
        "the absolute total stays put while the rate streams: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&agent);
}
