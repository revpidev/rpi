//! TE52 `/statusline` command over the carrier seam: mode gating and the
//! `ui.emitText` contract (validateOnly first, no editor/file write outside
//! the TUI). The editor draft/save state machine itself is unit-tested in
//! `editor::tests`; this binary proves the dispatch-level routing and the
//! zero-side-effect non-TUI paths.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

static RECORDS: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
static CANNED: OnceLock<Mutex<std::collections::HashMap<String, Value>>> = OnceLock::new();
static UNSUPPORTED_OUTPUT: AtomicBool = AtomicBool::new(false);

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
    if method == "ui.emitText" && UNSUPPORTED_OUTPUT.load(Ordering::SeqCst) {
        return RVec::from(
            serde_json::to_vec(&json!({
                "error": {"kind": "call", "message": "unsupported-output"}
            }))
            .unwrap_or_default(),
        );
    }
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
        // A single scripted poll event ends the mounted editor immediately.
        "ui.mountComponent" => return RVec::from(b"{\"ok\":{\"handle\":1}}".to_vec()),
        "ui.pollComponent" => {
            return RVec::from(
                serde_json::to_vec(&json!({
                    "ok": {"event": {"type": "dispose", "reason": "sessionShutdown"}}
                }))
                .unwrap_or_default(),
            );
        }
        "ui.renderComponent" | "ui.disposeComponent" => {
            return RVec::from(b"{\"ok\":null}".to_vec());
        }
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

fn install() {
    set_canned("ctx.cwd", json!("/tmp/project"));
    set_canned("ctx.model", json!({"id": "m", "name": "Model"}));
    set_canned("getThinkingLevel", json!("off"));
    set_canned("getSessionName", Value::Null);
    set_canned("getMode", json!("default"));
    set_canned("ctx.isIdle", json!(true));
    set_canned(
        "ctx.getContextUsage",
        json!({"tokens": 100, "contextWindow": 1000, "percent": 10.0}),
    );
    set_canned(
        "ctx.sessionStats",
        json!({
            "sessionId": "s1",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 0,
            "tokens": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "cost": 0.0
        }),
    );
    set_canned("ui.confirm", json!(false));
    set_canned("ui.input", Value::Null);
    let receipt = rpi_ext_statusline::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        std::ptr::null(),
    );
    assert!(receipt.get("error").is_none(), "{receipt:#?}");
}

const COOKIE: PluginCookie = std::ptr::null();

fn send_command(args: &str) -> Value {
    let message = json!({"kind": "command", "name": "statusline", "args": args});
    let packed = rpi_ext_statusline::dispatch(
        COOKIE,
        RVec::from(serde_json::to_vec(&message).expect("serialize command")),
    );
    serde_json::from_slice(&packed[..]).unwrap_or(Value::Null)
}

fn agent_dir() -> std::path::PathBuf {
    std::env::var("RPI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .expect("RPI_CODING_AGENT_DIR set by the test wrapper")
}

fn write_settings(value: Value) {
    let dir = agent_dir();
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&value).expect("serialize"),
    )
    .expect("write");
}

fn emit_texts(records: &[(String, Value)]) -> Vec<Value> {
    records
        .iter()
        .filter(|(method, _)| method == "ui.emitText")
        .map(|(_, args)| args.clone())
        .collect()
}

fn calls_of<'a>(records: &'a [(String, Value)], method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|(recorded, _)| recorded == method)
        .map(|(_, args)| args)
        .collect()
}

#[test]
fn statusline_command_routes_by_mode() {
    let agent =
        std::env::temp_dir().join(format!("rpi-statusline-editor-int-{}", std::process::id()));
    std::fs::create_dir_all(&agent).expect("mkdir");
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent);
    write_settings(json!({"statusLine": {
        "type": "items",
        "items": ["model"],
        "placement": "widget"
    }}));
    install();

    // ── RPC + help: validate first, then emit the help text; no editor.
    set_canned("ctx.mode", json!("rpc"));
    set_canned("ctx.hasUI", json!(true));
    let result = send_command("help");
    assert!(result.get("error").is_none(), "{result:#?}");
    let texts = emit_texts(&records());
    assert_eq!(texts.len(), 2, "validateOnly + text: {texts:#?}");
    assert_eq!(texts[0], json!({"text": "", "validateOnly": true}));
    assert!(
        texts[1]["text"]
            .as_str()
            .unwrap()
            .contains("session-duration")
    );
    assert!(calls_of(&records(), "ui.mountComponent").is_empty());

    // ── RPC: current configuration text.
    let result = send_command("");
    assert!(result.get("error").is_none(), "{result:#?}");
    let texts = emit_texts(&records());
    let config = texts.last().expect("config text");
    let text = config["text"].as_str().expect("text");
    assert!(text.contains("items (Widget)"), "{text}");
    assert!(text.contains("model"), "{text}");
    assert!(calls_of(&records(), "ui.mountComponent").is_empty());

    // ── Unknown argument → help, still no editor.
    send_command("nonsense");
    assert!(
        emit_texts(&records())
            .last()
            .and_then(|args| args["text"].as_str())
            .is_some_and(|text| text.contains("unknown argument"))
    );

    // ── TUI reset with a declined confirmation: no mount, no write.
    set_canned("ctx.mode", json!("tui"));
    let before = std::fs::read(agent.join("settings.json")).expect("bytes");
    let result = send_command("reset");
    assert!(result.get("error").is_none(), "{result:#?}");
    assert!(!calls_of(&records(), "ui.confirm").is_empty());
    assert!(calls_of(&records(), "ui.mountComponent").is_empty());
    assert_eq!(
        std::fs::read(agent.join("settings.json")).expect("bytes"),
        before
    );

    // ── TUI open: mounts the line-frame editor and disposes without saving.
    let mounts_before = calls_of(&records(), "ui.mountComponent").len();
    let result = send_command("");
    assert!(result.get("error").is_none(), "{result:#?}");
    assert_eq!(
        calls_of(&records(), "ui.mountComponent").len(),
        mounts_before + 1,
        "the TUI opens the editor"
    );
    let snapshot = records();
    let render = calls_of(&snapshot, "ui.renderComponent");
    let frame = render.last().expect("final frame");
    assert!(
        frame["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line
                .as_str()
                .is_some_and(|line| line.contains("rpi-statusline")))),
        "the editor frame is rendered: {frame:#?}"
    );
    assert_eq!(
        std::fs::read(agent.join("settings.json")).expect("bytes"),
        before,
        "opening/cancelling does not write settings"
    );

    // ── NullUi/subagents: unsupported-output before any editor or write.
    set_canned("ctx.mode", json!("print"));
    set_canned("ctx.hasUI", json!(false));
    UNSUPPORTED_OUTPUT.store(true, Ordering::SeqCst);
    let emits_before = emit_texts(&records()).len();
    let mounts_before = calls_of(&records(), "ui.mountComponent").len();
    let result = send_command("");
    assert_eq!(result["error"]["kind"], "unsupported-output", "{result:#?}");
    // The validate attempt reached the host, which answered unsupported.
    assert_eq!(emit_texts(&records()).len(), emits_before + 1);
    assert_eq!(
        calls_of(&records(), "ui.mountComponent").len(),
        mounts_before
    );
    assert_eq!(
        std::fs::read(agent.join("settings.json")).expect("bytes"),
        before
    );

    std::fs::remove_dir_all(&agent).ok();
}
