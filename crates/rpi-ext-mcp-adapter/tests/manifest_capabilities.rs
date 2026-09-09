//! TE20 A8: the manifest `commands` capability and the install-time
//! `registerCommand` surface must agree (R7.2.1.2, 门禁项).
//!
//! [RPI-OWN] — no upstream parity leg; the assertions are the equivalent
//! behavioral anchor (task §4.2 A8).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::{commands, install_for_test};
use serde_json::{json, Value};

struct FakeHost {
    registered_commands: Mutex<Vec<String>>,
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // SAFETY: the Arc handed to install_for_test is kept alive by the test
    // for the whole process; from_raw + forget only re-borrows it.
    let host = unsafe { Arc::from_raw(host_ptr as *const FakeHost) };
    let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = request.get("call").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "ctx.cwd" => json!({"ok": std::env::temp_dir().to_string_lossy()}),
        "ctx.hasUI" => json!({"ok": false}),
        "ctx.mode" => json!({"ok": "print"}),
        "getFlag" => json!({"ok": null}),
        "on" | "registerFlag" | "registerTool" | "unregisterTool" | "setActiveTools" => {
            json!({"ok": true})
        }
        "getActiveTools" | "getAllTools" => json!({"ok": []}),
        "registerCommand" => {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() {
                let mut commands = host
                    .registered_commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !commands.contains(&name) {
                    commands.push(name);
                }
            }
            json!({"ok": true})
        }
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rpi-extension.json")
}

#[test]
fn manifest_commands_capability_matches_registered_commands() {
    let raw = std::fs::read_to_string(manifest_path()).expect("rpi-extension.json");
    let manifest: Value = serde_json::from_str(&raw).expect("manifest JSON");
    let capabilities: Vec<&str> = manifest
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // G4/additive: rpiAbi stays 1 and no exec capability is declared.
    assert_eq!(manifest["rpiAbi"], json!(1));
    assert!(!capabilities.contains(&"exec"));

    let declared = command_definitions();
    if declared.is_empty() {
        assert!(
            !capabilities.contains(&"commands"),
            "manifest must not declare `commands` without registrations"
        );
        return;
    }
    assert!(
        capabilities.contains(&"commands"),
        "manifest must declare `commands` when commands are registered: {capabilities:?}"
    );

    // install must register exactly the declared names (declared == registered).
    let host = Arc::new(FakeHost {
        registered_commands: Mutex::new(Vec::new()),
    });
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, Arc::into_raw(host.clone()) as PluginCookie);
    assert_eq!(installed, json!({"ok": true}), "install must succeed");
    let registered = host
        .registered_commands
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(registered, declared, "registered != declared command set");
    assert_eq!(registered, vec!["mcp".to_string(), "mcp-auth".to_string()]);
}

fn command_definitions() -> Vec<String> {
    commands::command_definitions()
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}
