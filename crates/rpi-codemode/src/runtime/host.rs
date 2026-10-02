//! Async host side of the sandbox (port of `CodemodeSandbox` / `Execution`
//! in `packages/codemode/src/runtime/host.ts` @ a13d35a74).
//!
//! One script run gets its own worker thread, fresh wasmtime `Store`, and
//! fresh QuickJS VM. A runaway script, including one that only spins the
//! microtask queue, is interrupted and cannot poison a later run.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::identifier::to_codemode_identifier;
use crate::runtime::protocol::{CallTarget, HostToWorker, WorkerToHost};
use crate::runtime::worker::{WorkerInput, run_worker};
use crate::types::{
    CodemodeCall, CodemodeCallStatus, CodemodeError, CodemodeErrorKind, CodemodeExecuteOptions,
    CodemodeOutputItem, CodemodeResult, CodemodeSandboxOptions, CodemodeStoreWrites,
    CodemodeTimeout, CodemodeTool, CodemodeToolContext, RESERVED_GLOBALS,
};

/// One in-flight call as the host tracks it (`PendingCall`, host.ts:68-72).
struct PendingCall {
    /// Tool calls get a record; globals are tracked for the reply only.
    index: Option<usize>,
    started_at: Instant,
}

/// Call records plus the pending index (`Execution.output/calls/pending`).
#[derive(Default)]
struct CallTable {
    records: Vec<CodemodeCall>,
    by_id: HashMap<u32, PendingCall>,
}

/// `CodemodeSandbox`: holds the tool table and defaults; every `execute()`
/// gets its own worker and VM.
pub struct CodemodeSandbox {
    /// Insertion-ordered so `tools`/`ALL_TOOLS` expose registration order
    /// (upstream `Map` semantics).
    tools: Mutex<Vec<CodemodeTool>>,
    globals: Mutex<Vec<CodemodeTool>>,
    timeout: CodemodeTimeout,
    memory_limit_bytes: Option<u64>,
    closed: AtomicBool,
    running: Mutex<HashMap<u64, CancellationToken>>,
    next_execution_id: Mutex<u64>,
    idle: tokio::sync::Notify,
    /// Test-only hook: report a sandbox error before starting a worker
    /// (mirrors upstream's injectable `wasm`/`workerUrl` failure paths).
    #[doc(hidden)]
    pub fail_engine: Option<String>,
}

impl CodemodeSandbox {
    /// Build a sandbox; `Err` mirrors the upstream constructor throws for
    /// invalid/reserved/conflicting global names and duplicate tools.
    pub fn new(options: CodemodeSandboxOptions) -> Result<Self, String> {
        let mut tools: Vec<CodemodeTool> = Vec::new();
        for tool in options.tools {
            if tools.iter().any(|existing| existing.name == tool.name) {
                return Err(format!("Tool \"{}\" is already registered", tool.name));
            }
            tools.push(tool);
        }
        let mut globals: Vec<CodemodeTool> = Vec::new();
        let mut namespaces: Vec<String> = Vec::new();
        for global in options.globals {
            let parts: Vec<&str> = global.name.split('.').collect();
            if parts.len() > 2
                || parts.iter().any(|part| !is_identifier(part))
                || RESERVED_GLOBALS.contains(&parts[0])
            {
                return Err(format!("Invalid global name \"{}\"", global.name));
            }
            if globals.iter().any(|existing| existing.name == global.name) {
                return Err(format!("Global \"{}\" is already registered", global.name));
            }
            if parts.len() == 2 && !namespaces.contains(&parts[0].to_owned()) {
                namespaces.push(parts[0].to_owned());
            }
            globals.push(global);
        }
        for namespace in namespaces {
            if globals.iter().any(|global| global.name == namespace) {
                return Err(format!(
                    "Global \"{namespace}\" conflicts with the namespace \"{namespace}\""
                ));
            }
        }
        Ok(Self {
            tools: Mutex::new(tools),
            globals: Mutex::new(globals),
            timeout: options.timeout_ms,
            memory_limit_bytes: options.memory_limit_bytes,
            closed: AtomicBool::new(false),
            running: Mutex::new(HashMap::new()),
            next_execution_id: Mutex::new(1),
            idle: tokio::sync::Notify::new(),
            fail_engine: None,
        })
    }

    /// Throws if a tool with the same name is already registered.
    pub fn register_tool(&self, tool: CodemodeTool) -> Result<(), String> {
        let mut tools = lock(&self.tools);
        if tools.iter().any(|existing| existing.name == tool.name) {
            return Err(format!("Tool \"{}\" is already registered", tool.name));
        }
        tools.push(tool);
        Ok(())
    }

    pub fn unregister_tool(&self, name: &str) -> bool {
        let mut tools = lock(&self.tools);
        let before = tools.len();
        tools.retain(|tool| tool.name != name);
        tools.len() != before
    }

    pub fn tools(&self) -> Vec<CodemodeTool> {
        lock(&self.tools).clone()
    }

    pub fn globals(&self) -> Vec<CodemodeTool> {
        lock(&self.globals).clone()
    }

    /// `execute(code, options)`: `code` is an async function body, so
    /// `return` and top-level `await` work. Never `Err` for script failures;
    /// `Err` only when the sandbox is closed (upstream: a rejected promise).
    pub async fn execute(
        &self,
        code: &str,
        options: CodemodeExecuteOptions,
    ) -> Result<CodemodeResult, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("Sandbox is closed".to_owned());
        }
        let execution_id = {
            let mut next = lock(&self.next_execution_id);
            let id = *next;
            *next += 1;
            id
        };
        let abort = CancellationToken::new();
        lock(&self.running).insert(execution_id, abort.clone());
        let result = self.run_execution(code, options, abort).await;
        lock(&self.running).remove(&execution_id);
        self.idle.notify_one();
        result
    }

    /// Abort in-flight executions (they resolve with `kind: "aborted"`) and
    /// reject new ones; waits until every in-flight execution has settled.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let tokens: Vec<CancellationToken> = lock(&self.running).values().cloned().collect();
        for token in tokens {
            token.cancel();
        }
        loop {
            let notified = self.idle.notified();
            if lock(&self.running).is_empty() {
                break;
            }
            notified.await;
        }
    }

    async fn run_execution(
        &self,
        code: &str,
        options: CodemodeExecuteOptions,
        abort: CancellationToken,
    ) -> Result<CodemodeResult, String> {
        if let Some(message) = &self.fail_engine {
            return Ok(CodemodeResult::Err {
                error: CodemodeError {
                    kind: CodemodeErrorKind::Sandbox,
                    name: None,
                    message: message.clone(),
                    stack: None,
                },
                output: Vec::new(),
                calls: Vec::new(),
            });
        }
        let tools: Vec<CodemodeTool> = lock(&self.tools).clone();
        let globals: Vec<CodemodeTool> = lock(&self.globals).clone();
        let effective_timeout = options.timeout_ms.unwrap_or(self.timeout);
        let deadline = effective_timeout
            .effective_ms()
            .map(|ms| Instant::now() + Duration::from_millis(ms));

        let interrupt = Arc::new(AtomicBool::new(false));
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (replies_tx, replies_rx) = std::sync::mpsc::channel();
        let worker_input = WorkerInput {
            code: code.to_owned(),
            tools: tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.clone(),
                        to_codemode_identifier(&tool.name),
                        tool.description.clone().unwrap_or_default(),
                    )
                })
                .collect(),
            globals: globals
                .iter()
                .map(|global| (global.name.clone(), global.spread))
                .collect(),
            store: serialize_store(options.store.as_ref()),
            memory_limit_bytes: self.memory_limit_bytes,
            interrupt: interrupt.clone(),
            events: events_tx,
            replies: replies_rx,
        };
        let handle = std::thread::spawn(move || run_worker(worker_input));

        let calls = Arc::new(Mutex::new(CallTable::default()));
        let mut output: Vec<CodemodeOutputItem> = Vec::new();
        let abort_message = |closed: bool| {
            if closed {
                "Sandbox closed"
            } else {
                "Execution aborted"
            }
        };

        let outcome: CodemodeResult;
        loop {
            let timeout_sleep = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                _ = abort.cancelled() => {
                    interrupt.store(true, Ordering::SeqCst);
                    outcome = CodemodeResult::Err {
                        error: CodemodeError {
                            kind: CodemodeErrorKind::Aborted,
                            name: None,
                            message: abort_message(self.closed.load(Ordering::SeqCst)).to_owned(),
                            stack: None,
                        },
                        output: std::mem::take(&mut output),
                        calls: finish_calls(&calls),
                    };
                    break;
                }
                _ = timeout_sleep => {
                    interrupt.store(true, Ordering::SeqCst);
                    outcome = CodemodeResult::Err {
                        error: CodemodeError {
                            kind: CodemodeErrorKind::Timeout,
                            name: None,
                            message: match effective_timeout.effective_ms() {
                                Some(ms) => format!("Execution timed out after {ms} ms"),
                                None => "Execution timed out".to_owned(),
                            },
                            stack: None,
                        },
                        output: std::mem::take(&mut output),
                        calls: finish_calls(&calls),
                    };
                    break;
                }
                message = events_rx.recv() => {
                    match message {
                        Some(WorkerToHost::Output(item)) => output.push(item),
                        Some(WorkerToHost::Call { id, target, name, args }) => {
                            let source = if target == CallTarget::Tool {
                                lookup_tool(&tools, &name)
                            } else {
                                lookup_tool(&globals, &name)
                            };
                            let is_tool = target == CallTarget::Tool;
                            let child = abort.child_token();
                            {
                                let mut table = lock(&calls);
                                let index = if is_tool {
                                    let index = table.records.len();
                                    table.records.push(CodemodeCall {
                                        name: name.clone(),
                                        status: CodemodeCallStatus::Cancelled,
                                        duration_ms: 0.0,
                                    });
                                    Some(index)
                                } else {
                                    None
                                };
                                table.by_id.insert(id, PendingCall {
                                    index,
                                    started_at: Instant::now(),
                                });
                            }
                            let Some(tool) = source else {
                                fail_call(&calls, &replies_tx, id, &format!(
                                    "Unknown {} \"{name}\"",
                                    if is_tool { "tool" } else { "global" }
                                ));
                                continue;
                            };
                            let parsed = match args {
                                None => Ok(Value::Null),
                                Some(text) => serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()),
                            };
                            let args_value = match parsed {
                                Ok(value) => value,
                                Err(error) => {
                                    fail_call(&calls, &replies_tx, id, &error);
                                    continue;
                                }
                            };
                            let calls_for_task = calls.clone();
                            let replies = replies_tx.clone();
                            let execution = abort.clone();
                            tokio::spawn(async move {
                                let result = (tool.execute)(
                                    args_value,
                                    CodemodeToolContext { signal: child.clone() },
                                )
                                .await;
                                complete_call(&calls_for_task, &replies, id, result, &execution);
                            });
                        }
                        Some(WorkerToHost::Done { ok, value, writes, error }) => {
                            outcome = if ok {
                                CodemodeResult::Ok {
                                    value: value.map(|text| {
                                        serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
                                    }),
                                    output: std::mem::take(&mut output),
                                    calls: finish_calls(&calls),
                                    store_writes: parse_store_writes(writes.as_deref()),
                                }
                            } else {
                                CodemodeResult::Err {
                                    error: parse_script_error(error.as_deref()),
                                    output: std::mem::take(&mut output),
                                    calls: finish_calls(&calls),
                                }
                            };
                            break;
                        }
                        Some(WorkerToHost::Crash { message }) => {
                            outcome = CodemodeResult::Err {
                                error: CodemodeError {
                                    kind: CodemodeErrorKind::Sandbox,
                                    name: None,
                                    message,
                                    stack: None,
                                },
                                output: std::mem::take(&mut output),
                                calls: finish_calls(&calls),
                            };
                            break;
                        }
                        None => {
                            outcome = CodemodeResult::Err {
                                error: CodemodeError {
                                    kind: CodemodeErrorKind::Sandbox,
                                    name: None,
                                    message: "Worker exited before the script settled".to_owned(),
                                    stack: None,
                                },
                                output: std::mem::take(&mut output),
                                calls: finish_calls(&calls),
                            };
                            break;
                        }
                    }
                }
            }
        }

        // Already cancelled by finish(): the record keeps "cancelled" and the
        // worker is gone or going (host.ts:247-250).
        abort.cancel();
        drop(replies_tx);
        drop(handle);
        Ok(outcome)
    }
}

/// Tool lookup by raw name.
fn lookup_tool(tools: &[CodemodeTool], name: &str) -> Option<CodemodeTool> {
    tools.iter().find(|tool| tool.name == name).cloned()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn serialize_store(store: Option<&Value>) -> serde_json::Map<String, Value> {
    let mut serialized = serde_json::Map::new();
    if let Some(Value::Object(entries)) = store {
        for (key, value) in entries {
            serialized.insert(key.clone(), Value::String(value.to_string()));
        }
    }
    serialized
}

/// `parseStoreWrites` (host.ts:53-61): entries of `[key, json]` (set) and
/// `[key]` (delete).
fn parse_store_writes(json: Option<&str>) -> CodemodeStoreWrites {
    let mut writes = CodemodeStoreWrites::default();
    let Some(json) = json else {
        return writes;
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(json) else {
        return writes;
    };
    for entry in entries {
        let Some(entry) = entry.as_array() else {
            continue;
        };
        let Some(key) = entry.first().and_then(Value::as_str) else {
            continue;
        };
        if entry.len() < 2 {
            writes.delete.push(key.to_owned());
        } else if let Some(Value::String(value)) = entry.get(1) {
            // The worker sends `[key, json]` where `json` is the value's JSON
            // text; `parseStoreWrites` parses it back (host.ts:53-61).
            if let Ok(parsed) = serde_json::from_str::<Value>(value) {
                writes.set.insert(key.to_owned(), parsed);
            }
        }
    }
    writes
}

/// `handleDone` error branch (host.ts:216-220): `{name?, message, stack?}`.
fn parse_script_error(error: Option<&str>) -> CodemodeError {
    let parsed = error
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|value| value.as_object().cloned());
    match parsed {
        Some(fields) => CodemodeError {
            kind: CodemodeErrorKind::Script,
            name: fields
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            message: fields
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            stack: fields
                .get("stack")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        None => CodemodeError {
            kind: CodemodeErrorKind::Script,
            name: None,
            message: error.unwrap_or("Unknown script error").to_owned(),
            stack: None,
        },
    }
}

/// Update a call that failed before/without running the tool.
fn fail_call(
    calls: &Arc<Mutex<CallTable>>,
    replies: &std::sync::mpsc::Sender<HostToWorker>,
    id: u32,
    error: &str,
) {
    {
        let mut table = lock(calls);
        if let Some(pending) = table.by_id.remove(&id)
            && let Some(index) = pending.index
        {
            table.records[index].status = CodemodeCallStatus::Error;
            table.records[index].duration_ms = pending.started_at.elapsed().as_secs_f64() * 1000.0;
        }
    }
    let _ = replies.send(HostToWorker::Result {
        id,
        ok: false,
        payload: Some(error.to_owned()),
    });
}

/// Record a completed call and reply to the worker; a call that finished
/// after the execution did is ignored for the record but still replies (the
/// worker may be gone, in which case the send is dropped).
fn complete_call(
    calls: &Arc<Mutex<CallTable>>,
    replies: &std::sync::mpsc::Sender<HostToWorker>,
    id: u32,
    result: Result<Value, String>,
    execution: &CancellationToken,
) {
    let payload;
    let ok;
    {
        let mut table = lock(calls);
        match table.by_id.remove(&id) {
            Some(pending) => {
                let duration = pending.started_at.elapsed().as_secs_f64() * 1000.0;
                if let Some(index) = pending.index {
                    table.records[index].duration_ms = duration;
                    // A call cut off by finish() keeps `cancelled`, even when
                    // the tool surfaces its own cancellation as an error
                    // (host.ts:243-250).
                    match result {
                        Ok(value) if !execution.is_cancelled() => {
                            table.records[index].status = CodemodeCallStatus::Ok;
                            ok = true;
                            payload = Some(value.to_string());
                        }
                        Ok(value) => {
                            table.records[index].status = CodemodeCallStatus::Cancelled;
                            ok = true;
                            payload = Some(value.to_string());
                        }
                        Err(error) if execution.is_cancelled() => {
                            table.records[index].status = CodemodeCallStatus::Cancelled;
                            ok = false;
                            payload = Some(error);
                        }
                        Err(error) => {
                            table.records[index].status = CodemodeCallStatus::Error;
                            ok = false;
                            payload = Some(error);
                        }
                    }
                } else {
                    match result {
                        Ok(value) => {
                            ok = true;
                            payload = Some(value.to_string());
                        }
                        Err(error) => {
                            ok = false;
                            payload = Some(error);
                        }
                    }
                }
            }
            // After finish(), no reply is needed (host.ts:247-250).
            None => return,
        }
    }
    let _ = replies.send(HostToWorker::Result { id, ok, payload });
}

/// `finish()` call bookkeeping: pending calls get their duration and stay
/// `cancelled`; running records are snapshotted.
fn finish_calls(calls: &Arc<Mutex<CallTable>>) -> Vec<CodemodeCall> {
    let mut table = lock(calls);
    let now = Instant::now();
    let pending: Vec<(Option<usize>, Instant)> = table
        .by_id
        .values()
        .map(|call| (call.index, call.started_at))
        .collect();
    for (index, started_at) in pending {
        if let Some(index) = index {
            table.records[index].duration_ms =
                now.duration_since(started_at).as_secs_f64() * 1000.0;
        }
    }
    table.by_id.clear();
    table.records.clone()
}
