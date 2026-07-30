//! T02 Wasm ABI spike — guest side (validates the protocol shape of design
//! doc §7 / requirements §9.2; the real Wasm host lands in T15).
//!
//! String-passing ABI used by this spike:
//! - All payloads are UTF-8 JSON in guest linear memory.
//! - Guest → host calls pass `(ptr, len)`.
//! - Host → guest returns pack a response string into one `u64`:
//!   `(ptr << 32) | len`, allocated via the guest's own `pir_alloc`.
//!
//! Demonstrates the three round trips the spike must close:
//! 1. `registerTool` — guest registers a tool descriptor; the host later
//!    invokes `pir_tool_execute`.
//! 2. dialog — mid-execution the guest calls `pir_host_dialog_select` and
//!    consumes the host's answer.
//! 3. declarative UI component — the guest hands the host a component tree
//!    description (`pir_host_render_component`), receives a user event back,
//!    and re-renders an updated tree.

#[link(wasm_import_module = "pir")]
extern "C" {
    fn pir_host_register_tool(ptr: *const u8, len: usize);
    fn pir_host_dialog_select(ptr: *const u8, len: usize) -> u64;
    fn pir_host_render_component(ptr: *const u8, len: usize) -> u64;
}

/// Host-callable bump allocator for response strings.
#[no_mangle]
pub extern "C" fn pir_alloc(len: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn pir_dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

/// Pack a guest-owned string for return to the host (leaks by design; the
/// host copies out and calls `pir_dealloc`).
fn pack(s: &str) -> u64 {
    let mut bytes = s.as_bytes().to_vec();
    let ptr = bytes.as_mut_ptr() as u64;
    let len = bytes.len() as u64;
    std::mem::forget(bytes);
    (ptr << 32) | len
}

unsafe fn unpack(ptr: *const u8, len: usize) -> String {
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(slice).into_owned()
}

unsafe fn host_dialog_select(request_json: &str) -> String {
    unsafe {
        let packed = pir_host_dialog_select(request_json.as_ptr(), request_json.len());
        let ptr = (packed >> 32) as u32 as *const u8;
        let len = (packed & 0xffff_ffff) as usize;
        unpack(ptr, len)
    }
}

unsafe fn host_render_component(tree_json: &str) -> String {
    unsafe {
        let packed = pir_host_render_component(tree_json.as_ptr(), tree_json.len());
        let ptr = (packed >> 32) as u32 as *const u8;
        let len = (packed & 0xffff_ffff) as usize;
        unpack(ptr, len)
    }
}

/// Extract the (single, flat) `"text":"..."` value of the spike args JSON.
/// Naive on purpose — this spike validates the ABI, not JSON parsing.
fn extract_text(args_json: &str) -> String {
    let marker = "\"text\":\"";
    if let Some(start) = args_json.find(marker) {
        let rest = &args_json[start + marker.len()..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    String::new()
}

/// Entry point 1 (called by the host right after instantiation): register the
/// spike tool and report back.
#[no_mangle]
pub extern "C" fn pir_extension_init() -> u64 {
    let descriptor = r#"{"name":"spike_echo","description":"Echoes text and asks a dialog question","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}"#;
    unsafe {
        pir_host_register_tool(descriptor.as_ptr(), descriptor.len());
    }
    pack(r#"{"registered":"spike_echo"}"#)
}

/// Entry point 2: host invokes the registered tool. Round trip 2 (dialog)
/// happens inside the execution.
#[no_mangle]
pub unsafe extern "C" fn pir_tool_execute(args_ptr: *const u8, args_len: usize) -> u64 {
    let args = unsafe { unpack(args_ptr, args_len) };
    let text = extract_text(&args);

    // Dialog round trip: ask the host UI a question, consume the answer.
    let answer = unsafe {
        host_dialog_select(r#"{"kind":"select","title":"Continue?","options":["option-a","option-b"]}"#)
    };

    let result = format!(
        r#"{{"content":[{{"type":"text","text":"echo:{text};dialog:{answer}"}}],"isError":false}}"#
    );
    pack(&result)
}

/// Entry point 3: declarative UI component render round trip. The guest
/// renders v1, receives a click event, re-renders v2, and reports the event
/// sequence back to the host.
#[no_mangle]
pub extern "C" fn pir_render_ui() -> u64 {
    let v1 = r#"{"type":"column","children":[{"type":"text","value":"Spike dialog"},{"type":"button","id":"ok","label":"OK"}]}"#;
    let event1 = unsafe { host_render_component(v1) };

    let mut status = format!("event1={event1}");
    if event1.contains("\"click\"") {
        let v2 = r#"{"type":"column","children":[{"type":"text","value":"Clicked!"},{"type":"button","id":"ok","label":"Done"}]}"#;
        let event2 = unsafe { host_render_component(v2) };
        status.push_str(&format!(";event2={event2}"));
    }
    pack(&format!(r#"{{"status":"{status}"}}"#))
}
