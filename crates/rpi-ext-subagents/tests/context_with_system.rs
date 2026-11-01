//! FR-J R1 wiring: the plugin registers `context_with_system` and returns the
//! notice-marked message list with system-message positions unchanged.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{Value, json};

struct FakeHost {
    cwd: PathBuf,
    registered: Mutex<Vec<String>>,
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    let host = unsafe { &*(host_ptr as *const FakeHost) };
    let parsed: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = parsed.get("call").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "registerTool" | "registerCommand" | "registerMessageRenderer" | "registerFlag" => {
            json!({ "ok": true })
        }
        "on" => {
            if let Some(event) = parsed
                .get("args")
                .and_then(|args| args.get("event"))
                .and_then(Value::as_str)
            {
                host.registered.lock().unwrap().push(event.to_string());
            }
            json!({ "ok": true })
        }
        "ctx.cwd" => json!({ "ok": host.cwd.to_string_lossy() }),
        "ctx.model" => json!({ "ok": null }),
        "getAllTools" => json!({ "ok": [] }),
        _ => json!({ "error": { "kind": "unknownMethod", "message": method } }),
    };
    RVec::from(serde_json::to_vec(&response).unwrap_or_default())
}

fn sandbox() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("rpi-sub-ctxwit-{}-{nonce}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("agent/extensions/subagent")).unwrap();
    std::fs::write(
        dir.join("agent/extensions/subagent/rpi-extension.json"),
        r#"{"name":"subagent","version":"0.0.0","rpiAbi":1,"entry":"lib"}"#,
    )
    .unwrap();
    dir
}

#[test]
fn context_with_system_marks_notices_and_keeps_system_positions() {
    let dir = sandbox();
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", dir.join("agent"));
    let host = Arc::new(FakeHost {
        cwd: dir.join("proj"),
        registered: Mutex::new(Vec::new()),
    });
    let response = rpi_ext_subagents::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host.clone()) as PluginCookie,
    );
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)), "{response}");
    let registered = host.registered.lock().unwrap().clone();
    assert!(
        registered
            .iter()
            .any(|event| event == "context_with_system"),
        "context_with_system registered: {registered:?}"
    );

    let result = rpi_ext_subagents::dispatch_event_for_test(
        "context_with_system",
        json!({
            "messages": [
                { "role": "system", "content": [{ "type": "text", "text": "lead" }] },
                { "role": "custom", "customType": "subagent-notify", "content": "done", "display": true },
                { "role": "system", "content": [{ "type": "text", "text": "mid" }] },
                { "role": "user", "content": "next" }
            ]
        }),
    );
    let messages = result
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], json!("system"));
    assert_eq!(messages[2]["role"], json!("system"));
    assert_eq!(messages[2]["content"][0]["text"], json!("mid"));
    let marked = messages[1]["content"].as_str().expect("string content");
    assert!(marked.starts_with("<rpi_subagents_notice type=\"subagent-notify\""));
    assert!(marked.ends_with("</rpi_subagents_notice>"));
    assert!(marked.contains("done"));
}
