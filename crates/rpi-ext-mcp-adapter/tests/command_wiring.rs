//! TE20 command wiring end-to-end (R7.2.1.1–.4, [RPI-OWN]).
//!
//! Drives the real `install` → `registerCommand` → `dispatch` path with an
//! in-process fake host (no mocked internals), covering task §4.2 A1–A10:
//!
//! - A1/A2 status/tools text in a no-UI session (print/json contract);
//! - A3/A4 enable/disable project override + `/reload` guidance;
//! - A5/A6 `/mcp-auth` headless no-hang + no `ui.select` without UI;
//! - A7 TUI `status` uses `ui.select`, other subcommands stay text;
//! - A8 registration surface (asserted in `manifest_capabilities.rs`);
//! - A9 unknown subcommand/command name → error result, no panic/hang;
//! - A10 writes land in `<cwd>/.rpi/mcp.json`, never `~/.pi`/`.pi`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use rpi_ext_mcp_adapter::{dispatch, dispatcher_for_test, install_for_test};
use serde_json::{json, Value};

/// The byte-exact `/mcp status` baseline for the fixture config below; the
/// same literal is asserted against `commands::format_status_text` in its
/// unit test (`format_status_text_known_baseline`), so A1 ties the dispatch
/// output to the pure-function baseline.
const STATUS_BASELINE: &str = "MCP Server Status:\n\n○ demo: not connected\n⊘ off: disabled (run /mcp enable off, then /reload)\n○ oauth-demo: not connected";

struct FakeHost {
    cwd: Mutex<String>,
    has_ui: Mutex<bool>,
    mode: Mutex<String>,
    ui_calls: Mutex<Vec<Value>>,
    registered_commands: Mutex<Vec<String>>,
}

impl FakeHost {
    fn new(cwd: &str) -> Arc<Self> {
        Arc::new(Self {
            cwd: Mutex::new(cwd.to_string()),
            has_ui: Mutex::new(false),
            mode: Mutex::new("print".to_string()),
            ui_calls: Mutex::new(Vec::new()),
            registered_commands: Mutex::new(Vec::new()),
        })
    }

    fn set_ui(&self, has_ui: bool, mode: &str) {
        *self.has_ui.lock().unwrap_or_else(|e| e.into_inner()) = has_ui;
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode.to_string();
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

    fn registered_commands(&self) -> Vec<String> {
        self.registered_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        m if m.starts_with("ui.") => {
            host.ui_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(json!({"call": m, "args": args}));
            match m {
                "ui.select" => json!({"ok": "Close"}),
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
        "rpi-mcp-command-wiring-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn dispatch_command(name: &str, args: &str) -> Value {
    let message = json!({"kind": "command", "name": name, "args": args});
    let bytes = serde_json::to_vec(&message).expect("json");
    // `dispatch` ignores the cookie (it is only an opaque host handle).
    let response = dispatch(std::ptr::null(), RVec::from(bytes));
    serde_json::from_slice(&response[..]).expect("command result JSON")
}

/// Run a command on a fresh thread and fail if it does not return within the
/// bound (A5/A9 no-hang assertions; the real host also dispatches native
/// plugins from a blocking-pool thread).
fn dispatch_command_with_timeout(name: &str, args: &str, timeout: Duration) -> Value {
    let thread_name = name.to_string();
    let thread_args = args.to_string();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = dispatch_command(&thread_name, &thread_args);
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(timeout)
        .unwrap_or_else(|_| panic!("command `{name} {args}` did not return within {timeout:?}"))
}

fn result_text(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing text content: {result}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_wiring_end_to_end() {
    let dir = temp_dir("e2e");
    let agent_dir = dir.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    // Fixture config: one lazy server, one disabled server, one OAuth HTTP
    // server. No eager/keep-alive server, so init performs no connections.
    std::fs::write(
        dir.join(".mcp.json"),
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "demo": { "command": "node", "lifecycle": "lazy" },
                "off": { "command": "node", "disabled": true },
                "oauth-demo": {
                    "url": "http://127.0.0.1:9/mcp",
                    "auth": "oauth",
                    "lifecycle": "lazy"
                }
            }
        }))
        .expect("json"),
    )
    .expect("write .mcp.json");

    // Isolation: config discovery + metadata cache + direct-tools env.
    let saved_home = std::env::var_os("HOME");
    let saved_agent_dir = std::env::var_os("RPI_CODING_AGENT_DIR");
    let saved_direct_tools = std::env::var_os("MCP_DIRECT_TOOLS");
    std::env::set_var("HOME", &dir);
    std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
    std::env::remove_var("MCP_DIRECT_TOOLS");
    // Pre-create the metadata cache: its absence flips init into
    // `bootstrap-all` (connects every server, including lazy ones); an empty
    // cache keeps init to eager/keep-alive servers only (none here).
    std::fs::write(
        agent_dir.join("mcp-cache.json"),
        json!({"version": 1, "servers": {}}).to_string(),
    )
    .expect("seed metadata cache");

    let host = FakeHost::new(&dir.to_string_lossy());
    let calls = RpiHostCalls {
        call: fake_host_call,
    };
    let installed = install_for_test(calls, Arc::into_raw(host.clone()) as PluginCookie);
    assert_eq!(installed, json!({"ok": true}), "install must succeed");

    // A8 (install half): both commands registered on the ABI.
    assert_eq!(
        host.registered_commands(),
        vec!["mcp".to_string(), "mcp-auth".to_string()]
    );

    // Drive init to Ready (lazy-only config: no connections).
    let dispatcher = dispatcher_for_test().expect("plugin state");
    dispatcher.start_init(dir.clone(), None);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while dispatcher.try_runtime().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "MCP init must complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // ---- A1: `/mcp status` in a no-UI session returns the pure-function text.
    let result = dispatch_command_with_timeout("mcp", "status", Duration::from_secs(5));
    assert!(result.get("isError").is_none(), "status: {result}");
    assert_eq!(result_text(&result), STATUS_BASELINE);
    // A6: no `ui.select` host call without a UI.
    assert!(
        host.ui_calls("ui.select").is_empty(),
        "hasUI=false must not open a select dialog"
    );

    // ---- A2: `/mcp tools` text (no metadata → upstream empty text).
    let result = dispatch_command_with_timeout("mcp", "tools", Duration::from_secs(5));
    assert_eq!(result_text(&result), "No MCP tools available");

    // ---- A3/A4: disable writes `<cwd>/.rpi/mcp.json` and points at /reload.
    let override_path = dir.join(".rpi").join("mcp.json");
    assert!(!override_path.exists());
    // Pre-write the override with unknown fields so the end-to-end path
    // exercises read-modify-write preservation (the pure function's
    // "creates file when missing" branch is unit-tested separately).
    std::fs::create_dir_all(override_path.parent().expect("parent")).expect("override dir");
    std::fs::write(
        &override_path,
        serde_json::to_string_pretty(&json!({
            "mcpServers": { "demo": { "command": "node", "custom": 7 } },
            "settings": { "toolPrefix": "mcp" },
            "customField": 42
        }))
        .expect("json"),
    )
    .expect("seed override");
    let result = dispatch_command_with_timeout("mcp", "disable demo", Duration::from_secs(5));
    let text = result_text(&result).to_string();
    assert!(
        text.contains("Disabled server \"demo\""),
        "disable text: {text}"
    );
    assert!(
        text.contains("— run /reload to apply"),
        "disable text: {text}"
    );
    assert_eq!(result["details"]["changed"], json!(true));
    assert_eq!(result["details"]["disabled"], json!(true));
    assert_eq!(
        result["details"]["path"],
        json!(override_path.to_string_lossy())
    );
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&override_path).expect("override exists"))
            .expect("override JSON");
    assert_eq!(written["mcpServers"]["demo"]["disabled"], json!(true));
    // Unknown-field preservation (pure function contract, asserted end-to-end).
    assert_eq!(written["mcpServers"]["demo"]["command"], json!("node"));
    assert_eq!(written["mcpServers"]["demo"]["custom"], json!(7));
    assert_eq!(written["settings"]["toolPrefix"], json!("mcp"));
    assert_eq!(written["customField"], json!(42));
    // A4: idempotent second call reports the already-disabled state.
    let result = dispatch_command_with_timeout("mcp", "disable demo", Duration::from_secs(5));
    assert!(
        result_text(&result).contains("already disabled"),
        "second disable: {result}"
    );
    // enable removes the flag again (FR-D round trip), preserving the rest.
    let result = dispatch_command_with_timeout("mcp", "enable demo", Duration::from_secs(5));
    assert!(
        result_text(&result).contains("Enabled server \"demo\""),
        "enable text: {result}"
    );
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&override_path).expect("override exists"))
            .expect("override JSON");
    assert!(written["mcpServers"]["demo"].get("disabled").is_none());
    assert_eq!(written["mcpServers"]["demo"]["command"], json!("node"));
    assert_eq!(written["mcpServers"]["demo"]["custom"], json!(7));

    // A9: unknown subcommand and unknown command name return error results.
    let result = dispatch_command_with_timeout("mcp", "setup", Duration::from_secs(5));
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["details"]["error"], json!("unknown_subcommand"));
    assert!(result_text(&result).contains("Usage: /mcp"));
    let result = dispatch_command_with_timeout("mcp-unknown", "", Duration::from_secs(5));
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["details"]["error"], json!("unknown_command"));
    // Missing target for enable/disable/logout/auth → usage error, no panic.
    for (name, args) in [
        ("mcp", "disable"),
        ("mcp", "enable"),
        ("mcp", "logout"),
        ("mcp-auth", ""),
    ] {
        let result = dispatch_command_with_timeout(name, args, Duration::from_secs(5));
        assert_eq!(result["isError"], json!(true), "{name} {args}: {result}");
        assert_eq!(result["details"]["error"], json!("invalid_args"));
    }

    // FR-B subcommand coverage: reconnect/logout not-found and disabled paths.
    let result = dispatch_command_with_timeout("mcp", "reconnect missing", Duration::from_secs(5));
    assert_eq!(result["details"]["error"], json!("server_not_found"));
    let result = dispatch_command_with_timeout("mcp", "reconnect off", Duration::from_secs(5));
    assert!(
        result_text(&result).contains("is disabled"),
        "disabled reconnect: {result}"
    );
    let result = dispatch_command_with_timeout("mcp", "logout missing", Duration::from_secs(5));
    assert_eq!(result["details"]["error"], json!("server_not_found"));
    let result = dispatch_command_with_timeout("mcp", "logout demo", Duration::from_secs(5));
    assert!(
        result_text(&result).contains("OAuth credentials cleared for \"demo\""),
        "logout text: {result}"
    );

    // ---- A5: `/mcp-auth` without a UI returns guidance immediately.
    let result = dispatch_command_with_timeout("mcp-auth", "oauth-demo", Duration::from_secs(5));
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["details"]["error"], json!("no_ui"));
    assert!(
        result_text(&result).contains("interactive session"),
        "headless auth text: {result}"
    );
    assert!(host.ui_calls("ui.select").is_empty());

    // ---- A7: with a TUI, `status` uses `ui.select`; other subcommands stay text.
    host.set_ui(true, "tui");
    let result = dispatch_command_with_timeout("mcp", "status", Duration::from_secs(5));
    assert_eq!(result_text(&result), STATUS_BASELINE);
    assert_eq!(
        host.ui_calls("ui.select").len(),
        1,
        "TUI status must open the select dialog"
    );
    let select_args = host.ui_calls("ui.select")[0]["args"].clone();
    assert_eq!(select_args["title"], json!("MCP Server Status"));
    let options: Vec<&str> = select_args["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(options.contains(&"Close"), "options: {options:?}");
    assert!(
        options
            .iter()
            .any(|line| line.contains("demo: not connected")),
        "options must show the status lines: {options:?}"
    );

    let notify_before = host.ui_calls("ui.notify").len();
    let result = dispatch_command_with_timeout("mcp", "tools", Duration::from_secs(5));
    assert_eq!(result_text(&result), "No MCP tools available");
    assert_eq!(
        host.ui_calls("ui.select").len(),
        1,
        "tools must not open another select"
    );
    assert!(
        host.ui_calls("ui.notify").len() > notify_before,
        "tools must notify its text in the TUI"
    );

    // ---- A10: no `~/.pi` / `.pi` reads or writes; the override is the only
    // artifact and it lives under `<cwd>/.rpi`.
    assert!(!dir.join(".pi").exists(), "must not create .pi");
    assert!(!agent_dir.join(".pi").exists(), "must not create agent .pi");
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .expect("cwd readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert!(
        entries.contains(&".rpi".to_string()),
        "override dir present: {entries:?}"
    );

    // Env restore.
    match saved_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match saved_agent_dir {
        Some(value) => std::env::set_var("RPI_CODING_AGENT_DIR", value),
        None => std::env::remove_var("RPI_CODING_AGENT_DIR"),
    }
    match saved_direct_tools {
        Some(value) => std::env::set_var("MCP_DIRECT_TOOLS", value),
        None => std::env::remove_var("MCP_DIRECT_TOOLS"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
