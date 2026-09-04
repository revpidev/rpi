//! `/resume` 会话替换回归（独立二进制：STATE OnceLock 只允许一次首次
//! install；本测试在其上验证第二宿主的 rebind 路径）。
//!
//! 复现（修复前）：会话替换重载同一 dlopen 记忆化的 cdylib，第二次
//! install 的 `STATE.set` 失败被吞（"first instance serves"）——工具在
//! 新宿主上重注册了（注册在 gate 之前），但 `STATE.host` 永久冻结在
//! 宿主 1 的通道：此后每次 web_fetch 的 `ctx.cwd`/settings 解析与流式
//! `toolUpdate` 推送都走旧 cookie——旧宿主随 apply() 丢弃后为悬垂指针
//! （UAF），未丢弃时则是失效通道（cwd 回退进程目录 → settings 解析错）。
//!
//! 修复后：第二次 install rebind 到新宿主通道。断言：B 宿主重注册两
//! 个工具、toolExecute 的 `ctx.cwd` 走 B 的通道、A 宿主在 rebind 后
//! 零宿主调用。

use std::collections::HashMap;
use std::sync::Mutex;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{json, Value};

/// Two fake hosts, switched by the cookie VALUE (0xA / 0xB — never
/// dereferenced). Each records every host call + its canned `ctx.cwd`.
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

fn records(cookie: PluginCookie) -> Vec<(String, Value)> {
    with_side(cookie, |side| side.records.clone())
}

fn registered_tools(cookie: PluginCookie) -> Vec<String> {
    records(cookie)
        .iter()
        .filter(|(method, _)| method == "registerTool")
        .filter_map(|(_, args)| {
            // smart-fetch 的 register 直接把 definition 作为 args 传。
            args.get("name").and_then(Value::as_str).map(str::to_string)
        })
        .collect()
}

fn ctx_cwd_calls(cookie: PluginCookie) -> usize {
    records(cookie)
        .iter()
        .filter(|(method, _)| method == "ctx.cwd")
        .count()
}

/// One toolExecute dispatch (sync — the plugin blocks internally).
fn dispatch_tool_execute(message: Value) -> Value {
    let reply = rpi_ext_smart_fetch::dispatch(
        COOKIE_B,
        RVec::from(serde_json::to_vec(&message).expect("serialize message")),
    );
    serde_json::from_slice(&reply[..]).unwrap_or(Value::Null)
}

#[test]
fn resume_rebinds_second_host_and_routes_cwd_through_it() {
    // 宿主 A：进程启动。ctx.cwd 走默认 canned（null → 进程目录回退）。
    let receipt = rpi_ext_smart_fetch::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE_A,
    );
    assert_eq!(receipt, json!({"ok": true}), "install A must succeed");
    let tools_a = registered_tools(COOKIE_A);
    assert!(
        tools_a.contains(&"web_fetch".to_string())
            && tools_a.contains(&"batch_web_fetch".to_string()),
        "host A must get both tools: {tools_a:?}"
    );
    let a_call_count = records(COOKIE_A).len();

    // ── 宿主 B：/resume 重载同一插件（修复点） ─────────────────────────
    let receipt = rpi_ext_smart_fetch::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        COOKIE_B,
    );
    assert_eq!(
        receipt,
        json!({"ok": true}),
        "second-host install must succeed (rebind)"
    );
    let tools_b = registered_tools(COOKIE_B);
    assert!(
        tools_b.contains(&"web_fetch".to_string())
            && tools_b.contains(&"batch_web_fetch".to_string()),
        "host B must get both tools re-registered: {tools_b:?}"
    );

    // ── toolExecute：ctx.cwd 必须走 B 的通道 ──────────────────────────
    // 端口 9（discard）无人监听 → 连接拒绝，快速失败；重点在执行前的
    // resolve_runtime → ctx.cwd 已落在 B 上。
    let result = dispatch_tool_execute(json!({
        "kind": "toolExecute",
        "toolName": "web_fetch",
        "toolCallId": "call-1",
        "params": { "url": "http://127.0.0.1:9/mcp" },
    }));
    // 快速失败 sanity：错误结果（连接拒绝），非 panic 哨兵。
    assert!(
        result.get("content").is_some(),
        "web_fetch must return a tool result envelope: {result:#?}"
    );

    assert!(
        ctx_cwd_calls(COOKIE_B) > 0,
        "the post-rebind execute must resolve ctx.cwd through host B: {:#?}",
        records(COOKIE_B)
    );
    assert_eq!(
        records(COOKIE_A).len(),
        a_call_count,
        "host A must not receive any post-rebind host calls (stale channel)"
    );
}
