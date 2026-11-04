//! TE52 items-mode integration over the L0 carrier seam: real `install`,
//! real `dispatch`, a fake host that records every host call.
//!
//! A separate test binary (own process, own `OnceLock` install) from
//! `statusline_integration.rs`, so the scenario can start with an items
//! configuration at install time and exercise the field-driven subscription
//! set, `ctx.sessionStats`, live reconciliation and the three placements.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

static RECORDS: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
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
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&method)
        .cloned()
        .unwrap_or(Value::Null);
    RVec::from(serde_json::to_vec(&json!({"ok": ok})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec()))
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
    set_canned(
        "ctx.sessionStats",
        json!({
            "sessionId": "s1",
            "startedAt": "2020-01-01T00:00:00Z",
            "entryCount": 4,
            "tokens": {"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40},
            "cost": 0.0123
        }),
    );
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

fn widget_texts(records: &[(String, Value)]) -> Vec<String> {
    calls_of(records, "ui.setWidget")
        .iter()
        .filter_map(|args| {
            args.pointer("/content/children")
                .and_then(Value::as_array)
                .map(|children| {
                    children
                        .iter()
                        .filter_map(|child| {
                            child
                                .pointer("/props/text")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        })
        .collect()
}

fn last_widget_contains(records: &[(String, Value)], needle: &str) -> bool {
    widget_texts(records)
        .last()
        .is_some_and(|text| text.contains(needle))
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
fn items_mode_lifecycle_over_the_carrier_seam() {
    let agent = std::env::temp_dir().join(format!("rpi-statusline-items-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);

    // Default install: realtime token statistics only (live-output-tokens +
    // live-output-rate, liveTokens on); every other field is off.
    write_settings(json!({"statusLine": {"type": "items"}}));
    install_fake_ctx();
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE,
    );
    assert!(receipt.get("error").is_none(), "install: {receipt:#?}");

    // ── 1. Field-driven subscription set at install: base lifecycle plus the
    //        live message events; no session totals, Git, mode or run-state.
    let at_install = subscribed(&records());
    for event in [
        "session_start",
        "session_info_changed",
        "model_select",
        "thinking_level_select",
        "session_shutdown",
        "message_start",
        "message_update",
        "message_end",
    ] {
        assert!(at_install.iter().any(|s| s == event), "missing {event}");
    }
    for event in [
        "tool_execution_end",
        "turn_end",
        "session_compact",
        "mode_change",
        "agent_start",
        "agent_settled",
    ] {
        assert!(
            !at_install.iter().any(|s| s == event),
            "default items must not subscribe {event}"
        );
    }

    // ── 2. Default frame: idle keeps both live rows as dash placeholders so
    //        the status line never reflows when a stream starts; a stream
    //        then shows the estimated token/rate pair live, and the exact
    //        provider count on message_end. No shell, no command.
    let idle = poll_until("idle live placeholders", |records| {
        status_texts(records)
            .last()
            .is_some_and(|text| text == "- · - tok/s")
    });
    assert!(
        calls_of(&idle, "ui.setWidget").is_empty(),
        "status placement never mounts a widget"
    );
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
    let now = poll_until("default estimated token rows", |records| {
        last_status_contains(records, "~") && last_status_contains(records, "tok/s")
    });
    let text = status_texts(&now).last().cloned().unwrap_or_default();
    assert!(text.contains('~'), "estimate marker: {text:?}");
    assert!(text.contains("tok/s"), "live rate: {text:?}");
    assert!(
        calls_of(&now, "ui.setWidget").is_empty(),
        "status placement never mounts a widget"
    );
    send_event(
        "message_end",
        json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"output": 42}},
        }),
    );
    let now = poll_until("default exact token rows", |records| {
        last_status_contains(records, "42") && last_status_contains(records, "tok/s")
    });
    let text = status_texts(&now).last().cloned().unwrap_or_default();
    assert!(
        !text.contains('~'),
        "exact count drops the marker: {text:?}"
    );

    // ── 2b. Between two assistant messages the live rows hold the last
    //        numbers: the next `message_start` has no delta yet, and the
    //        footer must not flip to the dash placeholders (which would
    //        visibly jump while the model thinks).
    let exact_text = text.clone();
    let frames_before = status_texts(&records()).len();
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    // Wait for the frame this event actually publishes (the event path
    // debounces, so sleeping a fixed amount races the refresh loop), then
    // assert it still carries the previous message's numbers.
    let gap = poll_until("message_start frame", |records| {
        status_texts(records).len() > frames_before
    });
    let gap_text = status_texts(&gap).last().cloned().unwrap_or_default();
    assert_eq!(
        gap_text, exact_text,
        "the gap before the next delta must hold the previous live numbers"
    );
    assert!(
        !gap_text.contains("- tok/s"),
        "the gap must not fall back to the dash placeholders: {gap_text:?}"
    );
    // The first delta of the new message refreshes the rows.
    send_event(
        "message_update",
        json!({
            "type": "message_update",
            "message": {"role": "assistant"},
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": "second message body"
            },
        }),
    );
    let now = poll_until("second message refreshes the rows", |records| {
        status_texts(records)
            .last()
            .is_some_and(|text| text != &exact_text && text.contains("tok/s"))
    });
    assert!(
        status_texts(&now)
            .last()
            .is_some_and(|text| text.contains('~')),
        "the second message re-estimates: {:?}",
        status_texts(&now).last()
    );
    send_event(
        "message_end",
        json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"output": 43}},
        }),
    );
    poll_until("second exact token rows", |records| {
        last_status_contains(records, "43") && last_status_contains(records, "tok/s")
    });
    assert!(
        !records()
            .iter()
            .any(|(method, _)| method == "ctx.usage.fetch"
                || method == "ctx.usage.cached"
                || method == "ctx.usage.fetchDetailed"
                || method == "ctx.usage.listProviders"),
        "statusline must never call ctx.usage.*"
    );

    // ── 2c. A SHORT reply (decode window below the streaming 0.5 s guard)
    //        settles with its OWN frozen rate instead of holding the previous
    //        message's number, and the settled frame never reuses old rows.
    let long_text = status_texts(&records()).last().cloned().unwrap_or_default();
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    send_event(
        "message_update",
        json!({
            "type": "message_update",
            "message": {"role": "assistant", "usage": {"output": 1}},
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": "hi"
            },
        }),
    );
    std::thread::sleep(Duration::from_millis(200));
    send_event(
        "message_end",
        json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"output": 1}},
        }),
    );
    let now = poll_until("short settled frame with its own rate", |records| {
        status_texts(records).last().is_some_and(|text| {
            text.starts_with("1 · ") && text.contains("tok/s") && !text.contains("- tok/s")
        })
    });
    let short_text = status_texts(&now).last().cloned().unwrap_or_default();
    assert_ne!(
        short_text, long_text,
        "the settled frame must not reuse the previous message's rows"
    );
    assert!(
        short_text
            .split(" · ")
            .nth(1)
            .is_some_and(|rate| rate != long_text.split(" · ").nth(1).unwrap_or_default()),
        "the short message refreshes its rate: {short_text:?} (previous {long_text:?})"
    );

    // ── 3. Field selection reconciles the extra subscriptions and renders
    //        the strict 26-field values (aliases + unknown id skipped).
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": [
            "model-name",
            "thread-name",
            "context-used",
            "used-tokens",
            "total-input-tokens",
            "cache-read-tokens",
            "cache-write-tokens",
            "session-cost",
            "hostname"
        ],
        "placement": "widget",
        "useColors": false
    }}));
    send_event("turn_end", json!({"type": "turn_end"}));
    let now = poll_until("rich items widget", |records| {
        last_widget_contains(records, "Test Model") && last_widget_contains(records, "70")
    });
    let text = widget_texts(&now).last().cloned().unwrap_or_default();
    assert!(text.contains("Test Model"), "{text:?}");
    assert!(text.contains("te52"), "{text:?}");
    assert!(text.contains("Context 6% used"), "{text:?}");
    assert!(text.contains("70"), "used = 10 + 40 + 20: {text:?}");
    assert!(
        text.contains("80 in"),
        "input + cacheRead + cacheWrite: {text:?}"
    );
    assert!(text.contains("30 cache-read"), "{text:?}");
    assert!(text.contains("40 cache-write"), "{text:?}");
    assert!(text.contains("$0.0123"), "local cost estimate: {text:?}");
    assert!(
        !text.contains("hostname"),
        "unknown ids are never rendered: {text:?}"
    );
    let after = subscribed(&now);
    for event in ["turn_end", "agent_settled"] {
        assert!(
            after.iter().any(|s| s == event),
            "session totals add {event}"
        );
    }
    assert!(
        !after.iter().any(|s| s == "mode_change"),
        "permission-mode not selected"
    );
    assert!(
        !after.iter().any(|s| s == "tool_execution_end"),
        "Git fields not selected"
    );

    // ── 4. Live fields + liveTokens: message events are added dynamically;
    //        streaming output prefers provider usage and falls back to the
    //        built-in token estimate.
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["streaming", "live-output-tokens"],
        "liveTokens": {"refreshMs": 300},
        "placement": "widget",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    poll_until("live subscriptions after config save", |records| {
        let events = subscribed(records);
        ["message_start", "message_update", "message_end"]
            .iter()
            .all(|event| events.iter().any(|seen| seen == event))
    });
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    send_event(
        "message_update",
        json!({
            "type": "message_update",
            "message": {"role": "assistant", "usage": {"output": 25}},
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "hello"},
        }),
    );
    let now = poll_until("live widget", |records| {
        last_widget_contains(records, "25") && last_widget_contains(records, "streaming")
    });
    let text = widget_texts(&now).last().cloned().unwrap_or_default();
    assert_eq!(text, "streaming · 25");
    send_event(
        "message_end",
        json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"output": 40}},
        }),
    );
    // After the stream settles the exact provider count wins over the last
    // streaming running total (R20 §4.3: 结束后 exact).
    poll_until("post-stream exact live tokens", |records| {
        last_widget_contains(records, "40")
    });
    assert!(
        !widget_texts(&records())
            .last()
            .is_some_and(|text| text.contains("25")),
        "the settled frame uses usage.output from message_end: {:?}",
        widget_texts(&records()).last()
    );

    // ── 4b. No provider usage while streaming: the built-in estimate makes
    //        the token/rate rows update live instead of waiting for
    //        message_end. The tier split is visible too.
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": [
            "streaming",
            "live-output-tokens",
            "live-output-rate",
            "live-text-tokens"
        ],
        "liveTokens": {"refreshMs": 300},
        "placement": "widget",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    poll_until("estimated live fields subscribed", |records| {
        let events = subscribed(records);
        ["message_start", "message_update", "message_end"]
            .iter()
            .all(|event| events.iter().any(|seen| seen == event))
    });
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
    // Give the 300ms ticker a full rate window (>500ms) and wait for the
    // estimate-marked rows.
    std::thread::sleep(Duration::from_millis(650));
    let now = poll_until("estimated live widget", |records| {
        last_widget_contains(records, "~") && last_widget_contains(records, "tok/s")
    });
    let text = widget_texts(&now).last().cloned().unwrap_or_default();
    assert!(text.contains("streaming"), "{text:?}");
    assert!(text.contains("~"), "estimate marker: {text:?}");
    assert!(text.contains("tok/s"), "live rate: {text:?}");
    assert!(text.contains("text"), "tier row: {text:?}");
    assert!(
        !text.contains("25") && !text.contains("40"),
        "no provider usage was reported for this message: {text:?}"
    );
    send_event(
        "message_end",
        json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"output": 0}},
        }),
    );
    // The frozen estimate remains visible after the stream settles (0 usage
    // is the provider's aborted/no-usage signal and never becomes "exact 0").
    poll_until("frozen estimate after message_end", |records| {
        last_widget_contains(records, "~") && last_widget_contains(records, "text")
    });

    // ── 5. Turning live off stops the events and clears the measurement.
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model"],
        "liveTokens": false,
        "placement": "widget",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    let now = poll_until("live subscriptions removed", |records| {
        let offs = calls_of(records, "off").len();
        offs >= 3 && last_widget_contains(records, "Test Model")
    });
    let final_events = subscribed(&now);
    // The `on` list is historical; the off calls prove removal. A late live
    // tick may repaint the new (model-only) selection but must never show
    // stale live values.
    std::thread::sleep(Duration::from_millis(1200));
    let after = widget_texts(&records());
    assert_eq!(
        after.last().map(String::as_str),
        Some("Test Model"),
        "the settled frame is the model-only selection: {after:?}"
    );
    assert!(
        !after
            .last()
            .is_some_and(|text| text.contains("streaming") || text.contains("25")),
        "live values are gone after the switch: {after:?}"
    );
    assert!(!final_events.is_empty());

    // ── 6. Empty items: the widget key is cleared (no blank footer).
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": [],
        "placement": "widget"
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    poll_until("empty items clears the widget", |records| {
        calls_of(records, "ui.setWidget").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("content") == Some(&Value::Null)
        })
    });

    // ── 7. Placement switch: widget → status → replace, each clearing the
    //        previous channel first; replace takes the footer over.
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model"],
        "placement": "status",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    let now = poll_until("status placement", |records| {
        calls_of(records, "ui.setStatus")
            .iter()
            .any(|args| args.get("text").and_then(Value::as_str) == Some("Test Model"))
    });
    assert!(
        calls_of(&now, "ui.setWidget").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("content") == Some(&Value::Null)
        }),
        "status switch clears the widget first"
    );
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model"],
        "placement": "replace",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    let now = poll_until("replace placement", |records| {
        calls_of(records, "ui.setFooter").iter().any(|args| {
            args.pointer("/component/children/0/props/text")
                .and_then(Value::as_str)
                == Some("Test Model")
        })
    });
    assert!(
        calls_of(&now, "ui.setStatus").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text") == Some(&Value::Null)
        }),
        "replace switch clears the status row first"
    );

    // ── 7b. Theme changes repaint: the items fingerprint includes
    //        `ui.theme`, so a palette change with no other data movement
    //        still re-renders (useColors=true).
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model"],
        "placement": "widget",
        "useColors": true,
        // Long interval: the theme repaint must come from the fingerprint,
        // not the periodic interval tick.
        "refreshInterval": 60
    }}));
    set_canned(
        "ui.theme",
        json!({"appearance": "dark", "colors": {"accent": "#ff0000"}}),
    );
    send_event("model_select", json!({"type": "model_select"}));
    poll_until("red accent theme", |records| {
        last_widget_contains(records, "\u{1b}[38;2;255;0;0mTest Model")
    });
    // Let the next poll settle on the red fingerprint before switching.
    std::thread::sleep(Duration::from_millis(1500));
    set_canned(
        "ui.theme",
        json!({"appearance": "dark", "colors": {"accent": "#00ff00"}}),
    );
    poll_until("green accent after the theme change", |records| {
        last_widget_contains(records, "\u{1b}[38;2;0;255;0mTest Model")
    });

    // ── 8. Config removal restores the built-in footer through the owned
    //        channel.
    write_settings(json!({}));
    send_event("model_select", json!({"type": "model_select"}));
    poll_until("config removal restores the footer", |records| {
        calls_of(records, "ui.setFooter")
            .iter()
            .any(|args| args.get("component") == Some(&Value::Null))
    });

    // ── 9. No shell ever started for items: no ui.* script traffic beyond
    //        the channels above, and no command with the retained command
    //        string (there was none). The editor command is registered once.
    let commands = records()
        .iter()
        .filter(|(method, _)| method == "registerCommand")
        .count();
    assert_eq!(commands, 1, "exactly one /statusline registration");

    send_event("session_shutdown", json!({"type": "session_shutdown"}));
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_dir_all(&agent).ok();
}
