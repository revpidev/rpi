//! TE52 R22 joint frames: the real rpi-statusline items renderer composed
//! with the TE50 compact usage footer shape on the host's status row.
//!
//! The test drives the actual plugin install/dispatch path against a fake
//! host, captures the `ui.setStatus`/`ui.setWidget`/`ui.setFooter` payloads,
//! then composes the two plugin keys in the host's frozen order: sorted by
//! key, whitespace-folded, joined with a ` · ` separator and wrapped to the
//! terminal width by `rpi_tui::utils::wrap_text_with_ansi` — the exact
//! helper the built-in footer uses. 40/80/120 columns and the three
//! placements are covered.
//!
//! Boundary: TE52 establishes the fixture, the host row composition and the
//! statusline side. Linking `rpi-ext-usage`'s abi_stable root module into
//! this binary conflicts with this plugin's exported module-loader symbol,
//! so the usage side reads the exact typed-footer golden strings from
//! `crates/rpi-ext-usage/fixtures/joint_footer_golden.json` (TE53). The usage
//! plugin's own `tests/joint_frames.rs` runs the real plugin against that
//! same fixture and pins the strings with the real formatter, the xAI
//! cached-only auto path and the zero-second-request assertion.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_tui::utils::{visible_width, wrap_text_with_ansi};
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
        .push((method.clone(), args));
    match method.as_str() {
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
        "off" | "registerCommand" => return RVec::from(b"{\"ok\":null}".to_vec()),
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
    set_canned(
        "ctx.sessionStats",
        json!({
            "sessionId": "s1",
            "startedAt": "2020-01-01T00:00:00Z",
            "entryCount": 2,
            "tokens": {"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40},
            "cost": 0.0123
        }),
    );
}

fn send_event(event: &str, payload: Value) {
    let message = json!({"kind": "event", "event": event, "payload": payload});
    rpi_ext_statusline::dispatch(
        std::ptr::null(),
        RVec::from(serde_json::to_vec(&message).expect("serialize event")),
    );
}

fn write_settings(value: Value) {
    let dir = std::env::var("RPI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .expect("RPI_CODING_AGENT_DIR");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&value).expect("serialize"),
    )
    .expect("write");
}

fn calls_of<'a>(records: &'a [(String, Value)], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|(recorded, _)| recorded == method)
        .map(|(_, args)| args)
        .collect()
}

fn wait_until(description: &str, predicate: impl Fn(&[(String, Value)]) -> bool) {
    let start = Instant::now();
    loop {
        let current = records();
        if predicate(&current) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The host footer's `sanitizeStatusText` (footer.ts:13-19): fold CR/LF/TAB/
/// spaces to single spaces and trim.
fn sanitize_status_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_space = false;
    for ch in text.chars() {
        if matches!(ch, '\r' | '\n' | '\t' | ' ') {
            if !previous_space {
                out.push(' ');
            }
            previous_space = true;
        } else {
            previous_space = false;
            out.push(ch);
        }
    }
    out.trim().to_owned()
}

/// Compose the extension status row exactly like the built-in footer.
fn compose_status_row(statusline: &str, usage: &str) -> String {
    let mut statuses = BTreeMap::new();
    statuses.insert("rpi-statusline", statusline);
    statuses.insert("rpi-usage", usage);
    statuses
        .values()
        .map(|text| sanitize_status_text(text))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The shared TE53 typed-footer fixture (real formatter output pinned by the
/// usage plugin's own joint frame).
fn joint_golden() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rpi-ext-usage/fixtures/joint_footer_golden.json");
    serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
    .expect("joint golden json")
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.clone().next() == Some('[') {
            // Consume the CSI introducer and parameters up to the final byte.
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[test]
fn statusline_and_usage_compose_on_the_host_status_row() {
    let agent = std::env::temp_dir().join(format!("rpi-statusline-joint-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);

    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model", "context-used", "used-tokens"],
        "placement": "status",
        "useColors": false
    }}));
    install_fake_ctx();
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        std::ptr::null(),
    );
    assert!(receipt.get("error").is_none(), "{receipt:#?}");

    // ── statusline side: the real plugin render → ui.setStatus.
    wait_until("statusline status text", |records| {
        calls_of(records, "ui.setStatus").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text").and_then(Value::as_str).is_some()
        })
    });
    let snapshot = records();
    let statusline = calls_of(&snapshot, "ui.setStatus")
        .iter()
        .filter_map(|args| {
            (args.get("key").and_then(Value::as_str) == Some("rpi-statusline"))
                .then(|| args.get("text").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        })
        .next_back()
        .expect("statusline status text");
    assert_eq!(statusline, "Test Model · Context 6% used · 70");

    // ── usage side: the typed footer from the shared TE53 golden fixture
    //        (`windows[]` → `ledgers[]` → `balance[]`). The real usage plugin
    //        pins these same strings in its own joint frame.
    let golden = joint_golden();
    let usage = golden["used"].as_str().expect("used golden").to_owned();

    let row = compose_status_row(&statusline, &usage);
    assert_eq!(
        row,
        format!("Test Model · Context 6% used · 70 · {usage}"),
        "sorted keys: rpi-statusline before rpi-usage, dot separator"
    );

    // ── 40/80/120 columns: wrap, never truncate; every visible char stays.
    // used/remaining/static variants cover the typed footer modes.
    for key in ["used", "remaining", "static"] {
        let usage = golden[key].as_str().expect("golden variant").to_owned();
        let row = compose_status_row(&statusline, &usage);
        let visible_row = strip_ansi(&row);
        for width in [40usize, 80, 120] {
            let lines = wrap_text_with_ansi(&row, width);
            for line in &lines {
                assert!(
                    visible_width(line) <= width,
                    "{key} line exceeds {width}: {line:?}"
                );
            }
            let rejoined = lines
                .iter()
                .map(|line| strip_ansi(line))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                sanitize_status_text(&rejoined),
                sanitize_status_text(&visible_row),
                "{key} width {width} preserves the whole row (no truncation)"
            );
            if width == 40 {
                assert!(lines.len() > 1, "{key}: 40 columns wrap the joint row");
            }
        }
    }

    // ── widget: the statusline owns belowEditor; usage keeps the status row.
    let widget_text = "Test Model · Context 6% used · 70";
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model", "context-used", "used-tokens"],
        "placement": "widget",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    wait_until("statusline widget", |records| {
        calls_of(records, "ui.setWidget").iter().any(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args
                    .pointer("/content/children/0/props/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text == widget_text)
        })
    });
    let snapshot = records();
    let clears = calls_of(&snapshot, "ui.setStatus")
        .into_iter()
        .filter(|args| {
            args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text") == Some(&Value::Null)
        })
        .count();
    // The status→widget switch clears the old key exactly once; after the
    // widget frame the status row belongs to rpi-usage.
    assert_eq!(clears, 1, "one clear on the status→widget switch");
    let widget_index = snapshot
        .iter()
        .rposition(|(method, _)| method == "ui.setWidget")
        .expect("widget frame");
    let status_after = snapshot[widget_index..]
        .iter()
        .filter(|(method, args)| {
            method == "ui.setStatus"
                && args.get("key").and_then(Value::as_str) == Some("rpi-statusline")
                && args.get("text").and_then(Value::as_str).is_some()
        })
        .count();
    assert_eq!(status_after, 0, "widget mode publishes no status text");
    // The usage key is never touched by statusline (call graph isolation).
    assert!(
        calls_of(&snapshot, "ui.setStatus")
            .iter()
            .all(|args| args.get("key").and_then(Value::as_str) != Some("rpi-usage")),
        "statusline never writes the usage key"
    );
    assert!(
        !records()
            .iter()
            .any(|(method, _)| method.starts_with("ctx.usage.")),
        "statusline never calls ctx.usage.* (usage requests stay in its own worker)"
    );

    // ── replace: the statusline takes the footer over (usage's status row is
    // hidden by the host while replace is active).
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model", "context-used", "used-tokens"],
        "placement": "replace",
        "useColors": false
    }}));
    send_event("model_select", json!({"type": "model_select"}));
    wait_until("statusline footer takeover", |records| {
        calls_of(records, "ui.setFooter").iter().any(|args| {
            args.pointer("/component/children/0/props/text")
                .and_then(Value::as_str)
                == Some(widget_text)
        })
    });
    let snapshot = records();
    assert!(
        calls_of(&snapshot, "ui.setWidget")
            .iter()
            .any(|args| args.get("content") == Some(&Value::Null)),
        "replace clears the widget first"
    );

    send_event("session_shutdown", json!({"type": "session_shutdown"}));
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_dir_all(&agent).ok();
}
