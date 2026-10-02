//! Worker-thread entry: one worker runs one script inside a fresh QuickJS VM
//! (a separate wasm instance), relays tool calls and output to the host, and
//! reports the result (port of `packages/codemode/src/runtime/worker.ts` and
//! the host-side `Execution` lifecycle @ a13d35a74).
//!
//! The host side terminates the worker when the script settles, times out,
//! or is aborted; the worker exists so that a spinning script never blocks
//! the async host. The VM lives until the worker exits.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wasmtime::{AsContextMut, Caller, Engine, Extern, Instance, Linker, Memory, Module, Store};

use crate::quickjs::{self, HostQjs, QjsFuncs, QjsInstance, QuickJsVm, Thrown};
use crate::runtime::prelude::PRELUDE_SOURCE;
use crate::runtime::protocol::{CallTarget, HostToWorker, WorkerToHost};
use crate::wasm::{FUEL_BUDGET, MAX_STACK_SIZE, WASM_MEMORY_LIMIT_BYTES, compiled_module, engine};

/// Everything one execution needs to start.
pub struct WorkerInput {
    pub code: String,
    /// `{name, jsName, description}` tuples, JSON-encoded by the host later.
    pub tools: Vec<(String, String, String)>,
    /// `{name, spread}` tuples.
    pub globals: Vec<(String, bool)>,
    /// Snapshot for `load()`: key to JSON text.
    pub store: serde_json::Map<String, Value>,
    pub memory_limit_bytes: Option<u64>,
    pub interrupt: Arc<AtomicBool>,
    pub events: tokio::sync::mpsc::UnboundedSender<WorkerToHost>,
    pub replies: std::sync::mpsc::Receiver<HostToWorker>,
}

/// Host-call state of one worker VM.
struct WorkerHost {
    interrupt: Arc<AtomicBool>,
    events: tokio::sync::mpsc::UnboundedSender<WorkerToHost>,
    qjs: Option<QjsInstance>,
    done: bool,
    limiter: MemoryLimiter,
}

impl HostQjs for WorkerHost {
    fn qjs_instance(&self) -> Option<QjsInstance> {
        self.qjs.clone()
    }
}

/// Hard cap on the wasm linear memory; the JS heap limit is enforced by
/// QuickJS itself (`qjs_set_memory_limit`).
struct MemoryLimiter;

impl wasmtime::ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(desired <= WASM_MEMORY_LIMIT_BYTES.max(current))
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(true)
    }
}

/// Run one script on a dedicated thread. Never panics: all failures are
/// reported through the event channel.
pub fn run_worker(input: WorkerInput) {
    let module: Module = match compiled_module() {
        Ok(module) => module.clone(),
        Err(message) => {
            let _ = input.events.send(WorkerToHost::Crash { message });
            return;
        }
    };
    let mut store = Store::new(
        engine(),
        WorkerHost {
            interrupt: input.interrupt.clone(),
            events: input.events.clone(),
            qjs: None,
            done: false,
            limiter: MemoryLimiter,
        },
    );
    store.limiter(|state| &mut state.limiter);
    // CPU backstop: the budget is replenished at every bridge/settle boundary
    // (see `host_call` and `run_script`), so only a wasm-internal spin can
    // exhaust it. QuickJS's own interrupt handler is the primary timeout
    // mechanism.
    let _ = store.set_fuel(FUEL_BUDGET);

    let mut linker: Linker<WorkerHost> = Linker::new(engine());
    if let Err(error) = define_imports(&mut linker) {
        let _ = input.events.send(WorkerToHost::Crash { message: error });
        return;
    }
    let instance: Instance = match linker.instantiate(&mut store, &module) {
        Ok(instance) => instance,
        Err(error) => {
            let _ = input.events.send(WorkerToHost::Crash {
                message: format!("Failed to start worker: {error}"),
            });
            return;
        }
    };
    let memory = match instance.get_memory(&mut store, "memory") {
        Some(memory) => memory,
        None => {
            let _ = input.events.send(WorkerToHost::Crash {
                message: "quickjs.wasm is missing the memory export".to_owned(),
            });
            return;
        }
    };
    let funcs = match QjsFuncs::new(&mut store, &instance) {
        Ok(funcs) => Arc::new(funcs),
        Err(message) => {
            let _ = input.events.send(WorkerToHost::Crash { message });
            return;
        }
    };
    store.data_mut().qjs = Some(QjsInstance {
        memory,
        funcs: funcs.clone(),
    });
    let mut vm = QuickJsVm {
        store,
        instance,
        memory,
        funcs,
    };

    if let Err(message) = run_script(&mut vm, input) {
        let _ = vm.store.data().events.send(WorkerToHost::Crash { message });
    }
}

fn run_script(vm: &mut QuickJsVm<WorkerHost>, input: WorkerInput) -> Result<(), String> {
    quickjs::initialize(&mut vm.store, &vm.funcs)?;
    if let Some(limit) = input.memory_limit_bytes {
        quickjs::set_memory_limit(&mut vm.store, &vm.funcs, limit.min(i32::MAX as u64) as i32);
    }
    quickjs::set_max_stack_size(&mut vm.store, &vm.funcs, MAX_STACK_SIZE);
    quickjs::set_interrupt_handler(&mut vm.store, &vm.funcs);

    let prelude = {
        let _ = vm.store.set_fuel(FUEL_BUDGET);
        match vm.eval(PRELUDE_SOURCE, "codemode-prelude.js") {
            Ok(value) => value,
            Err(thrown) => return crash_from(vm, thrown),
        }
    };
    let bridge = vm.new_host_function("bridge")?;
    let tools_json = serde_json::to_string(
        &input
            .tools
            .iter()
            .map(|(name, js_name, description)| {
                serde_json::json!({
                    "name": name,
                    "jsName": js_name,
                    "description": description,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| error.to_string())?;
    let globals_json = serde_json::to_string(
        &input
            .globals
            .iter()
            .map(|(name, spread)| serde_json::json!({ "name": name, "spread": spread }))
            .collect::<Vec<_>>(),
    )
    .map_err(|error| error.to_string())?;
    let store_json = Value::Object(input.store.clone()).to_string();

    let tools_value = vm.new_string(&tools_json)?;
    let globals_value = vm.new_string(&globals_json)?;
    let store_value = vm.new_string(&store_json)?;
    let undefined = vm.undefined();
    let api = {
        let _ = vm.store.set_fuel(FUEL_BUDGET);
        match vm.call(
            prelude,
            undefined,
            &[bridge, tools_value, globals_value, store_value],
        ) {
            Ok(value) => value,
            Err(thrown) => return crash_from(vm, thrown),
        }
    };
    vm.free_value(prelude);
    vm.free_value(bridge);
    vm.free_value(tools_value);
    vm.free_value(globals_value);
    vm.free_value(store_value);

    let settle = vm.get_prop(api, "settle")?;
    let run = vm.get_prop(api, "run")?;
    let stalled = vm.get_prop(api, "stalled")?;

    // The prefix shares the first line with the script so reported line
    // numbers match the script as written.
    let source = format!("(async (tools, console) => {{{}\n}})", input.code);
    let _ = vm.store.set_fuel(FUEL_BUDGET);
    let script = match vm.eval(&source, "codemode.js") {
        Ok(value) => value,
        Err(Thrown::Exception(exception)) => {
            let info = vm.describe_exception(exception);
            vm.free_value(exception);
            let _ = vm.store.data().events.send(WorkerToHost::Done {
                ok: false,
                value: None,
                writes: None,
                error: Some(info.json),
            });
            return Ok(());
        }
        Err(Thrown::Trap(message)) => return Err(message),
    };

    if let Err(thrown) = vm.call(run, api, &[script]) {
        return crash_from(vm, thrown);
    }
    vm.free_value(script);
    let _ = vm.store.set_fuel(FUEL_BUDGET);
    if let Err(thrown) = drain(vm, api, stalled) {
        return crash_from(vm, thrown);
    }

    while !vm.store.data().done {
        let Ok(message) = input.replies.recv() else {
            break;
        };
        let HostToWorker::Result { id, ok, payload } = message;
        let id_value = vm.new_number(f64::from(id));
        let ok_value = vm.boolean(ok);
        let has_payload = payload.is_some();
        let payload_value = match &payload {
            Some(payload) => vm.new_string(payload)?,
            None => vm.undefined(),
        };
        let result = vm.call(settle, api, &[id_value, ok_value, payload_value]);
        vm.free_value(id_value);
        vm.free_value(ok_value);
        if has_payload {
            vm.free_value(payload_value);
        }
        let _ = vm.store.set_fuel(FUEL_BUDGET);
        if let Err(thrown) = result {
            return crash_from(vm, thrown);
        }
        if let Err(thrown) = drain(vm, api, stalled) {
            return crash_from(vm, thrown);
        }
    }
    vm.free_value(settle);
    vm.free_value(run);
    vm.free_value(stalled);
    vm.free_value(api);
    Ok(())
}

/// Run queued jobs, then fail a script that waits on nothing that can ever
/// resume it (`drain` in worker.ts:145-148).
fn drain(vm: &mut QuickJsVm<WorkerHost>, api: u32, stalled: u32) -> Result<(), Thrown> {
    let _ = vm.store.set_fuel(FUEL_BUDGET);
    vm.execute_pending_jobs()?;
    let _ = vm.store.set_fuel(FUEL_BUDGET);
    let result = vm.call(stalled, api, &[])?;
    vm.free_value(result);
    Ok(())
}

fn crash_from(vm: &mut QuickJsVm<WorkerHost>, thrown: Thrown) -> Result<(), String> {
    match thrown {
        Thrown::Exception(exception) => {
            let info = vm.describe_exception(exception);
            vm.free_value(exception);
            let _ = vm.store.data().events.send(WorkerToHost::Crash {
                message: format!("{}: {}", info.name, info.message),
            });
            Ok(())
        }
        Thrown::Trap(message) => Err(message),
    }
}

// ---------------------------------------------------------------------------
// Imports: `env.*` (QuickJS host hooks) and a minimal `wasi_snapshot_preview1`
// (the worker discards engine output, worker.ts:39-51).
// ---------------------------------------------------------------------------

fn define_imports(linker: &mut Linker<WorkerHost>) -> Result<(), String> {
    linker
        .func_wrap("env", "host_call", host_call)
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "env",
            "host_interrupt",
            |caller: Caller<'_, WorkerHost>| -> i32 {
                i32::from(caller.data().interrupt.load(Ordering::SeqCst))
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "env",
            "host_promise_rejection",
            |mut caller: Caller<'_, WorkerHost>, promise: i32, reason: i32, _handled: i32| {
                // No handler: free the heap-allocated values (wasi-shim.ts:21-24).
                if let Some(qjs) = quickjs::caller_instance(&caller) {
                    quickjs::free_value(&mut caller, &qjs.funcs, promise as u32);
                    quickjs::free_value(&mut caller, &qjs.funcs, reason as u32);
                }
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "env",
            "host_module_normalize",
            |_: Caller<'_, WorkerHost>, _base: i32, name: i32| -> i32 {
                // No module loader: pass the specifier through unchanged.
                name
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "env",
            "host_module_load",
            |_: Caller<'_, WorkerHost>, _name: i32, _out_len: i32| -> i32 { 0 },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "env",
            "host_get_timezone_offset",
            |_: Caller<'_, WorkerHost>, _hi: i32, _lo: i32| -> i32 {
                // The codemode sandbox has no clock surface of its own; report
                // UTC (documented difference from the upstream host-timezone
                // default, which no codemode script can observe through the
                // tool-only capability set).
                0
            },
        )
        .map_err(|error| error.to_string())?;

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, WorkerHost>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             written: i32|
             -> i32 {
                // Engine diagnostics belong to the host application (a TUI):
                // discard every byte but report it as written so libc does not
                // retry (worker.ts:39-51).
                if fd != 1 && fd != 2 {
                    return 8; // BADF
                }
                let Some(memory) = caller_memory(&mut caller) else {
                    return 8;
                };
                let mut total: u32 = 0;
                for index in 0..iovs_len {
                    let base = iovs as u32 + (index as u32) * 8;
                    let Ok(buf) = quickjs::read_u32(&mut caller, &memory, base) else {
                        return 8;
                    };
                    let Ok(len) = quickjs::read_u32(&mut caller, &memory, base + 4) else {
                        return 8;
                    };
                    let _ = buf;
                    total = total.saturating_add(len);
                }
                if quickjs::write_u32(&mut caller, &memory, written as u32, total).is_err() {
                    return 8;
                }
                0
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |_: Caller<'_, WorkerHost>, _fd: i32| -> i32 {
                52 // NOSYS
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |mut caller: Caller<'_, WorkerHost>, fd: i32, stat: i32| -> i32 {
                if fd != 1 && fd != 2 {
                    return 8; // BADF
                }
                let Some(memory) = caller_memory(&mut caller) else {
                    return 8;
                };
                let stat = stat as u32;
                if quickjs::write_u32(&mut caller, &memory, stat, 2).is_err() {
                    return 8;
                }
                if quickjs::write_u32(&mut caller, &memory, stat + 2, 0).is_err() {
                    return 8;
                }
                for offset in [8, 16] {
                    if quickjs::write_u32(&mut caller, &memory, stat + offset, 0).is_err()
                        || quickjs::write_u32(&mut caller, &memory, stat + offset + 4, 0).is_err()
                    {
                        return 8;
                    }
                }
                0
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_seek",
            |_: Caller<'_, WorkerHost>,
             _fd: i32,
             _offset: i64,
             _whence: i32,
             _result: i32|
             -> i32 {
                52 // NOSYS
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, WorkerHost>,
             clock_id: i32,
             _precision: i64,
             result: i32|
             -> i32 {
                if clock_id != 0 && clock_id != 1 {
                    return 52; // NOSYS
                }
                let Some(memory) = caller_memory(&mut caller) else {
                    return 8;
                };
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos() as u64)
                    .unwrap_or(0);
                if quickjs::write_u32(&mut caller, &memory, result as u32, nanos as u32).is_err()
                    || quickjs::write_u32(
                        &mut caller,
                        &memory,
                        result as u32 + 4,
                        (nanos >> 32) as u32,
                    )
                    .is_err()
                {
                    return 8;
                }
                0
            },
        )
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, WorkerHost>, buf: i32, len: i32| -> i32 {
                let Some(memory) = caller_memory(&mut caller) else {
                    return 8;
                };
                let mut bytes = vec![0u8; len.max(0) as usize];
                let mut counter: u64 = 0;
                while (counter as usize) < bytes.len() {
                    let state = RandomState::new();
                    let mut hasher = state.build_hasher();
                    hasher.write_u64(counter);
                    hasher.write_u64(std::process::id() as u64);
                    let chunk = hasher.finish().to_le_bytes();
                    let start = counter as usize;
                    let end = (start + 8).min(bytes.len());
                    bytes[start..end].copy_from_slice(&chunk[..end - start]);
                    counter += 8;
                }
                match memory.write(&mut caller, buf.max(0) as usize, &bytes) {
                    Ok(()) => 0,
                    Err(_) => 8,
                }
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn caller_memory<W>(caller: &mut Caller<'_, W>) -> Option<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => Some(memory),
        _ => None,
    }
}

/// The generic `bridge` host function (worker.ts:82-137).
fn host_call(
    mut caller: Caller<'_, WorkerHost>,
    name_ptr: u32,
    name_len: u32,
    _this_ptr: u32,
    argc: u32,
    argv_ptr: u32,
) -> i32 {
    let Some(qjs) = quickjs::caller_instance(&caller) else {
        return 0;
    };
    let mut name_bytes = vec![0u8; name_len as usize];
    if qjs
        .memory
        .read(&caller, name_ptr as usize, &mut name_bytes)
        .is_err()
    {
        return 0;
    }
    let name = String::from_utf8_lossy(&name_bytes).into_owned();
    if name != "bridge" {
        return throw_host_error(
            &mut caller,
            &qjs,
            &format!("Host callback \"{name}\" is not registered"),
        );
    }
    let mut args = Vec::with_capacity(argc as usize);
    for index in 0..argc {
        match quickjs::read_u32(&mut caller, &qjs.memory, argv_ptr + index * 4) {
            Ok(value) => args.push(value),
            Err(_) => return 0,
        }
    }
    let Some(kind_value) = args.first().copied() else {
        return throw_host_error(&mut caller, &qjs, "bridge() expects a kind");
    };
    // Replenish the CPU backstop at the bridge boundary.
    let _ = caller.as_context_mut().set_fuel(FUEL_BUDGET);
    let kind = match quickjs::read_string_value(&mut caller, &qjs.funcs, &qjs.memory, kind_value) {
        Ok(kind) => kind,
        Err(_) => return 0,
    };
    match kind.as_str() {
        "call" | "global" => {
            let id =
                quickjs::to_f64(&mut caller, &qjs.funcs, args.get(1).copied().unwrap_or(0)) as u32;
            let target_name = match quickjs::read_string_value(
                &mut caller,
                &qjs.funcs,
                &qjs.memory,
                args.get(2).copied().unwrap_or(0),
            ) {
                Ok(name) => name,
                Err(_) => return 0,
            };
            let args_json = match args.get(3).copied() {
                Some(value) if !quickjs::is_undefined(&mut caller, &qjs.funcs, value) => {
                    quickjs::read_string_value(&mut caller, &qjs.funcs, &qjs.memory, value).ok()
                }
                _ => None,
            };
            let target = if kind == "call" {
                CallTarget::Tool
            } else {
                CallTarget::Global
            };
            let _ = caller.data().events.send(WorkerToHost::Call {
                id,
                target,
                name: target_name,
                args: args_json,
            });
        }
        "output" => {
            let output_kind = match quickjs::read_string_value(
                &mut caller,
                &qjs.funcs,
                &qjs.memory,
                args.get(1).copied().unwrap_or(0),
            ) {
                Ok(kind) => kind,
                Err(_) => return 0,
            };
            let item = if output_kind == "image" {
                let data = match quickjs::read_string_value(
                    &mut caller,
                    &qjs.funcs,
                    &qjs.memory,
                    args.get(2).copied().unwrap_or(0),
                ) {
                    Ok(data) => data,
                    Err(_) => return 0,
                };
                let mime_type = match quickjs::read_string_value(
                    &mut caller,
                    &qjs.funcs,
                    &qjs.memory,
                    args.get(3).copied().unwrap_or(0),
                ) {
                    Ok(mime) => mime,
                    Err(_) => return 0,
                };
                crate::types::CodemodeOutputItem::Image { data, mime_type }
            } else {
                let text = match quickjs::read_string_value(
                    &mut caller,
                    &qjs.funcs,
                    &qjs.memory,
                    args.get(2).copied().unwrap_or(0),
                ) {
                    Ok(text) => text,
                    Err(_) => return 0,
                };
                crate::types::CodemodeOutputItem::Text { text }
            };
            let _ = caller.data().events.send(WorkerToHost::Output(item));
        }
        "done" => {
            let ok = quickjs::to_bool(&mut caller, &qjs.funcs, args.get(1).copied().unwrap_or(0));
            if ok {
                let value = match args.get(2).copied() {
                    Some(value) if !quickjs::is_undefined(&mut caller, &qjs.funcs, value) => {
                        quickjs::read_string_value(&mut caller, &qjs.funcs, &qjs.memory, value).ok()
                    }
                    _ => None,
                };
                let writes = quickjs::read_string_value(
                    &mut caller,
                    &qjs.funcs,
                    &qjs.memory,
                    args.get(3).copied().unwrap_or(0),
                )
                .unwrap_or_else(|_| "[]".to_owned());
                let _ = caller.data().events.send(WorkerToHost::Done {
                    ok: true,
                    value,
                    writes: Some(writes),
                    error: None,
                });
            } else {
                let error = quickjs::read_string_value(
                    &mut caller,
                    &qjs.funcs,
                    &qjs.memory,
                    args.get(2).copied().unwrap_or(0),
                )
                .unwrap_or_else(|_| "{\"message\":\"unknown script error\"}".to_owned());
                let _ = caller.data().events.send(WorkerToHost::Done {
                    ok: false,
                    value: None,
                    writes: None,
                    error: Some(error),
                });
            }
            caller.data_mut().done = true;
        }
        _ => {}
    }
    // The trampoline takes ownership of the returned value; duplicate the
    // undefined singleton.
    let undefined = quickjs::undefined(&mut caller, &qjs.funcs);
    quickjs::dup(&mut caller, &qjs.funcs, undefined) as i32
}

fn throw_host_error(caller: &mut Caller<'_, WorkerHost>, qjs: &QjsInstance, message: &str) -> i32 {
    if let Ok(value) = quickjs::new_string(caller, &qjs.funcs, &qjs.memory, message) {
        quickjs::throw_value(caller, &qjs.funcs, value);
        quickjs::free_value(caller, &qjs.funcs, value);
    }
    0
}

/// The engine the worker thread runs on (kept public so the host side can
/// increment the epoch backstop on timeout).
pub fn engine_handle() -> &'static Engine {
    engine()
}
