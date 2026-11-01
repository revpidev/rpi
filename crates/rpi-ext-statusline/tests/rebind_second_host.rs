//! `/resume` session-replacement regression (a separate binary: the
//! statusline state is a process-global `OnceLock`; this test exercises the
//! second-host rebind path on top of it).
//!
//! Before the fix: a session replacement (`/resume` etc.) rebuilds the
//! `NativeExtensionHost` and reloads the same dlopen-memoized cdylib — install
//! returned `{"ok": true}` early:
//! 1. the fresh host had **zero event subscriptions** (the early return ran
//!    before `on`), so every statusline event (message_end / model_select …)
//!    stopped refreshing the footer after `/resume`;
//! 2. the refresh loop held host 1's channel and its poll timer kept pushing
//!    through the old cookie — a dangling pointer once that host was dropped;
//! 3. the old loop had already exited on `session_shutdown` and nothing
//!    restarted it.
//!
//! After the fix: the second install takes the rebind path — re-subscribes the
//! events, swaps the CHANNEL and restarts the refresh loop. This test asserts
//! that B receives the full subscription set, event-driven footer refreshes
//! resume on B, and A pushes nothing after the rebind.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

/// Two fake hosts, switched by the cookie VALUE (0xA / 0xB — never
/// dereferenced). Each has its own record buffer + canned replies.
#[derive(Default)]
struct HostSide {
    records: Vec<(String, Value)>,
    canned: HashMap<String, Value>,
}

static HOST_A: Mutex<Option<HostSide>> = Mutex::new(None);
static HOST_B: Mutex<Option<HostSide>> = Mutex::new(None);

const COOKIE_A: PluginCookie = 0xA as PluginCookie;
const COOKIE_B: PluginCookie = 0xB as PluginCookie;

fn with_side<R>(cookie: PluginCookie, f: impl FnOnce(&mut HostSide) -> R) -> R {
    let side = if cookie == COOKIE_A { &HOST_A } else { &HOST_B };
    let mut guard = side.lock().unwrap_or_else(|error| error.into_inner());
    f(guard.get_or_insert_with(HostSide::default))
}

extern "C" fn fake_host_call(cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    let message: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = message
        .get("call")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    let ok = with_side(cookie, |side| {
        side.records.push((method.clone(), args));
        side.canned.get(&method).cloned().unwrap_or(Value::Null)
    });
    RVec::from(serde_json::to_vec(&json!({"ok": ok})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec()))
}

fn set_canned(cookie: PluginCookie, method: &str, ok: Value) {
    with_side(cookie, |side| {
        side.canned.insert(method.to_owned(), ok);
    });
}

fn records(cookie: PluginCookie) -> Vec<(String, Value)> {
    with_side(cookie, |side| side.records.clone())
}

fn install_fake_ctx(cookie: PluginCookie) {
    set_canned(cookie, "ctx.hasUI", json!(true));
    set_canned(cookie, "ctx.cwd", json!("/tmp"));
    set_canned(
        cookie,
        "ctx.model",
        json!({"id": "test-model", "name": "Test Model", "contextWindow": 200_000}),
    );
    set_canned(
        cookie,
        "ctx.getContextUsage",
        json!({"tokens": 12_345, "contextWindow": 200_000, "percent": 6.2}),
    );
    set_canned(cookie, "getThinkingLevel", json!("high"));
    set_canned(cookie, "getSessionName", Value::Null);
}

fn send_event(event: &str, payload: Value) {
    let message = json!({"kind": "event", "event": event, "payload": payload});
    rpi_ext_statusline::dispatch(
        COOKIE_B,
        RVec::from(serde_json::to_vec(&message).expect("serialize event")),
    );
}

fn poll_until(
    description: &str,
    predicate: impl Fn(&[(String, Value)]) -> bool,
) -> Vec<(String, Value)> {
    let start = Instant::now();
    loop {
        let current = records(COOKIE_B);
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

fn footer_render_ok(records: &[(String, Value)]) -> usize {
    records
        .iter()
        .filter(|(method, _)| method == "ui.setFooter")
        .filter(|(_, args)| {
            args.pointer("/component/children/0/props/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("rebind-ok"))
        })
        .count()
}

#[test]
fn resume_rebinds_second_host_and_revives_the_refresh_loop() {
    // Temp agent dir so settings.json is fully test-owned; a statusLine
    // command whose stdout marks the footer.
    let agent = std::env::temp_dir().join(format!(
        "rpi-statusline-rebind-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&agent).expect("mkdir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&json!({"statusLine": {
            "type": "command",
            "command": "echo rebind-ok",
        }}))
        .expect("serialize settings"),
    )
    .expect("write settings");

    // ── 宿主 A：进程启动 ─────────────────────────────────────────────
    install_fake_ctx(COOKIE_A);
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE_A,
    );
    assert!(receipt.get("error").is_none(), "install A: {receipt:#?}");

    // 事件驱动刷新：message_end → 脚本 → footer。
    let message_end = json!({"type": "message_end", "message": {
        "role": "assistant",
        "usage": {"input": 100, "output": 20, "cacheRead": 1000, "cacheWrite": 0,
                  "cost": {"total": 0.01}},
    }});
    rpi_ext_statusline::dispatch(
        COOKIE_A,
        RVec::from(
            serde_json::to_vec(&json!({
                "kind": "event", "event": "message_end", "payload": message_end
            }))
            .expect("serialize event"),
        ),
    );
    let start = Instant::now();
    while footer_render_ok(&records(COOKIE_A)) == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "host A must render the footer: {:#?}",
            records(COOKIE_A)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let a_footer_count = records(COOKIE_A)
        .iter()
        .filter(|(method, _)| method == "ui.setFooter")
        .count();

    // ── /resume teardown：session_shutdown → 旧循环退出 ───────────────
    rpi_ext_statusline::dispatch(
        COOKIE_A,
        RVec::from(
            serde_json::to_vec(&json!({
                "kind": "event", "event": "session_shutdown", "payload": {}
            }))
            .expect("serialize event"),
        ),
    );
    std::thread::sleep(Duration::from_millis(100));

    // ── 宿主 B：重载同一插件（修复点） ────────────────────────────────
    install_fake_ctx(COOKIE_B);
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE_B,
    );
    assert!(receipt.get("error").is_none(), "install B: {receipt:#?}");

    // B 宿主必须重新拿到完整事件订阅（修复前：早退 → 零订阅 → 事件死）。
    let subscribed: Vec<String> = records(COOKIE_B)
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
            "host B missing re-subscription: {event}"
        );
    }

    // ── 新会话事件 → 刷新在 B 上恢复（新通道） ─────────────────────────
    send_event(
        "session_start",
        json!({"type": "session_start", "reason": "resume"}),
    );
    send_event("message_end", message_end.clone());
    poll_until("host B renders the footer after the rebind", |records| {
        footer_render_ok(records) > 0
    });

    // 旧宿主 A 在 rebind 之后不得再收到任何 footer 推送（stale 通道
    // 回归断言——修复前 poll 定时器会一直烧旧 cookie）。
    std::thread::sleep(Duration::from_millis(300));
    let a_footer_after = records(COOKIE_A)
        .iter()
        .filter(|(method, _)| method == "ui.setFooter")
        .count();
    assert_eq!(
        a_footer_after, a_footer_count,
        "host A must not receive post-rebind footer pushes"
    );

    let _ = std::fs::remove_dir_all(&agent);
}
