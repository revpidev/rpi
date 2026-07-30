//! T02 Wasm ABI spike — host side (design doc §7 / requirements §9.2).
//!
//! Loads the spike guest (`examples/wasm-spike/guest`, wasm32-unknown-unknown)
//! into wasmtime and closes the three round trips the M0 spike must validate:
//!
//! 1. **registerTool**: the guest registers a tool descriptor; the host calls
//!    back into the guest's `pir_tool_execute` with JSON arguments.
//! 2. **dialog**: mid-execution the guest calls `pir_host_dialog_select`; the
//!    host (scripted UiBridge) answers `"option-b"`.
//! 3. **declarative UI component**: the guest sends a component tree
//!    description; the host renders it to a text frame and returns a user
//!    event; the guest re-renders an updated tree.
//!
//! Runbook:
//! ```bash
//! cd crates/pir-ext-host/examples/wasm-spike/guest
//! cargo build --release --target wasm32-unknown-unknown
//! cd ../../../../..
//! cargo run -p pir-ext-host --example wasm_spike
//! ```
//!
//! Exit code 0 + `WASM SPIKE OK` = spike passed.

use std::path::PathBuf;

use serde_json::Value;
use wasmtime::{AsContextMut, Caller, Engine, Instance, Linker, Module, Store, TypedFunc};

/// Host-side state shared with the imported host functions.
#[derive(Default)]
struct HostState {
    instance: Option<Instance>,
    registered_tools: Vec<String>,
    render_count: u32,
    rendered_frames: Vec<String>,
}

fn guest_wasm_path() -> PathBuf {
    if let Ok(p) = std::env::var("PIR_WASM_SPIKE_GUEST") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "examples/wasm-spike/guest/target/wasm32-unknown-unknown/release/pir_wasm_spike_guest.wasm",
    )
}

/// Read a guest string at `(ptr, len)`.
fn read_guest_string(
    caller: &mut Caller<'_, HostState>,
    ptr: u32,
    len: u32,
) -> wasmtime::Result<String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest does not export memory"))?;
    let mut buf = vec![0u8; len as usize];
    memory.read(&mut *caller, ptr as usize, &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Allocate a string inside the guest (via its `pir_alloc`), write the bytes,
/// and return the packed `(ptr << 32) | len` handle.
fn write_guest_string(caller: &mut Caller<'_, HostState>, s: &str) -> wasmtime::Result<u64> {
    let instance = caller
        .data()
        .instance
        .ok_or_else(|| wasmtime::Error::msg("instance not linked into host state"))?;
    let alloc: TypedFunc<u32, u32> = instance.get_typed_func(&mut *caller, "pir_alloc")?;
    let memory = instance
        .get_memory(&mut *caller, "memory")
        .ok_or_else(|| wasmtime::Error::msg("guest does not export memory"))?;
    let bytes = s.as_bytes();
    let ptr = alloc.call(&mut *caller, bytes.len() as u32)?;
    memory.write(&mut *caller, ptr as usize, bytes)?;
    Ok(((ptr as u64) << 32) | bytes.len() as u64)
}

/// Mock renderer: turns the declarative component tree into a text frame.
/// The real UiBridge (T15) maps these descriptions onto pir-tui components.
fn render_component_frame(tree: &Value) -> String {
    fn render_node(node: &Value, out: &mut Vec<String>) {
        match node.get("type").and_then(Value::as_str) {
            Some("column") => {
                if let Some(children) = node.get("children").and_then(Value::as_array) {
                    for child in children {
                        render_node(child, out);
                    }
                }
            }
            Some("text") => {
                out.push(node["value"].as_str().unwrap_or("").to_owned());
            }
            Some("button") => {
                out.push(format!("[ {} ]", node["label"].as_str().unwrap_or("?")));
            }
            other => out.push(format!("<unknown component {other:?}>")),
        }
    }
    let mut lines = Vec::new();
    render_node(tree, &mut lines);
    lines.join("\n")
}

fn main() -> wasmtime::Result<()> {
    let wasm_path = guest_wasm_path();
    println!("guest module: {}", wasm_path.display());

    let engine = Engine::default();
    let module = Module::from_file(&engine, &wasm_path).map_err(|e| {
        wasmtime::Error::msg(format!(
            "failed to load guest wasm (build it first — see doc comment): {e}"
        ))
    })?;
    let mut store = Store::new(&engine, HostState::default());
    let mut linker = Linker::new(&engine);

    // Host import: registerTool(descriptor JSON).
    linker.func_wrap(
        "pir",
        "pir_host_register_tool",
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let descriptor = read_guest_string(&mut caller, ptr, len)?;
            let parsed: Value = serde_json::from_str(&descriptor)?;
            let name = parsed["name"].as_str().unwrap_or("<unnamed>").to_owned();
            println!("[host] registerTool: {name}");
            caller.data_mut().registered_tools.push(name);
            Ok(())
        },
    )?;

    // Host import: dialog select round trip (scripted UiBridge answer).
    linker.func_wrap(
        "pir",
        "pir_host_dialog_select",
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> wasmtime::Result<u64> {
            let request = read_guest_string(&mut caller, ptr, len)?;
            println!("[host] dialog select request: {request}");
            write_guest_string(&mut caller, r#"{"selected":"option-b"}"#)
        },
    )?;

    // Host import: declarative component render; returns a scripted user
    // event (click on first render, close on second).
    linker.func_wrap(
        "pir",
        "pir_host_render_component",
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> wasmtime::Result<u64> {
            let tree_json = read_guest_string(&mut caller, ptr, len)?;
            let tree: Value = serde_json::from_str(&tree_json)?;
            let frame = render_component_frame(&tree);
            let n = caller.data().render_count + 1;
            println!("[host] render frame #{n}:\n---\n{frame}\n---");
            caller.data_mut().render_count = n;
            caller.data_mut().rendered_frames.push(frame);
            let event = if n == 1 {
                r#"{"type":"click","target":"ok"}"#
            } else {
                r#"{"type":"close"}"#
            };
            write_guest_string(&mut caller, event)
        },
    )?;

    let instance = linker.instantiate(&mut store, &module)?;
    store.data_mut().instance = Some(instance);

    // Round trip 1: registerTool.
    let init: TypedFunc<(), u64> = instance.get_typed_func(&mut store, "pir_extension_init")?;
    let packed = init.call(&mut store, ())?;
    println!("[host] init returned handle 0x{packed:016x}");
    if store.data().registered_tools != ["spike_echo"] {
        return Err(wasmtime::Error::msg(format!(
            "expected spike_echo to be registered, got {:?}",
            store.data().registered_tools
        )));
    }

    // Round trip 2: tool execution with an embedded dialog round trip.
    let args = r#"{"text":"hello-spike"}"#;
    let args_handle = {
        let alloc: TypedFunc<u32, u32> = instance.get_typed_func(&mut store, "pir_alloc")?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("no memory"))?;
        let ptr = alloc.call(&mut store, args.len() as u32)?;
        memory.write(&mut store, ptr as usize, args.as_bytes())?;
        (ptr, args.len() as u32)
    };
    let execute: TypedFunc<(u32, u32), u64> =
        instance.get_typed_func(&mut store, "pir_tool_execute")?;
    let result_packed = execute.call(&mut store, args_handle)?;
    let result = {
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("no memory"))?;
        let ptr = (result_packed >> 32) as u32;
        let len = (result_packed & 0xffff_ffff) as u32;
        let mut buf = vec![0u8; len as usize];
        memory.read(&mut store, ptr as usize, &mut buf)?;
        String::from_utf8_lossy(&buf).into_owned()
    };
    println!("[host] tool result: {result}");
    if !(result.contains("echo:hello-spike")
        && result.contains("dialog:{\"selected\":\"option-b\"}"))
    {
        return Err(wasmtime::Error::msg(format!(
            "tool result missing echo text or dialog answer: {result}"
        )));
    }

    // Round trip 3: declarative component render (two frames).
    let render_ui: TypedFunc<(), u64> = instance.get_typed_func(&mut store, "pir_render_ui")?;
    let status_packed = render_ui.call(&mut store, ())?;
    let status = {
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("no memory"))?;
        let ptr = (status_packed >> 32) as u32;
        let len = (status_packed & 0xffff_ffff) as u32;
        let mut buf = vec![0u8; len as usize];
        memory.read(
            AsContextMut::as_context_mut(&mut store),
            ptr as usize,
            &mut buf,
        )?;
        String::from_utf8_lossy(&buf).into_owned()
    };
    println!("[host] ui status: {status}");
    if store.data().render_count != 2 {
        return Err(wasmtime::Error::msg("expected 2 render frames"));
    }
    {
        let frames = &store.data().rendered_frames;
        if !(frames[0].contains("Spike dialog")
            && frames[0].contains("[ OK ]")
            && frames[1].contains("Clicked!"))
        {
            return Err(wasmtime::Error::msg(format!(
                "rendered frames do not match the expected component trees: {frames:?}"
            )));
        }
    }
    if !status.contains("\"click\"") {
        return Err(wasmtime::Error::msg(format!(
            "guest did not receive click event: {status}"
        )));
    }

    println!("WASM SPIKE OK");
    Ok(())
}
