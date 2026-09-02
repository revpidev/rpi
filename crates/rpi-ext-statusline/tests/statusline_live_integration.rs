//! Live-token integration tests (03-realtime-token-count §2.7 场景 ⑧–⑬ /
//! A3 / A8): a SECOND carrier-seam binary whose install happens with
//! `statusLine.liveTokens` configured — the subscription set is fixed at
//! load (§1.5 启用时机注记), so the unconfigured set (⑨, the TE12 eight)
//! is pinned by the sibling `statusline_integration.rs` binary instead.
//!
//! Same fake-host harness as `statusline_integration.rs`; the scenarios
//! run sequentially inside one process-global install.

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

fn install_fake_ctx() {
    set_canned("ctx.hasUI", json!(true));
    set_canned("ctx.cwd", json!("/tmp"));
    set_canned(
        "ctx.model",
        json!({"id": "test-model", "name": "Test Model", "contextWindow": 200_000}),
    );
    set_canned(
        "ctx.getContextUsage",
        json!({"tokens": 1_000, "contextWindow": 200_000, "percent": 0.5}),
    );
    set_canned("getThinkingLevel", json!("low"));
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
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for: {description}\nrecords: {current:#?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn agent_dir() -> std::path::PathBuf {
    std::env::var("RPI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .expect("RPI_CODING_AGENT_DIR set by the test wrapper")
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

/// One streaming assistant message: start + N deltas + end with exact
/// usage (chars: text 4/delta, thinking 6/delta, toolcall 5/delta).
fn stream_message(label: &str, deltas: usize, exact_output: Option<u64>) {
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    for i in 0..deltas {
        let payload = json!({
            "type": "message_update",
            "message": {"role": "assistant", "content": []},
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0,
                                      "delta": format!("{label}-{i}-xxxx")},
        });
        send_event("message_update", payload);
        std::thread::sleep(Duration::from_millis(40));
    }
    let mut message = json!({"role": "assistant"});
    if let Some(output) = exact_output {
        message["usage"] = json!({"input": 50, "output": output});
    }
    send_event(
        "message_end",
        json!({"type": "message_end", "message": message}),
    );
}

/// Parsed per-run stdin payloads from the capture log (compact JSON, one
/// per line, each run appends one line).
fn captured_stdins(log: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("captured stdin json"))
        .collect()
}

#[test]
fn live_tokens_lifecycle_over_the_carrier_seam() {
    let agent = std::env::temp_dir().join(format!("rpi-statusline-live-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent);

    // liveTokens configured BEFORE install (load-time evaluation).
    let stdin_log = agent.join("stdin.log");
    let runs_log = agent.join("runs.log");
    write_settings(json!({"statusLine": {
        "type": "command",
        // Each run appends its stdin JSON (newline-terminated) + a
        // monotonic timestamp.
        "command": format!(
            "cat >> {}; echo >> {}; date +%s%N >> {}; echo live-ok",
            stdin_log.display(), stdin_log.display(), runs_log.display(),
        ),
        "liveTokens": {"refreshMs": 300},
    }}));

    install_fake_ctx();
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let receipt = rpi_ext_statusline::install_for_test(calls, COOKIE);
    assert!(receipt.get("error").is_none(), "install: {receipt:#?}");

    // ── ⑧ With liveTokens configured the subscription set grows by
    //      message_start + message_update (10 total). ─────────────────────
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
    assert_eq!(subscribed.len(), 10, "subscribed: {subscribed:?}");
    for event in ["message_start", "message_update"] {
        assert!(subscribed.iter().any(|s| s == event), "missing {event}");
    }

    // ── ⑩ Streaming: spawn throttle + live_output measurement. ──────────
    let runs_before = captured_stdins(&stdin_log).len();
    let run_timestamps_before = std::fs::read_to_string(&runs_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(runs_before, run_timestamps_before);

    let streaming_start = Instant::now();
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
    for i in 0..20 {
        send_event(
            "message_update",
            json!({
                "type": "message_update",
                "message": {"role": "assistant", "content": []},
                "assistantMessageEvent": {
                    "type": if i % 2 == 0 { "text_delta" } else { "thinking_delta" },
                    "contentIndex": 0,
                    "delta": "0123456789",
                },
            }),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // ~1s of streaming at refreshMs=300 → at least a couple of live runs,
    // and never more than the throttle allows (~1s/300ms + slack).
    let streamed_secs = streaming_start.elapsed().as_secs_f64();
    let mid_stdins = captured_stdins(&stdin_log);
    let live_runs = mid_stdins.len() - runs_before;
    assert!(
        live_runs >= 2,
        "live ticks must fire during ~1s of streaming (got {live_runs})"
    );
    let max_runs = (streamed_secs / 0.3).ceil() as usize + 2;
    assert!(
        live_runs <= max_runs,
        "spawn rate must respect refreshMs: {live_runs} runs in {streamed_secs:.2}s"
    );

    // Inter-run spacing ≥ refreshMs (minus scheduling slack) among the
    // live-tick runs (A4: 流式期间脚本 spawn ≤ 1/refreshMs).
    let timestamps: Vec<u64> = std::fs::read_to_string(&runs_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().expect("nanos"))
        .collect();
    let new_timestamps = &timestamps[run_timestamps_before..];
    for pair in new_timestamps.windows(2) {
        let gap_ms = (pair[1] - pair[0]) / 1_000_000;
        assert!(
            gap_ms >= 250,
            "live runs closer than refreshMs-50ms: {gap_ms}ms"
        );
    }

    // The live payloads carry the measurements (A3): 10 text deltas and
    // 10 thinking deltas of 10 chars each (category split, FR-A).
    let with_live: Vec<&Value> = mid_stdins
        .iter()
        .filter(|payload| payload.pointer("/rpi/live_output").is_some())
        .collect();
    assert!(!with_live.is_empty(), "live_output present while streaming");
    for payload in &with_live {
        let live = payload.pointer("/rpi/live_output").expect("live block");
        assert_eq!(live["streaming"], true);
        let text: u64 = live["text_chars"].as_u64().unwrap_or(0);
        let thinking: u64 = live["thinking_chars"].as_u64().unwrap_or(0);
        assert_eq!(text % 10, 0, "whole deltas counted (text={text})");
        assert_eq!(
            thinking % 10,
            0,
            "whole deltas counted (thinking={thinking})"
        );
        assert!(text + thinking > 0, "chars accumulated");
        assert_eq!(
            live.pointer("/hook_event_name"),
            None,
            "hook_event_name stays top-level"
        );
        // FR-F: live-tick runs carry the real event name.
        assert_eq!(payload["hook_event_name"], "message_update");
    }
    let chars_of = |payload: &Value| -> u64 {
        payload
            .pointer("/rpi/live_output/text_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + payload
                .pointer("/rpi/live_output/thinking_chars")
                .and_then(Value::as_u64)
                .unwrap_or(0)
    };
    assert!(
        chars_of(with_live.last().unwrap()) > chars_of(with_live[0]),
        "measurements grow across ticks"
    );

    // ── ⑫ message_end: exact tokens land, streaming freezes. ────────────
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {
            "role": "assistant", "usage": {"input": 50, "output": 123},
        }}),
    );
    let settled = poll_until("message_end run with exact tokens", |records| {
        let _ = records;
        captured_stdins(&stdin_log).iter().any(|payload| {
            payload.pointer("/rpi/live_output/output_tokens_exact") == Some(&json!(123))
        })
    });
    let _ = settled;
    let stdins = captured_stdins(&stdin_log);
    let final_live = stdins
        .iter()
        .rev()
        .find(|payload| payload.pointer("/rpi/live_output").is_some())
        .expect("final live payload")
        .pointer("/rpi/live_output")
        .unwrap()
        .clone();
    assert_eq!(final_live["streaming"], false);
    assert_eq!(final_live["output_tokens_exact"], 123);
    assert_eq!(final_live["text_chars"], 100);
    assert_eq!(final_live["thinking_chars"], 100);
    // Regular (non-live) runs keep the CC hook name (A4).
    assert_eq!(stdins.last().unwrap()["hook_event_name"], "Status");

    // Frozen fingerprint: after the ONE self-heal re-render the poll
    // performs once the stream settles (frozen values changed), no
    // further script runs may happen (⑪ idle quiet).
    let settled_count = stdins.len();
    std::thread::sleep(Duration::from_millis(1300));
    let after_self_heal = captured_stdins(&stdin_log).len();
    assert!(
        after_self_heal <= settled_count + 1,
        "at most the single post-settle self-heal re-render ({} → {})",
        settled_count,
        after_self_heal
    );
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        captured_stdins(&stdin_log).len(),
        after_self_heal,
        "frozen live fingerprint must not re-spawn (⑪ stall/idle quiet)"
    );

    // ── ⑪ A new short stream that stalls: delta window measures the gap,
    //      idle re-runs report delta_chars = 0. ──────────────────────────
    stream_message("second", 2, Some(7));
    poll_until("second message settled", |records| {
        let _ = records;
        captured_stdins(&stdin_log).iter().any(|payload| {
            payload.pointer("/rpi/live_output/output_tokens_exact") == Some(&json!(7))
        })
    });
    let stdins = captured_stdins(&stdin_log);
    let second_live = stdins
        .iter()
        .rev()
        .find(|payload| payload.pointer("/rpi/live_output").is_some())
        .expect("second live payload")
        .pointer("/rpi/live_output")
        .unwrap()
        .clone();
    assert_eq!(second_live["text_chars"], 2 * 13, "second-stream chars");
    assert_eq!(second_live["output_tokens_exact"], 7);

    // ── A8: ctx.sessionFile is authoritative — an mtime-newer sibling
    //      file in the heuristic directory must NOT win. ─────────────────
    let mine =
        agent.join("mine-2026-08-20T10-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl");
    std::fs::write(&mine, b"{}").expect("write mine");
    let sibling =
        agent.join("sibling-2026-08-20T11-00-00-000_99999999-8888-7777-6666-555555555555.jsonl");
    std::fs::write(&sibling, b"{}").expect("write sibling");
    // Make the sibling strictly newer (mtime race winner under the old
    // heuristic).
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&sibling, b"{}").expect("touch sibling");
    set_canned(
        "ctx.sessionFile",
        json!({"path": mine.display().to_string(), "id": "018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e"}),
    );
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {"role": "assistant", "usage": {"output": 9}}}),
    );
    poll_until("A8 authoritative transcript path", |records| {
        let _ = records;
        captured_stdins(&stdin_log).iter().any(|payload| {
            payload.get("transcript_path") == Some(&json!(mine.display().to_string()))
        })
    });
    let stdins = captured_stdins(&stdin_log);
    let authoritative = stdins
        .iter()
        .rev()
        .find(|payload| payload.get("transcript_path") == Some(&json!(mine.display().to_string())))
        .expect("run with the authoritative path");
    assert_eq!(
        authoritative["session_id"],
        "018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e"
    );
    assert_ne!(
        authoritative["transcript_path"],
        json!(sibling.display().to_string()),
        "the mtime-newer sibling must not win (A8)"
    );

    // ── In-memory session (path: null): transcript_path omitted,
    //      session_id still present (FR-I). ──────────────────────────────
    set_canned(
        "ctx.sessionFile",
        json!({"path": null, "id": "memory-id-1"}),
    );
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {"role": "assistant", "usage": {"output": 3}}}),
    );
    poll_until("in-memory identity lands", |records| {
        let _ = records;
        captured_stdins(&stdin_log)
            .iter()
            .any(|payload| payload.get("session_id") == Some(&json!("memory-id-1")))
    });
    let stdins = captured_stdins(&stdin_log);
    let memory_run = stdins
        .iter()
        .rev()
        .find(|payload| payload.get("session_id") == Some(&json!("memory-id-1")))
        .expect("in-memory run");
    assert!(memory_run.get("transcript_path").is_none());

    // ── Zero/missing usage keeps output_tokens_exact null (FR-C). ───────
    stream_message("third", 1, None);
    poll_until("third message settled without exact tokens", |records| {
        let _ = records;
        captured_stdins(&stdin_log).iter().any(|payload| {
            payload.pointer("/rpi/live_output/text_chars") == Some(&json!(12))
                && payload
                    .pointer("/rpi/live_output/output_tokens_exact")
                    .is_some_and(|exact| exact.is_null())
        })
    });

    // ── session_start resets the live measurements (FR-D). ─────────────
    send_event(
        "session_start",
        json!({"type": "session_start", "reason": "new"}),
    );
    std::thread::sleep(Duration::from_millis(400));
    send_event(
        "message_end",
        json!({"type": "message_end", "message": {"role": "assistant", "usage": {"output": 1}}}),
    );
    poll_until("post-reset run omits live_output", |records| {
        let _ = records;
        let stdins = captured_stdins(&stdin_log);
        stdins
            .last()
            .is_some_and(|payload| payload.pointer("/rpi/live_output").is_none())
    });

    // ── ⑦ Shutdown stops the loop. ──────────────────────────────────────
    send_event("session_shutdown", json!({"type": "session_shutdown"}));
    std::thread::sleep(Duration::from_millis(300));

    std::fs::remove_dir_all(&agent).ok();
}
