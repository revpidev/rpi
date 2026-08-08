//! Test fixture native plugin (T15 W7): an abi_stable cdylib implementing
//! the same permission-gate behavior as the WAT wasm guest — `on
//! tool_call` blocks with reason "native-gate".

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use pir_ext_host::native::{PirHostCalls, PirNativeModule, PirNativeModule_Ref, PluginCookie};
use serde_json::{json, Value};

fn host_call(calls: &PirHostCalls, cookie: PluginCookie, method: &str, args: Value) -> Value {
    let request = serde_json::to_vec(&json!({
        "call": method,
        "args": args,
        "seq": 0,
    }))
    .unwrap_or_default();
    let response = (calls.call)(cookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

fn pack(value: Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()))
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: PirHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let response = host_call(&calls, cookie, "on", json!({"event": "tool_call"}));
    if response.get("error").is_some() {
        return pack(response);
    }
    pack(json!({"ok": true}))
}

pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind == "event" && message.get("event").and_then(Value::as_str) == Some("tool_call") {
        return pack(json!({"block": true, "reason": "native-gate"}));
    }
    pack(Value::Null)
}

/// The root module export (abi_stable).
#[abi_stable::export_root_module]
pub fn module() -> PirNativeModule_Ref {
    PirNativeModule {
        pir_extension_init: init,
        pir_dispatch: dispatch,
    }
    .leak_into_prefix()
}
