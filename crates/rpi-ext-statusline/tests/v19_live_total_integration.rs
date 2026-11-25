//! V19-07 integration fixtures (ADR-0049 decisions 6–8): the two calculation
//! policies, the live-total overlay (pending / settled / current), the
//! terminal counts-only watermark probe, the unknown-watermark hide/recovery,
//! and the three mid-stream policy switches — all over the carrier seam with
//! a fake host that answers `ctx.sessionStats({countsOnly:true})` separately
//! from the full snapshot.
//!
//! Same harness shape as `items_defaults_integration.rs`; the scenarios run
//! sequentially inside one process-global install (the plugin keeps
//! process-global state, so one binary = one install).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

static RECORDS: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
static CANNED: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
/// Answer for `ctx.sessionStats` when called with `{countsOnly:true}`
/// (`None` → `{"ok":null}`: the probe fails).
static CANNED_COUNTS: OnceLock<Mutex<Option<Value>>> = OnceLock::new();

fn current_records() -> Vec<(String, Value)> {
    records()
}

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

fn set_canned_counts(ok: Option<Value>) {
    *CANNED_COUNTS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = ok;
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
    let ok = if method == "ctx.sessionStats" && args.get("countsOnly") == Some(&json!(true)) {
        CANNED_COUNTS
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or(Value::Null)
    } else {
        CANNED
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&method)
            .cloned()
            .unwrap_or(Value::Null)
    };
    RVec::from(serde_json::to_vec(&json!({"ok": ok})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec()))
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
            "timed out waiting for: {description}\nlast status: {:?}",
            status_texts(&current).last()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn agent_dir() -> std::path::PathBuf {
    std::env::var("RPI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .expect("RPI_CODING_AGENT_DIR set by the test wrapper")
}

fn write_status_line(status_line: Value) {
    let dir = agent_dir();
    std::fs::create_dir_all(&dir).expect("mkdir agent dir");
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({"statusLine": status_line})).expect("serialize"),
    )
    .expect("write settings");
}

fn items(live_tokens: Value) -> Value {
    json!({
        "type": "items",
        "items": ["live-total-tokens", "live-output-tokens", "live-output-rate"],
        "liveTokens": live_tokens,
        // 1 s interval: the non-boundary full snapshot read of the fixtures.
        "refreshInterval": 1,
        // Plain digits: the fixtures compare totals.
        "numberFormat": "exact",
    })
}

fn status_texts(records: &[(String, Value)]) -> Vec<String> {
    records
        .iter()
        .filter(|(method, _)| method == "ui.setStatus")
        .filter_map(|(_, args)| args.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn last_status(records: &[(String, Value)]) -> String {
    status_texts(records).last().cloned().unwrap_or_default()
}

fn counts_only(records: &[(String, Value)]) -> usize {
    counts_only_calls(records).len()
}

fn full_reads(records: &[(String, Value)]) -> usize {
    full_snapshot_reads(records)
}

fn counts_only_calls(records: &[(String, Value)]) -> Vec<Value> {
    records
        .iter()
        .filter(|(method, args)| {
            method == "ctx.sessionStats" && args.get("countsOnly") == Some(&json!(true))
        })
        .map(|(_, args)| args.clone())
        .collect()
}

fn full_snapshot_reads(records: &[(String, Value)]) -> usize {
    records
        .iter()
        .filter(|(method, args)| method == "ctx.sessionStats" && args.get("countsOnly").is_none())
        .count()
}

/// A consistent host snapshot: `assistant_messages` and the usage totals
/// move together (one `SessionManager` lock in a real host).
fn snapshot(input: u64, output: u64, assistant_messages: u64) -> Value {
    json!({
        "sessionId": "s1",
        "startedAt": "2020-01-01T00:00:00Z",
        "entryCount": 4 + assistant_messages,
        "userMessages": 2,
        "assistantMessages": assistant_messages,
        "tokens": {"input": input, "output": output, "cacheRead": 0, "cacheWrite": 0},
        "cost": 0.0
    })
}

/// The O(1) counts shape the host answers for `{countsOnly:true}`.
fn counts(assistant_messages: u64) -> Value {
    json!({
        "sessionId": "s1",
        "startedAt": "2020-01-01T00:00:00Z",
        "entryCount": 4 + assistant_messages,
        "userMessages": 2,
        "assistantMessages": assistant_messages
    })
}

fn install_fake_ctx() {
    set_canned("ctx.hasUI", json!(true));
    set_canned("ctx.cwd", json!("/tmp"));
    set_canned(
        "ctx.model",
        json!({"id": "glm-5.3", "name": "GLM", "contextWindow": 200_000}),
    );
    set_canned(
        "ctx.getContextUsage",
        json!({"tokens": 1_000, "contextWindow": 200_000, "percent": 0.5}),
    );
    set_canned("getThinkingLevel", json!("low"));
    set_canned("getSessionName", Value::Null);
    set_canned("ui.theme", Value::Null);
    set_canned("ctx.sessionStats", snapshot(50, 50, 3));
    set_canned_counts(Some(counts(3)));
}

fn start_message() {
    send_event(
        "message_start",
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    );
}

/// One text delta; `usage` = the provider's running total when `Some`.
fn delta(text: &str, usage: Option<u64>) {
    let mut message = json!({"role": "assistant", "content": []});
    if let Some(output) = usage {
        message["usage"] = json!({"output": output});
    }
    send_event(
        "message_update",
        json!({
            "type": "message_update",
            "message": message,
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": text},
        }),
    );
}

fn end_message(usage: Option<u64>, stop_reason: Option<&str>) {
    let mut message = json!({"role": "assistant"});
    if let Some(output) = usage {
        message["usage"] = json!({"input": 10, "output": output});
    }
    if let Some(reason) = stop_reason {
        message["stopReason"] = json!(reason);
    }
    send_event(
        "message_end",
        json!({"type": "message_end", "message": message}),
    );
}

/// `(live total, live output tokens)` of a frame, `~` stripped.
fn numbers(frame: &str) -> (u64, u64) {
    let mut parts = frame.split(" · ");
    let total = parts
        .next()
        .and_then(|part| part.trim_start_matches('~').parse::<u64>().ok())
        .expect("total");
    let tokens = parts
        .next()
        .and_then(|part| part.trim_start_matches('~').parse::<u64>().ok())
        .expect("tokens");
    (total, tokens)
}

trait PipeRef<T> {
    fn pipe_ref<R>(self, f: impl FnOnce(&T) -> R) -> R;
}

impl<T> PipeRef<T> for T {
    fn pipe_ref<R>(self, f: impl FnOnce(&T) -> R) -> R {
        f(&self)
    }
}

/// Wait for the first status frame published after `before` frames.
fn next_frame(before: usize, description: &str) -> String {
    let records = poll_until(description, |records| status_texts(records).len() > before);
    last_status(&records)
}

fn frames() -> usize {
    status_texts(&records()).len()
}

fn trigger(reason: &str) {
    send_event(reason, json!({"type": reason}));
}

#[test]
fn v19_live_total_policies_probe_and_switches() {
    let agent = std::env::temp_dir().join(format!(
        "rpi-statusline-v19-live-total-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&agent);
    std::fs::create_dir_all(&agent).expect("mkdir agent dir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);
    write_status_line(items(json!({"refreshMs": 300, "calculation": "estimate"})));
    install_fake_ctx();
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE,
    );
    assert!(receipt.get("error").is_none(), "install: {receipt:#?}");
    // Baseline snapshot: 100 tokens, 3 assistant messages.
    let baseline_records = poll_until("baseline frame", |records| {
        status_texts(records)
            .last()
            .is_some_and(|text| text.starts_with("100 · "))
    });
    let baseline = last_status(&baseline_records);
    assert_eq!(
        baseline, "100 · - · - tok/s",
        "idle placeholders: {baseline}"
    );
    assert!(
        counts_only_calls(&baseline_records).is_empty(),
        "no probe on refresh/interval reads"
    );

    // ── A. `estimate`: the streaming frames carry `~` on total, tokens and
    //        rate even though the provider reports a running exact total;
    //        no tokenizer is loaded (no exact counter) while streaming.
    start_message();
    delta("hello world, hello world, hello world", Some(300));
    let frame = next_frame(frames(), "estimate streaming frame");
    // Heuristic: 35 chars / 3.5 = 10 → ceil 11, marked on total and tokens.
    assert!(
        frame.starts_with("~111 · ~11 · "),
        "estimate marks every streaming value: {frame}"
    );
    assert!(
        !frame.contains("300"),
        "no provider exact while streaming: {frame}"
    );
    let before = frames();
    delta("hello world, hello world, hello world", Some(310));
    let frame = next_frame(before, "second estimate frame");
    assert!(
        frame.starts_with("~122 · ~22 · "),
        "still estimated: {frame}"
    );
    assert!(!frame.contains("310"), "{frame}");
    // R5.3.2: `estimate` streaming never loaded a tokenizer (the official
    // pass is allowed at message_end only).
    assert_eq!(
        rpi_ext_statusline::tokens::tokenizer_loads(),
        0,
        "estimate streaming: 0 loads"
    );
    // message_end: the authoritative exact value, no `~`, and the pending
    // exact total joins the live total (100 + 320).
    end_message(Some(320), None);
    let frame = next_frame(frames(), "estimate settled frame");
    assert!(
        frame.starts_with("420 · 320 · ") && !frame.contains('~'),
        "estimate settles exact: {frame}"
    );
    let probe_calls = counts_only_calls(&records());
    assert_eq!(probe_calls.len(), 1, "one terminal probe: {probe_calls:?}");

    // ── B. Watermark: a snapshot that does not count the message keeps the
    //        pending part; the covering snapshot drops it without double
    //        counting (100 → 150+320 = 470 with the message counted once).
    set_canned("ctx.sessionStats", snapshot(50, 50, 3));
    let before = frames();
    trigger("turn_end");
    let frame = next_frame(before, "uncovered snapshot");
    assert!(
        frame.starts_with("420 · "),
        "uncovered keeps pending 320: {frame}"
    );
    set_canned("ctx.sessionStats", snapshot(50, 370, 4));
    set_canned_counts(Some(counts(4)));
    let before = frames();
    trigger("turn_end");
    let frame = next_frame(before, "covering snapshot");
    assert!(
        frame.starts_with("420 · "),
        "covering snapshot: no double count: {frame}"
    );
    assert!(!frame.contains("740"), "{frame}");

    // ── C. A normally completed usage-less message settles LOCALLY: with a
    //        bundled tokenizer (GLM) as local exact (no `~`), kept across the
    //        next applied snapshot and the next message_start; no probe runs.
    write_status_line(items(json!({"refreshMs": 300})));
    // The 1 s poll picks the configuration change up (no restart).
    let _ = next_frame(frames(), "config change frame");
    start_message();
    delta(
        "The quick brown fox jumps over the lazy dog. 你好世界",
        None,
    );
    let frame = next_frame(frames(), "auto streaming frame (local exact)");
    let (total, local) = numbers(&frame);
    assert_eq!(
        total,
        420 + local,
        "auto counts locally with the bundled GLM tokenizer: {frame}"
    );
    assert!(local > 0);
    assert!(
        !frame.contains('~'),
        "the bundled tokenizer is exact (no `~`): {frame}"
    );
    end_message(None, None);
    let frame = next_frame(frames(), "usage-less settled frame");
    // Snapshot 420 + settled local exact (GLM count of the text) + 0 pending.
    let (total, _) = numbers(&frame);
    let settled_local = total - 420;
    assert_eq!(
        settled_local, local,
        "the official correction equals the incremental count: {frame}"
    );
    assert!(!frame.contains('~'), "local exact carries no `~`: {frame}");
    assert_eq!(
        counts_only_calls(&records()).len(),
        1,
        "usage-less: no probe"
    );
    // Across the next snapshot (unchanged counts) and the next message.
    let before = frames();
    trigger("turn_end");
    let frame = next_frame(before, "snapshot after local settlement");
    assert!(
        frame.starts_with(&format!("{}", 420 + settled_local)),
        "settled local survives the snapshot: {frame}"
    );
    start_message();
    delta("x", None);
    let frame = next_frame(frames(), "next message frame");
    assert!(
        frame
            .trim_start_matches('~')
            .starts_with(&format!("{}", 420 + settled_local + 1))
            || frame.starts_with(&format!("{}", 420 + settled_local + 1)),
        "settled local survives the next message_start: {frame}"
    );
    end_message(None, None);
    let _ = next_frame(frames(), "second usage-less settled");

    // ── D. Aborted usage-less message: the official tokenizer result still
    //        settles as local exact (no `~`); with the provider reporting
    //        nothing there is no estimate to drop and no probe.
    start_message();
    delta("aborted partial answer", None);
    end_message(None, Some("aborted"));
    let frame = next_frame(frames(), "aborted settled frame");
    let (after_abort, _) = numbers(&frame);
    assert!(
        after_abort > 420 + settled_local + 1,
        "local exact of an aborted message still settles: {frame}"
    );
    assert!(!frame.contains('~'), "{frame}");
    assert_eq!(counts_only_calls(&records()).len(), 1);
    let base_after_abort = after_abort;

    // ── E. Probe failure hides `live-total-tokens` until a boundary read
    //        issued AFTER the message covers it; a non-boundary read issued
    //        after does not recover it.
    set_canned_counts(None);
    start_message();
    delta("with usage", Some(5));
    end_message(Some(64), None);
    // The row is omitted while the watermark is unknown: the frame starts
    // with the live tokens row (64) instead of the session total.
    let hidden = poll_until("hidden live-total after a failed probe", |records| {
        let last = last_status(records);
        last.starts_with("64 · ") && !last.starts_with(&format!("{base_after_abort}"))
    });
    let hidden_text = last_status(&hidden);
    assert!(
        !hidden_text.starts_with(&format!("{}", base_after_abort)),
        "live-total-tokens is omitted while the watermark is unknown: {hidden_text}"
    );
    assert_eq!(
        counts_only_calls(&hidden).len(),
        2,
        "the failed probe still counts as the one probe"
    );
    // A non-boundary read issued after the message (the 1 s interval) does
    // not recover it, even though the snapshot now counts the message.
    set_canned("ctx.sessionStats", snapshot(50, 370 + 64, 6));
    let reads_before = full_snapshot_reads(&records());
    let hidden_records = poll_until("interval read while hidden", |records| {
        full_snapshot_reads(records) > reads_before
            && status_texts(records)
                .last()
                .is_some_and(|text| text.starts_with("64 · "))
    });
    assert!(
        !last_status(&hidden_records).starts_with(&format!("{base_after_abort}")),
        "still hidden after a non-boundary read: {:?}",
        last_status(&hidden_records)
    );
    // A boundary read issued after the message recovers it.
    let before = frames();
    trigger("turn_end");
    // The snapshot now holds 50 + 434 = 484; the recovered total is that
    // baseline plus the surviving settled local sums (484 + S, with S =
    // base_after_abort − 420).
    let recovered = poll_until("recovered live-total", |records| {
        status_texts(records).len() > before
            && last_status(records)
                .split(" · ")
                .next()
                .and_then(|first| first.parse::<u64>().ok())
                .is_some_and(|total| total >= 484)
    });
    assert_eq!(
        counts_only_calls(&recovered).len(),
        2,
        "no extra probe during recovery"
    );
    // From here the baseline is the covering snapshot: 470 + 64 = 534? The
    // snapshot (370 output + 50 input = 420 + 64 = 484) — the pending 64
    // moved into the snapshot, settled local stays.
    let (baseline, _) = numbers(&last_status(&recovered));
    // The probe works again from here on.
    set_canned_counts(Some(counts(6)));

    // ── F. Mid-stream policy switches (CountReset once each; pending and
    //        settled overlay preserved).
    rpi_ext_statusline::reset_ticker_for_test();
    // F1: auto (exact BPE state) → estimate: the next refresh rebuilds the
    //     Han/other buckets only — every value turns `~`.
    start_message();
    delta("auto first ", None);
    let frame = next_frame(frames(), "auto streaming before the switch");
    let (total, local) = numbers(&frame);
    assert_eq!(
        total,
        baseline + local,
        "auto counts locally (GLM exact): {frame}"
    );
    assert!(local > 0 && !frame.contains('~'), "{frame}");
    let before = frames();
    write_status_line(items(json!({"refreshMs": 300, "calculation": "estimate"})));
    delta("auto second ", None);
    let frame = next_frame(before, "estimate frame after the switch");
    assert!(
        frame.contains('~'),
        "estimate applies on the next refresh: {frame}"
    );
    assert_eq!(rpi_ext_statusline::ticker_stats().count_resets, 1);
    // F2: estimate → auto while the provider reports a running exact total:
    //     adopted directly (no `~`; no tier row selected ⇒ no BPE needed).
    delta(" ", Some(900));
    let loads_before = rpi_ext_statusline::tokens::tokenizer_loads();
    let before = frames();
    write_status_line(items(json!({"refreshMs": 300, "calculation": "auto"})));
    let frame = next_frame(before, "auto frame after the switch");
    let (total, tokens) = numbers(&frame);
    assert_eq!(tokens, 900, "provider exact adopted directly: {frame}");
    assert_eq!(
        rpi_ext_statusline::tokens::tokenizer_loads(),
        loads_before,
        "a provider-satisfied auto frame never loads the tokenizer"
    );
    assert_eq!(total, baseline + 900, "{frame}");
    assert!(!frame.contains('~'), "{frame}");
    assert_eq!(rpi_ext_statusline::ticker_stats().count_resets, 2);
    end_message(Some(905), None);
    let frame = next_frame(frames(), "settled after the switches");
    let (total, tokens) = numbers(&frame);
    assert_eq!((total, tokens), (baseline + 905, 905), "{frame}");
    // The pending 905 survived the switches: a covering snapshot (assistant
    // count past the watermark) replaces it without a double count.
    set_canned("ctx.sessionStats", snapshot(50, 434 + 905, 7));
    set_canned_counts(Some(counts(7)));
    let before = frames();
    trigger("turn_end");
    let frame = next_frame(before, "covering snapshot after the switches");
    let (total, _) = numbers(&frame);
    assert_eq!(
        total,
        baseline + 905,
        "pending cleared by the watermark: {frame}"
    );
    // F3: estimate → auto WITHOUT a provider total: the local counter is
    //     rebuilt once from the shared full text (GLM exact, no `~`).
    write_status_line(items(json!({"refreshMs": 300, "calculation": "estimate"})));
    // The poll applies the configuration; a settled interval frame may still
    // show the previous message, so wait for the new message's own frame.
    std::thread::sleep(Duration::from_millis(1200));
    start_message();
    delta("rebuild me exactly", None);
    let frame = poll_until("estimate streaming", |records| {
        last_status(records).contains('~') && last_status(records).contains(" tok/s")
    })
    .pipe_ref(|records| last_status(records));
    assert!(frame.contains('~'), "{frame}");
    let before = frames();
    write_status_line(items(json!({"refreshMs": 300, "calculation": "auto"})));
    delta("!", None);
    let frame = poll_until("auto frame after rebuild", |records| {
        status_texts(records).len() > before
            && last_status(records).contains(" tok/s")
            && !last_status(records).contains('~')
            // The GLM count of "rebuild me exactly!" differs from the
            // heuristic the estimate frames showed.
            && numbers(&last_status(records)).1 != 6
    })
    .pipe_ref(|records| last_status(records));
    let (total, local) = numbers(&frame);
    assert_eq!(
        total,
        baseline + 905 + local,
        "local exact after the rebuild: {frame}"
    );
    assert!(local > 0, "{frame}");
    assert_eq!(rpi_ext_statusline::ticker_stats().count_resets, 3);
    assert_eq!(rpi_ext_statusline::ticker_stats().count_fallbacks, 0);
    assert_eq!(rpi_ext_statusline::ticker_stats().finalize_mismatches, 0);
    end_message(None, None);
    let frame = next_frame(frames(), "final settled");
    let (total, _) = numbers(&frame);
    assert_eq!(
        total,
        baseline + 905 + local,
        "the local settlement keeps the total: {frame}"
    );
    assert!(!frame.contains('~'), "local exact, no marker: {frame}");
    assert_eq!(rpi_ext_statusline::ticker_stats().finalize_mismatches, 0);

    // ── G. Command mode never probes: with liveTokens on, a finished
    //        message with usage triggers zero counts-only calls.
    let probes_before = counts_only(&current_records());
    write_status_line(json!({
        "type": "command",
        "command": "echo hi",
        "placement": "status",
        "liveTokens": {"refreshMs": 300},
    }));
    let _ = next_frame(frames(), "config change to command mode");
    start_message();
    delta("command mode", Some(3));
    end_message(Some(33), None);
    let before = frames();
    trigger("message_end");
    std::thread::sleep(Duration::from_millis(700));
    let _ = before;
    assert_eq!(
        counts_only(&current_records()),
        probes_before,
        "command mode creates no pending and runs no probe"
    );
    assert!(full_reads(&current_records()) >= 4);

    // ── H. Review P1-1 regression: the SECOND estimate message of a
    //        session counts from its own first byte — its streaming value is
    //        its full-text heuristic (never the previous message's offsets).
    write_status_line(items(json!({"refreshMs": 300, "calculation": "estimate"})));
    std::thread::sleep(Duration::from_millis(1200));
    // Let the poll apply the configuration and publish an idle items frame.
    let idle = poll_until("items idle frame after command mode", |records| {
        let last = last_status(records);
        last.contains(" · ")
            && last
                .split(" · ")
                .next()
                .is_some_and(|f| f.parse::<u64>().is_ok())
    });
    let second = "second estimate message, with its own bytes";
    let expected = heuristic(second);
    start_message();
    delta(&second[..20], None);
    let frame = poll_until("second estimate message frame", |records| {
        status_texts(records).len() > status_texts(&idle).len()
            && last_status(records).starts_with('~')
            && last_status(records).contains(" tok/s")
    });
    let (first_total, first_tokens) = numbers(&last_status(&frame));
    assert_eq!(
        first_tokens,
        heuristic(&second[..20]),
        "{:?}",
        last_status(&frame)
    );
    // Second delta: the estimate is the full-text heuristic and the total
    // grew by exactly the difference (no head lost, nothing double counted).
    let before = frames();
    delta(&second[20..], None);
    let frame = next_frame(before, "second estimate message second frame");
    let (total, tokens) = numbers(&frame);
    assert_eq!(
        tokens, expected,
        "the second message's estimate is its own heuristic: {frame}"
    );
    assert_eq!(
        total - tokens,
        first_total - first_tokens,
        "the session part of the total is unchanged between the two ticks: {frame}"
    );
    // The third message, shorter than the second, also counts from zero.
    end_message(None, None);
    let _ = next_frame(frames(), "second estimate message settled");
    start_message();
    delta("tiny", None);
    // "tiny" = 4 chars → ceil(4 / 3.5) = 2.
    let frame = poll_until("third estimate message frame", |records| {
        last_status(records).starts_with('~') && last_status(records).contains("~2 · ")
    });
    assert_eq!(
        numbers(&last_status(&frame)).1,
        heuristic("tiny"),
        "the third message starts from byte 0: {}",
        last_status(&frame)
    );
    end_message(None, None);
    let _ = next_frame(frames(), "third estimate message settled");
    let _ = std::fs::remove_dir_all(agent_dir());
}

/// The heuristic the `estimate` frames must show for a text.
fn heuristic(text: &str) -> u64 {
    let han = text
        .chars()
        .filter(|ch| rpi_ext_statusline::tokens::is_han_text(*ch))
        .count() as u64;
    let other = text.chars().count() as u64 - han;
    ((han as f64 / 1.2 + other as f64 / 3.5).ceil()) as u64
}
