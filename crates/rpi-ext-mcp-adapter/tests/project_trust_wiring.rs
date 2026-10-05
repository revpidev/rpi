//! #681 integration wiring: session_start gates PROJECT-scope MCP servers on
//! host project trust + the per-server approval store, and surface the block
//! reason through `/mcp status` and the disabled-call result.
//!
//! Runs in its own test binary: the plugin state is a process-global
//! `OnceLock`, so install/dispatch scenarios must not share a process with
//! other install-driven test files.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use abi_stable::std_types::RVec;
use rpi_ext_host::native::PluginCookie;
use rpi_ext_mcp_adapter::{dispatch, dispatcher_for_test, install_for_test};
use serde_json::{Value, json};

struct FakeHost {
    cwd: Mutex<String>,
    has_ui: Mutex<bool>,
    mode: Mutex<String>,
    project_trusted: Mutex<bool>,
    confirm_answer: Mutex<bool>,
    ui_calls: Mutex<Vec<Value>>,
}

impl FakeHost {
    fn new(cwd: &str) -> Arc<Self> {
        Arc::new(Self {
            cwd: Mutex::new(cwd.to_string()),
            has_ui: Mutex::new(false),
            mode: Mutex::new("print".to_string()),
            project_trusted: Mutex::new(false),
            confirm_answer: Mutex::new(true),
            ui_calls: Mutex::new(Vec::new()),
        })
    }

    fn set_ui(&self, has_ui: bool, mode: &str) {
        *self.has_ui.lock().unwrap_or_else(|e| e.into_inner()) = has_ui;
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode.to_string();
    }

    fn set_trusted(&self, trusted: bool) {
        *self
            .project_trusted
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = trusted;
    }

    fn set_confirm(&self, answer: bool) {
        *self
            .confirm_answer
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = answer;
    }

    fn ui_calls(&self, method: &str) -> Vec<Value> {
        self.ui_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|call| call.get("call").and_then(Value::as_str) == Some(method))
            .cloned()
            .collect()
    }
}

extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // SAFETY: the Arc handed to install_for_test is kept alive by the test
    // for the whole process; from_raw + forget only re-borrows it.
    let host = unsafe { Arc::from_raw(host_ptr as *const FakeHost) };
    let request: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = request.get("call").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "ctx.cwd" => json!({"ok": *host.cwd.lock().unwrap_or_else(|e| e.into_inner())}),
        "ctx.hasUI" => json!({"ok": *host.has_ui.lock().unwrap_or_else(|e| e.into_inner())}),
        "ctx.mode" => json!({"ok": *host.mode.lock().unwrap_or_else(|e| e.into_inner())}),
        "ctx.isProjectTrusted" => {
            json!({"ok": *host.project_trusted.lock().unwrap_or_else(|e| e.into_inner())})
        }
        "getFlag" => json!({"ok": null}),
        "on" | "registerFlag" | "registerTool" | "unregisterTool" | "setActiveTools"
        | "registerCommand" | "events.emit" => json!({"ok": true}),
        "getActiveTools" | "getAllTools" => json!({"ok": []}),
        m if m.starts_with("ui.") => {
            host.ui_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(json!({"call": m, "args": args}));
            match m {
                "ui.confirm" => {
                    json!({"ok": *host.confirm_answer.lock().unwrap_or_else(|e| e.into_inner())})
                }
                _ => json!({"ok": null}),
            }
        }
        _ => json!({"ok": null}),
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    std::mem::forget(host);
    RVec::from(bytes)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-project-trust-{}-{}-{}",
        tag,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn result_text(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing text content: {result}"))
}

fn session_start() {
    // The plugin bridges host dispatch through its own tokio runtime; run the
    // dispatch on a fresh thread so the test's async runtime context does not
    // nest `block_on` calls (the command_wiring helper does the same).
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let message = json!({"kind": "event", "event": "session_start"});
        let bytes = serde_json::to_vec(&message).expect("json");
        let response = dispatch(std::ptr::null(), RVec::from(bytes));
        let _ = sender.send(response.to_vec());
    });
    receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("session_start dispatch returns");
}

fn status_text(timeout: Duration) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let message = json!({"kind": "command", "name": "mcp", "args": "status"});
        let bytes = serde_json::to_vec(&message).expect("json");
        let response = dispatch(std::ptr::null(), RVec::from(bytes));
        let result: Value = serde_json::from_slice(&response[..]).expect("command result JSON");
        let _ = sender.send(result);
    });
    let result = receiver
        .recv_timeout(timeout)
        .unwrap_or_else(|_| panic!("/mcp status did not return within {timeout:?}"));
    result_text(&result).to_string()
}

fn project_config(project: &std::path::Path, command: &str) {
    std::fs::write(
        project.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "proj": { "command": command, "lifecycle": "lazy" }
            }
        }))
        .expect("json"),
    )
    .expect("write .mcp.json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_start_gates_project_servers_on_trust_and_approval() {
    let dir = temp_dir("gate");
    let project = dir.join("proj");
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    project_config(&project, "node");

    let saved_home = std::env::var_os("HOME");
    let saved_agent = std::env::var_os("RPI_CODING_AGENT_DIR");
    rpi_test_env::set_var("HOME", &dir);
    rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
    // Pre-create the metadata cache: its absence flips init into
    // `bootstrap-all`, which would eagerly spawn the lazy fixture command.
    std::fs::write(
        agent_dir.join("mcp-cache.json"),
        json!({"version": 1, "servers": {}}).to_string(),
    )
    .expect("seed metadata cache");

    let host = FakeHost::new(&project.to_string_lossy());
    let calls = rpi_ext_host::native::RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, Arc::into_raw(host.clone()) as PluginCookie);
    assert_eq!(installed, json!({"ok": true}), "install must succeed");

    // Read the text surface without UI so `/mcp status` never enters the
    // panel branch (the panel path is covered by command_wiring).
    let read_status = || {
        host.set_ui(false, "print");
        status_text(Duration::from_secs(10))
    };

    // 1. Untrusted project: no prompt, blocked, reason surfaced verbatim.
    host.set_ui(true, "tui");
    host.set_trusted(false);
    session_start();
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let dispatcher = dispatcher_for_test().expect("plugin state");
        while dispatcher.try_runtime().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "init must reach Ready within 10s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let text = read_status();
    assert!(
        text.contains("⊘ proj: blocked by project trust"),
        "untrusted block reason in status: {text}"
    );
    assert!(
        host.ui_calls("ui.confirm").is_empty(),
        "untrusted projects never prompt"
    );
    assert!(
        !agent_dir.join("mcp-project-approvals.json").exists(),
        "no approval is recorded without consent"
    );

    // 2. Trusted + UI: the prompt is shown and the approval is remembered;
    // the server loads on the next init.
    host.set_ui(true, "tui");
    host.set_trusted(true);
    host.set_confirm(true);
    session_start();
    let text = read_status();
    assert!(
        !text.contains("⊘ proj"),
        "approved server is not blocked: {text}"
    );
    assert!(
        text.contains("○ proj: not connected"),
        "approved lazy server appears in status: {text}"
    );
    let confirms = host.ui_calls("ui.confirm");
    assert_eq!(confirms.len(), 1, "one approval prompt: {confirms:?}");
    let confirm_args = &confirms[0]["args"];
    assert_eq!(
        confirm_args["title"],
        json!("Allow project MCP server “proj”?")
    );
    assert!(
        confirm_args["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Project config:")
    );
    let store = std::fs::read_to_string(agent_dir.join("mcp-project-approvals.json"))
        .expect("approval store written");
    let store: Value = serde_json::from_str(&store).expect("store JSON");
    assert_eq!(store["version"], json!(1));
    let record = &store["approvals"][0];
    assert_eq!(record["serverName"], json!("proj"));
    let approved_hash = record["definitionHash"]
        .as_str()
        .expect("definition hash")
        .to_string();
    assert_eq!(
        record["projectRoot"]
            .as_str()
            .map(|root| root.ends_with("proj")),
        Some(true),
        "scope keys the canonical project root: {record}"
    );
    assert!(!approved_hash.is_empty());

    // 3. A later non-interactive trusted session reuses the approval (no
    // prompt) and still loads the server.
    let prompts_before = host.ui_calls("ui.confirm").len();
    host.set_ui(false, "print");
    host.set_trusted(true);
    session_start();
    let text = read_status();
    assert!(
        text.contains("○ proj: not connected"),
        "approved reuse: {text}"
    );
    assert_eq!(
        host.ui_calls("ui.confirm").len(),
        prompts_before,
        "an approved definition needs no new prompt"
    );

    // 4. A changed definition invalidates the approval: a non-interactive
    // trusted session blocks with the approval-required reason.
    project_config(&project, "node-changed");
    session_start();
    let text = read_status();
    assert!(
        text.contains("⊘ proj: blocked: project server approval required"),
        "stale approval blocks with approval-required: {text}"
    );

    // 5. An interactive trusted session re-prompts and updates the record.
    host.set_ui(true, "tui");
    host.set_confirm(true);
    session_start();
    let text = read_status();
    assert!(
        text.contains("○ proj: not connected"),
        "re-approval loads: {text}"
    );
    let store =
        std::fs::read_to_string(agent_dir.join("mcp-project-approvals.json")).expect("store");
    let store: Value = serde_json::from_str(&store).expect("store JSON");
    assert_eq!(store["approvals"].as_array().map(Vec::len), Some(1));
    assert_ne!(
        store["approvals"][0]["definitionHash"].as_str(),
        Some(approved_hash.as_str()),
        "the re-approval stores the new definition hash"
    );

    // 6. Denying the prompt blocks with the denied reason.
    project_config(&project, "node-denied");
    host.set_ui(true, "tui");
    host.set_confirm(false);
    session_start();
    let text = read_status();
    assert!(
        text.contains("⊘ proj: blocked: project server approval denied"),
        "denial reason in status: {text}"
    );

    if let Some(dispatcher) = dispatcher_for_test() {
        dispatcher.shutdown().await;
    }
    match saved_home {
        Some(home) => rpi_test_env::set_var("HOME", home),
        None => rpi_test_env::remove_var("HOME"),
    }
    match saved_agent {
        Some(agent) => rpi_test_env::set_var("RPI_CODING_AGENT_DIR", agent),
        None => rpi_test_env::remove_var("RPI_CODING_AGENT_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
