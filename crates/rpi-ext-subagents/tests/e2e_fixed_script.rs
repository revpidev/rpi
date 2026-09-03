//! E2E with a fixed child script (design §5.3): the in-process plugin drives a
//! `rpi --mode json -p`-shaped child end to end — spawn contract (argv/env),
//! event parsing, artifacts, fork, depth blocking, timeout ladder and process
//! reclamation. One `#[test]` per binary: the plugin state is a OnceLock and
//! the scenarios share the process environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{json, Value};

struct FakeHost {
    cwd: PathBuf,
    model: Value,
    /// Authoritative `ctx.sessionFile` answer (V13-02): `Some` = served as
    /// `{"path", "id"}`, `None` = host call error (fallback path). Scenario
    /// 3 and the FR-D dual-instance scenario set it before forking.
    session_file: std::sync::Mutex<Option<Value>>,
}

/// TE09: host-call observation sinks — `toolUpdate` partial frames and
/// `sendMessage` custom messages, collected for the streaming/notify
/// assertions (static because the host trampoline is a plain extern "C" fn).
static TOOL_UPDATES: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
static SENT_MESSAGES: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
/// Full `sendMessage` call args (message + options) — the triggerTurn
/// assertion needs the options half the message-only sink drops.
static SENT_MESSAGE_CALLS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
/// TE11 FR-C: `ui.setWidget` call args — the fleet strip's push/remove
/// assertions (content `null` = removal).
static SET_WIDGET_CALLS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

fn take_tool_updates() -> Vec<Value> {
    TOOL_UPDATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

fn take_sent_messages() -> Vec<Value> {
    SENT_MESSAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

fn take_sent_message_calls() -> Vec<Value> {
    SENT_MESSAGE_CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

fn take_set_widget_calls() -> Vec<Value> {
    SET_WIDGET_CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

// Safety: the trampoline only dereferences the cookie as `&FakeHost`.
extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    let host = unsafe { &*(host_ptr as *const FakeHost) };
    let parsed: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = parsed.get("call").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "registerTool" | "registerCommand" | "registerMessageRenderer" | "on" | "registerFlag" => {
            json!({ "ok": true })
        }
        "ctx.cwd" => json!({ "ok": host.cwd.to_string_lossy() }),
        "ctx.sessionFile" => match host
            .session_file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(info) => json!({ "ok": info }),
            None => json!({ "error": { "message": "no authoritative session" } }),
        },
        "ctx.model" => json!({ "ok": host.model }),
        "toolUpdate" => {
            TOOL_UPDATES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(parsed["args"].clone());
            json!({ "ok": true })
        }
        "sendMessage" => {
            SENT_MESSAGES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(parsed["args"]["message"].clone());
            SENT_MESSAGE_CALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(parsed["args"].clone());
            json!({ "ok": true })
        }
        "ui.setWidget" => {
            SET_WIDGET_CALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(parsed["args"].clone());
            json!({ "ok": true })
        }
        _ => json!({ "error": { "kind": "unknownMethod", "message": method } }),
    };
    RVec::from(serde_json::to_vec(&response).unwrap_or_default())
}

struct Sandbox {
    root: PathBuf,
    project: PathBuf,
    dump_root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() % 1_000_000_000)
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("rpi-sub-e2e-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("proj");
        let agent_dir = root.join("agent");
        let dump_root = root.join("dumps");
        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        // TE05: asyncByDefault defaults to true (upstream parity), which
        // would flip every P0 scenario to background mode — pin the P0
        // foreground semantics in the sandbox config; async gets its own
        // scenarios below.
        std::fs::create_dir_all(agent_dir.join("extensions").join("subagent")).unwrap();
        std::fs::write(
            agent_dir
                .join("extensions")
                .join("subagent")
                .join("config.json"),
            r#"{"asyncByDefault": false}"#,
        )
        .unwrap();
        std::fs::create_dir_all(&dump_root).unwrap();
        std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
        std::env::set_var("RPI_SUBAGENT_RPI_BINARY", fixed_child_binary());
        std::env::set_var(
            "RPI_SUBAGENT_EXTENSION_PATH",
            "/opt/rpi-ext/librpi_ext_subagents.so",
        );
        let _ = &agent_dir;
        Self {
            root,
            project,
            dump_root,
        }
    }

    fn dump(&self, name: &str) -> PathBuf {
        let dir = self.dump_root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

/// Locate the fixed-child helper binary (cargo exposes CARGO_BIN_EXE_* to
/// test processes at runtime; fall back to the default target layout).
fn fixed_child_binary() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rpi-subagents-fixed-child") {
        return path;
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .unwrap_or_else(|_| format!("{}/../../target", env!("CARGO_MANIFEST_DIR")));
    format!("{target_dir}/debug/rpi-subagents-fixed-child")
}

fn execute(params: Value) -> Value {
    rpi_ext_subagents::execute_for_test(&params)
}

fn wait_for_process_exit(pid: u32) -> bool {
    for _ in 0..100 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

fn assert_no_rpi_subagent_children() -> bool {
    // Best-effort: any fixed-child process still alive?
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read_to_string(entry.path().join("cmdline")) {
            if cmdline.contains("rpi-subagents-fixed-child") {
                return false;
            }
        }
    }
    true
}

#[test]
fn e2e_fixed_child_full_pipeline() {
    let sandbox = Sandbox::new();
    let host = Arc::new(FakeHost {
        cwd: sandbox.project.clone(),
        model: json!({ "provider": "faux", "id": "faux-1" }),
        session_file: std::sync::Mutex::new(None),
    });
    let response = rpi_ext_subagents::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host.clone()) as PluginCookie,
    );
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)), "{response}");

    // ---- Scenario 1: plain foreground run (scout) ----------------------
    let dump = sandbox.dump("ok");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
    std::env::set_var("RPI_E2E_MODE", "ok");
    let result =
        execute(json!({ "agent": "scout", "task": "map the codebase", "timeoutMs": 30000 }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "Fixed child result: analysis complete");
    let run_id = result["details"]["runId"].as_str().unwrap().to_string();
    assert_eq!(run_id.len(), 8, "{run_id}");
    let single = &result["details"]["results"][0];
    assert_eq!(single["agent"], "scout");
    assert_eq!(single["exitCode"], 0);
    assert_eq!(single["task"], "[prompt redacted]");
    assert_eq!(single["context"], "fresh");
    assert_eq!(single["usage"]["input"], 100);
    assert_eq!(single["usage"]["cost"], 0.42);
    assert_eq!(single["model"], "faux/fixed-1");
    assert_eq!(result["details"]["timeoutMs"], 30000);

    // argv dump: launch contract for a fresh scout run.
    let argv = std::fs::read_to_string(dump.join("argv.txt")).unwrap();
    let argv_lines: Vec<&str> = argv.lines().skip(1).collect(); // argv[0] is the program
    let find_flag = |flag: &str| argv_lines.iter().position(|a| *a == flag);
    assert_eq!(argv_lines[0], "--mode");
    assert_eq!(argv_lines[1], "json");
    assert_eq!(argv_lines[2], "-p");
    let session_dir_pos = find_flag("--session-dir").expect("fresh uses --session-dir");
    assert!(argv_lines[session_dir_pos + 1].ends_with("/run-0"));
    // scout: thinking low, model inherited from the session (faux/1).
    let model_pos = find_flag("--model").expect("model inherited");
    assert_eq!(argv_lines[model_pos + 1], "faux/faux-1:low");
    // tools allowlist is the scout frontmatter list, single comma flag;
    // TE05: intercomBridge (default always) appends contact_supervisor
    // (FR-P1-10 applyIntercomBridgeToAgent).
    let tools_pos = find_flag("--tools").expect("scout declares tools");
    assert_eq!(
        argv_lines[tools_pos + 1],
        "read,grep,find,ls,bash,write,intercom,contact_supervisor"
    );
    // context files inherited (no --no-context-files), skills not (--no-skills).
    assert!(find_flag("--no-context-files").is_none());
    assert!(find_flag("--no-skills").is_some());
    // system prompt temp file with the active_agent tag + boundary block.
    assert!(
        find_flag("--system-prompt").is_some(),
        "replace mode uses --system-prompt"
    );
    // The temp file is cleaned up after the run; the child snapshots it.
    let prompt = std::fs::read_to_string(dump.join("prompt.md")).unwrap();
    assert!(prompt.starts_with("<active_agent name=\"scout\"/>\n\n"));
    assert!(prompt.contains("You are a child subagent, not the parent orchestrator."));
    assert!(
        prompt.contains("You are a scouting subagent running inside"),
        "{prompt}"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(dump.join("prompt.mode"))
            .unwrap()
            .trim(),
        "600"
    );
    // self extension injected; no --no-extensions (scout declares none).
    let extension_positions: Vec<usize> = argv_lines
        .iter()
        .enumerate()
        .filter(|(_, a)| **a == "--extension")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(extension_positions.len(), 1);
    assert_eq!(
        argv_lines[extension_positions[0] + 1],
        "/opt/rpi-ext/librpi_ext_subagents.so"
    );
    assert!(find_flag("--no-extensions").is_none());
    // task text rides argv under the 8000-char limit.
    assert!(argv_lines
        .last()
        .unwrap()
        .starts_with("Task: map the codebase"));

    // env dump: the child-side contract.
    let env_text = std::fs::read_to_string(dump.join("env.txt")).unwrap();
    for expected in [
        "RPI_SUBAGENT_CHILD=1",
        "RPI_SUBAGENT_FANOUT_CHILD=0",
        "RPI_SUBAGENT_DEPTH=1",
        "RPI_SUBAGENT_MAX_DEPTH=2",
        "RPI_SUBAGENT_INHERIT_PROJECT_CONTEXT=1",
        "RPI_SUBAGENT_INHERIT_SKILLS=0",
        "RPI_SUBAGENT_CHILD_AGENT=scout",
        "RPI_SUBAGENT_CHILD_INDEX=0",
        "MCP_DIRECT_TOOLS=__none__",
    ] {
        assert!(
            env_text.contains(expected),
            "missing {expected} in:\n{env_text}"
        );
    }
    assert!(env_text.contains(&format!("RPI_SUBAGENT_RUN_ID={run_id}")));
    // TE05: intercomBridge (default always) appends contact_supervisor to the
    // required list (FR-P1-10).
    assert!(env_text.contains("RPI_SUBAGENT_REQUIRED_TOOLS=[\"read\",\"grep\",\"find\",\"ls\",\"bash\",\"write\",\"intercom\",\"contact_supervisor\"]"));

    // artifacts trail (project mode).
    let artifacts_dir = sandbox.project.join(".rpi/subagents/artifacts");
    let base = artifacts_dir.join(format!("{run_id}_scout_0"));
    assert!(base
        .parent()
        .unwrap()
        .join(format!("{run_id}_scout_0_input.md"))
        .exists());
    let output_content = std::fs::read_to_string(
        base.parent()
            .unwrap()
            .join(format!("{run_id}_scout_0_output.md")),
    )
    .unwrap();
    assert_eq!(output_content, "Fixed child result: analysis complete");
    assert!(base
        .parent()
        .unwrap()
        .join(format!("{run_id}_scout_0_transcript.jsonl"))
        .exists());
    // FR-P0-09 fifth artifact: the raw child event stream, on by default
    // (`artifactConfig.includeJsonl !== false`, execution.ts:1517-1519).
    assert!(base
        .parent()
        .unwrap()
        .join(format!("{run_id}_scout_0.jsonl"))
        .exists());
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(
            base.parent()
                .unwrap()
                .join(format!("{run_id}_scout_0_meta.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(meta["runId"], run_id.as_str());
    assert_eq!(meta["agent"], "scout");
    assert_eq!(meta["task"], "[prompt redacted]");
    assert_eq!(meta["exitCode"], 0);
    assert_eq!(meta["usage"]["input"], 100);
    assert_eq!(meta["usage"]["cost"], 0.42);
    assert_eq!(meta["model"], "faux/fixed-1");

    // ---- Scenario 2: fork fail-fast without a parent session -----------
    let result = execute(
        json!({ "agent": "worker", "task": "t", "context": "fork", "timeoutMs": 5000, "artifacts": false }),
    );
    assert_eq!(result["isError"], Value::Bool(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Forked subagent context requires a persisted parent session."),
        "{text}"
    );

    // ---- Scenario 3: fork from a persisted parent session --------------
    let sessions = sandbox.root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let parent_session = sessions.join("20260814_parent-abc12345.jsonl");
    let real_project = sandbox.project.canonicalize().unwrap();
    std::fs::write(
        &parent_session,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"abc12345","timestamp":"2026-08-14T00:00:00.000Z","cwd":"{}"}}"#, "\n",
                r#"{{"type":"message","id":"id000001","timestamp":"2026-08-14T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"plan please"}}]}}}}"#, "\n",
                r#"{{"type":"message","id":"id000002","timestamp":"2026-08-14T00:00:02.000Z","message":{{"role":"assistant","provider":"anthropic","content":[{{"type":"thinking","thinking":"h","signature":"sig"}},{{"type":"text","text":"the plan"}}]}}}}"#, "\n",
                r#"{{"type":"message","id":"id000003","timestamp":"2026-08-14T00:00:03.000Z","message":{{"role":"toolResult","toolName":"subagent","content":[]}}}}"#, "\n",
                r#"{{"type":"message","id":"id000004","timestamp":"2026-08-14T00:00:04.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"t1","name":"subagent","arguments":{{}}}},{{"type":"text","text":"delegating"}}]}}}}"#, "\n"
            ),
            real_project.to_string_lossy()
        ),
    )
    .unwrap();
    // Point the plugin at the sessions dir via project settings, and serve
    // the authoritative `ctx.sessionFile` (V13-02): scenario 3 exercises the
    // FR-A main path — fork from the authoritative parent, not the dir
    // heuristic (the fabricated stem below is not host-shape anyway).
    std::fs::write(
        sandbox.project.join(".rpi/settings.json"),
        json!({ "sessionDir": sessions.to_string_lossy() }).to_string(),
    )
    .unwrap();
    *host.session_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(json!({
        "path": parent_session.to_string_lossy(),
        "id": "abc12345",
    }));

    let dump = sandbox.dump("fork");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
    let result = execute(json!({
        "agent": "delegate",
        "task": "continue from the fork",
        "context": "fork",
        "timeoutMs": 30000,
        "artifacts": false,
        "sessionDir": sessions.join("forkruns").to_string_lossy(),
    }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    // Branch file created and filtered: no subagent toolResult, no toolCall
    // block, signed thinking stripped, thinking-off entry appended.
    let fork_root = sessions.join("forkruns");
    let branch = find_latest_jsonl(&fork_root).expect("branch file exists");
    let branch_text = std::fs::read_to_string(&branch).unwrap();
    assert!(
        !branch_text.contains("toolName\":\"subagent"),
        "{branch_text}"
    );
    assert!(
        !branch_text.contains("\"name\":\"subagent\""),
        "{branch_text}"
    );
    assert!(!branch_text.contains("signature"), "{branch_text}");
    assert!(branch_text.contains("thinking_level_change"));
    let header: Value = serde_json::from_str(branch_text.lines().next().unwrap()).unwrap();
    assert_eq!(
        header["cwd"].as_str(),
        Some(real_project.to_string_lossy().as_ref())
    );
    // Child got the branch file via --session and the wrapped task.
    let argv = std::fs::read_to_string(dump.join("argv.txt")).unwrap();
    assert!(argv.contains("--session"));
    assert!(argv.lines().any(|l| l == branch.to_string_lossy()));
    assert!(
        argv.contains("Task: You are a delegated subagent running from a fork"),
        "fork preamble wraps the task:\n{argv}"
    );

    // ---- Scenario 3b (V13-02 FR-D): dual-instance authoritative parent -----
    // Two rpi instances share one session dir (double-open same cwd):
    // instance A's file is mtime-NEWEST and carries its own marker, while the
    // authoritative ctx.sessionFile names instance B. The fork must branch
    // from B (the authoritative value), not from A's mtime-leading file;
    // and once the fork artifact lands fresh, a second fork must STILL use B
    // (never lock onto fork.jsonl — audit hit surface 2).
    let dual_dir = sandbox.root.join("dual");
    std::fs::create_dir_all(&dual_dir).unwrap();
    let make_stem = |ts: &str, id: &str| format!("{ts}_{id}");
    // A: mtime-newest, B: older but authoritative.
    let file_a = dual_dir.join(format!(
        "{}.jsonl",
        make_stem(
            "2026-08-19T23-00-00-000",
            "11111111-1111-7111-8111-111111111111"
        )
    ));
    let file_b = dual_dir.join(format!(
        "{}.jsonl",
        make_stem(
            "2026-08-19T10-00-00-000",
            "22222222-2222-7222-8222-222222222222"
        )
    ));
    let write_session = |path: &std::path::Path, marker: &str, cwd: &str| {
        std::fs::write(
            path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-08-19T00:00:00.000Z","cwd":"{}"}}"# , "\n",
                    r#"{{"type":"message","id":"id000001","timestamp":"2026-08-19T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}}}"# , "\n"
                ),
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .rsplit_once('_')
                    .map(|(_, tail)| tail)
                    .unwrap_or("?"),
                cwd,
                marker
            ),
        )
        .unwrap();
        // Pin A's mtime strictly newest at write time (CI sandboxes round
        // mtime to whole seconds, so sleeps do not separate writes).
        if path == file_a.as_path() {
            let file = std::fs::File::open(path).unwrap();
            file.set_modified(std::time::SystemTime::now()).unwrap();
        }
    };
    write_session(&file_b, "marker-B", &real_project.to_string_lossy());
    write_session(&file_a, "marker-A", &real_project.to_string_lossy());
    // B gets a clearly older mtime (60s back), keeping A the newest.
    let fb = std::fs::File::open(&file_b).unwrap();
    fb.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
        .unwrap();
    // Authoritative = instance B while A holds the newest mtime.
    assert!(
        file_a
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .zip(file_b.metadata().and_then(|m| m.modified()).ok())
            .is_some_and(|(a, b)| a > b),
        "precondition: A must be mtime-newer than B"
    );
    *host.session_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(json!({
        "path": file_b.to_string_lossy(),
        "id": "22222222-2222-7222-8222-222222222222",
    }));
    std::fs::write(
        sandbox.project.join(".rpi/settings.json"),
        json!({ "sessionDir": dual_dir.to_string_lossy() }).to_string(),
    )
    .unwrap();
    let dual_dump = sandbox.dump("dual");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dual_dump);
    let dual_runs = dual_dir.join("runs");
    let result = execute(json!({
        "agent": "delegate",
        "task": "dual fork",
        "context": "fork",
        "timeoutMs": 30000,
        "artifacts": false,
        "sessionDir": dual_runs.to_string_lossy(),
    }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    let branch1 = find_latest_jsonl(&dual_runs).expect("fork branch #1");
    let branch1_text = std::fs::read_to_string(&branch1).unwrap();
    assert!(
        branch1_text.contains("marker-B"),
        "fork must branch from the authoritative instance B, not mtime-newest A:\n{branch1_text}"
    );
    assert!(!branch1_text.contains("marker-A"), "{branch1_text}");
    // The fresh branch file (fork-N.jsonl, mtime newest) must not hijack the
    // NEXT fork — the authoritative value still rules (audit hit surface 2).
    let result = execute(json!({
        "agent": "delegate",
        "task": "dual fork again",
        "context": "fork",
        "timeoutMs": 30000,
        "artifacts": false,
        "sessionDir": dual_runs.to_string_lossy(),
    }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    let branch2 = find_latest_jsonl(&dual_runs).expect("fork branch #2");
    let branch2_text = std::fs::read_to_string(&branch2).unwrap();
    assert!(
        branch2_text.contains("marker-B"),
        "second fork still from instance B, not fork artifact:\n{branch2_text}"
    );

    // ---- Scenario 4: depth block ---------------------------------------
    std::env::set_var("RPI_SUBAGENT_DEPTH", "2");
    let result = execute(json!({ "agent": "scout", "task": "go deeper", "timeoutMs": 5000 }));
    assert_eq!(result["isError"], Value::Bool(true));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Nested subagent call blocked (depth=2, max=2)"),
        "{text}"
    );
    // No child spawned: no new dump dir contents.
    let dump = sandbox.dump("blocked");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
    let result = execute(json!({ "agent": "scout", "task": "still blocked", "timeoutMs": 5000 }));
    assert!(result["isError"] == Value::Bool(true));
    assert!(!dump.join("pid.txt").exists(), "no child process spawned");
    std::env::remove_var("RPI_SUBAGENT_DEPTH");

    // ---- Scenario 5: timeout ladder + process reclamation --------------
    // The child emits a NON-terminal assistant message (toolCall present) and
    // hangs: only the timeout ladder (SIGINT → SIGTERM → SIGKILL) reclaims it.
    let dump = sandbox.dump("timeout");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
    std::env::set_var("RPI_E2E_MODE", "partial_toolcall_then_hang");
    let started = std::time::Instant::now();
    let result = execute(json!({ "agent": "scout", "task": "hang around", "timeoutMs": 1500 }));
    let elapsed = started.elapsed();
    assert_eq!(result["isError"], Value::Bool(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Subagent timed out after 1500ms."), "{text}");
    assert!(text.contains("Partial output before timeout:"), "{text}");
    assert!(text.contains("partial output before hanging"), "{text}");
    assert!(
        elapsed.as_secs() < 20,
        "timeout ladder finished promptly: {elapsed:?}"
    );
    let pid: u32 = std::fs::read_to_string(dump.join("pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_for_process_exit(pid),
        "child {pid} reclaimed after timeout ladder"
    );
    assert!(assert_no_rpi_subagent_children(), "no residual children");

    // ---- Scenario 5b: terminal drain reclaims a stuck child ------------
    // The child reaches a clean terminal stop but keeps running; the drain
    // ladder SIGTERMs it after the 1s grace (execution.ts:571-623).
    let dump = sandbox.dump("drain");
    std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
    std::env::set_var("RPI_E2E_MODE", "partial_then_hang");
    let started = std::time::Instant::now();
    let result = execute(json!({
        "agent": "scout",
        "task": "terminal but stuck",
        "timeoutMs": 60000,
        "artifacts": false
    }));
    let elapsed = started.elapsed();
    assert_eq!(result["isError"], Value::Bool(true), "{result}");
    assert!(
        elapsed.as_secs() < 20,
        "drain ladder finished promptly: {elapsed:?}"
    );
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("partial output before hanging"));
    let pid: u32 = std::fs::read_to_string(dump.join("pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_for_process_exit(pid),
        "child {pid} reclaimed after drain ladder"
    );

    // ---- Scenario 6: child failure propagates ---------------------------
    std::env::set_var("RPI_E2E_MODE", "fail");
    let result = execute(json!({
        "agent": "scout",
        "task": "explode",
        "timeoutMs": 30000,
        "artifacts": false
    }));
    assert_eq!(result["isError"], Value::Bool(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("model exploded"), "{text}");
    assert_eq!(result["details"]["results"][0]["exitCode"], 3);

    // ---- Scenario 7: raw non-JSON output on nonzero exit ---------------
    std::env::set_var("RPI_E2E_MODE", "rawjunk");
    let result = execute(json!({
        "agent": "scout",
        "task": "junk",
        "timeoutMs": 30000,
        "artifacts": false
    }));
    assert_eq!(result["isError"], Value::Bool(true), "{result}");
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("this is not json at all"));

    // ---- Scenario 8: willRetry then settled ----------------------------
    std::env::set_var("RPI_E2E_MODE", "willretry");
    let result = execute(json!({
        "agent": "scout",
        "task": "retry once",
        "timeoutMs": 30000,
        "artifacts": false
    }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    assert_eq!(
        result["content"][0]["text"].as_str().unwrap(),
        "after retry"
    );

    // ---- Scenario 9 (TE05): parallel tasks composition (FR-P1-01) ----
    {
        let dump = sandbox.dump("parallel");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        let result = execute(json!({
            "tasks": [
                { "key": "alpha", "agent": "scout", "task": "scan A" },
                { "key": "beta", "agent": "reviewer", "task": "review B" },
                { "key": "gamma", "agent": "scout", "task": "scan C" }
            ],
            "concurrency": 2,
            "async": false
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        assert_eq!(result["details"]["mode"], "parallel");
        let results = result["details"]["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        // Submission order preserved with per-key entries.
        assert_eq!(results[0]["key"], "alpha");
        assert_eq!(results[2]["key"], "gamma");
        assert_eq!(results[1]["agent"], "reviewer");
        // Aggregate sections in output text.
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("=== Parallel Task 1 (scout) ==="), "{text}");
        assert!(text.contains("=== Parallel Task 2 (reviewer) ==="));
        // Per-child artifacts with index suffixes + child_index env.
        let artifacts_dir = sandbox.project.join(".rpi/subagents/artifacts");
        let run_id = result["details"]["runId"].as_str().unwrap();
        for (index, agent) in [(0u32, "scout"), (1, "reviewer"), (2, "scout")] {
            assert!(
                artifacts_dir
                    .join(format!("{run_id}_{agent}_{index}_output.md"))
                    .exists(),
                "indexed artifact {index} for {agent}"
            );
            let env_text =
                std::fs::read_to_string(dump.join(format!("child-{index}")).join("env.txt"))
                    .unwrap_or_default();
            assert!(
                env_text.contains(&format!("RPI_SUBAGENT_CHILD_INDEX={index}")),
                "child {index} env carries its index: {env_text}"
            );
        }
    }

    // ---- Scenario 10 (TE05): chain steps composition (FR-P1-02) --------
    {
        let dump = sandbox.dump("chain");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        let result = execute(json!({
            "steps": [
                { "agent": "scout", "task": "gather {task}" },
                { "agent": "reviewer", "task": "review this:\n{previous}" }
            ],
            "task": "the plan",
            "async": false
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        assert_eq!(result["details"]["mode"], "chain");
        assert_eq!(result["details"]["chainStepCount"], 2);
        // Final text is the last completed step's output.
        assert_eq!(
            result["content"][0]["text"].as_str().unwrap(),
            "Fixed child result: analysis complete"
        );
        // Step 2's prompt received step 1's output via {previous} and the
        // original task via {task} on step 1 (prompt dump files).
        // The interpolated task rides argv (Task: ...), not the system prompt.
        let step0_argv =
            std::fs::read_to_string(dump.join("child-0").join("argv.txt")).unwrap_or_default();
        assert!(step0_argv.contains("gather the plan"), "{step0_argv}");
        let step1_argv =
            std::fs::read_to_string(dump.join("child-1").join("argv.txt")).unwrap_or_default();
        assert!(
            step1_argv.contains("review this:"),
            "step 2 template reached the child: {step1_argv}"
        );
        // {previous} interpolated to step 1's fixed-child output — the task
        // text is multiline in argv, so match both fragments.
        assert!(
            step1_argv.contains("Fixed child result: analysis complete"),
            "step 2 received step 1 output: {step1_argv}"
        );
        // Chain scratch dir materialized under .rpi/subagents/chain-runs.
        let chain_root = sandbox.project.join(".rpi/subagents/chain-runs");
        assert!(chain_root.exists(), "chain-runs root exists");

        // ---- Scenario 10b (TE09 FR-B): chain-mode streaming frames ------
        {
            let dump = sandbox.dump("chain-stream");
            std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
            std::env::set_var("RPI_E2E_MODE", "ok");
            take_tool_updates();
            let result = execute(json!({
                "steps": [
                    { "agent": "scout", "task": "first {task}" },
                    { "agent": "reviewer", "task": "second {previous}" }
                ],
                "task": "the plan",
                "async": false
            }));
            assert_eq!(result["isError"], Value::Bool(false), "{result}");
            let frames = take_tool_updates();
            assert!(!frames.is_empty(), "chain frames reached the host");
            let first = &frames[0]["update"]["details"];
            assert_eq!(first["mode"], json!("chain"));
            assert_eq!(first["totalSteps"], json!(2));
            assert_eq!(first["currentStepIndex"], json!(0));
            assert_eq!(first["chainAgents"], json!(["scout", "reviewer"]));
            assert_eq!(first["outputs"], json!({}));
            assert_eq!(first["results"].as_array().unwrap().len(), 1);
            assert_eq!(first["progress"].as_array().unwrap().len(), 1);
            // Step 2 frames accumulate step 1's terminal result + progress.
            let second_step_first = frames
                .iter()
                .find(|frame| frame["update"]["details"]["currentStepIndex"] == json!(1))
                .expect("step-2 frames arrived");
            let details = &second_step_first["update"]["details"];
            assert_eq!(details["results"].as_array().unwrap().len(), 2);
            assert_eq!(details["progress"].as_array().unwrap().len(), 2);
            assert_eq!(details["progress"][0]["status"], json!("completed"));
            assert_eq!(details["progress"][0]["agent"], json!("scout"));
        }
    }

    // ---- Scenario 11 (TE05): background run lifecycle (FR-P1-04) ------
    {
        let dump = sandbox.dump("async");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        let result = execute(json!({
            "agent": "scout",
            "task": "async work",
            "async": true,
            "timeoutMs": 30000
        }));
        // Receipt returns immediately; not an error.
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Background run"), "{text}");
        let status_file = result["details"]["statusFile"]
            .as_str()
            .unwrap()
            .to_string();
        // The driver finishes quickly (fixed child); wait for the terminal
        // status (subagent_wait core loop). Regression guard: finish used to
        // unregister the run, so the wait only ever ended by burning this
        // 30s timeout — assert the terminal observation, not just the
        // non-error receipt.
        let wait = rpi_ext_subagents::execute_tool_for_test(
            "subagent_wait",
            &json!({ "all": true, "timeoutMs": 30000 }),
        );
        assert_eq!(wait["isError"], Value::Bool(false), "{wait}");
        assert_eq!(wait["details"]["waited"], json!(1), "{wait}");
        assert!(
            wait["details"]["timedOut"].is_null(),
            "wait must observe the terminal transition, not time out: {wait}"
        );
        assert!(
            wait["details"]["runs"][0]["state"] == json!("complete"),
            "{wait}"
        );
        let status_text = std::fs::read_to_string(&status_file).unwrap_or_default();
        assert!(
            status_text.contains("\"complete\""),
            "status.json reached complete: {status_text}"
        );
        // events.jsonl carries the lifecycle.
        let events_file = status_file.replace("status.json", "events.jsonl");
        let events = std::fs::read_to_string(&events_file).unwrap_or_default();
        assert!(events.contains("run.started"), "{events}");
        assert!(events.contains("run.finished"));
    }

    // ---- Scenario 13 (TE09): foreground streaming snapshots (FR-A) ----
    {
        let dump = sandbox.dump("streaming");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "tools");
        take_tool_updates();
        let result = execute(json!({
            "agent": "scout",
            "task": "streaming run",
            "async": false,
            "timeoutMs": 30000
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        let frames = take_tool_updates();
        assert!(!frames.is_empty(), "streaming frames reached the host");
        let first = &frames[0];
        // toolUpdate envelope: toolCallId + the partial AgentToolResult.
        // The initial frame (execution.ts:1009 fireUpdate right after the
        // handler attaches) fires BEFORE any event: no tool, no output —
        // the first frame must not wait for the first 1s activity tick.
        assert_eq!(first["toolCallId"], json!("test"));
        assert_eq!(
            first["update"]["content"][0]["text"],
            json!("(running...)"),
            "initial frame precedes every event"
        );
        let details = &first["update"]["details"];
        assert_eq!(details["mode"], json!("single"));
        assert_eq!(details["controlEvents"], json!([]));
        let progress = &details["progress"][0];
        assert_eq!(progress["agent"], json!("scout"));
        assert_eq!(progress["status"], json!("running"));
        assert_eq!(progress["currentTool"], Value::Null);
        assert_eq!(progress["toolCount"], json!(0));
        assert_eq!(details["results"][0].get("messages"), None);
        // The second frame is the first progress-bearing event
        // (tool_execution_start): the read tool lights up.
        let second = &frames[1];
        let progress = &second["update"]["details"]["progress"][0];
        assert_eq!(progress["currentTool"], json!("read"));
        assert_eq!(progress["currentToolArgs"], json!("/tmp/e2e.rs"));
        assert_eq!(progress["currentPath"], json!("/tmp/e2e.rs"));
        assert_eq!(progress["toolCount"], json!(1));
        assert_eq!(
            second["update"]["details"]["results"][0].get("messages"),
            None
        );
        // After tool_execution_end + the final message: recentTools carries
        // the finished call, the frame text is the final output, and the
        // streamed result embeds the bounded toolCalls summary.
        let last = frames.last().unwrap();
        assert_eq!(
            last["update"]["content"][0]["text"],
            json!("Analyzed the file")
        );
        let details = &last["update"]["details"];
        let progress = &details["progress"][0];
        assert_eq!(
            progress["recentTools"][0]["tool"],
            json!("read"),
            "tool_execution_end moved the call into recentTools"
        );
        assert_eq!(progress["currentTool"], Value::Null);
        assert_eq!(progress["turnCount"], json!(1));
        assert_eq!(
            progress["recentOutput"],
            json!(["file contents here", "Analyzed the file"])
        );
        assert_eq!(
            details["results"][0]["toolCalls"][0]["text"],
            json!("read /tmp/e2e.rs")
        );
    }

    // ---- Scenario 14 (TE09): notify 归位 + renderer (FR-D) --------------
    {
        let dump = sandbox.dump("notify");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        take_sent_messages();
        take_sent_message_calls();
        let result = execute(json!({
            "agent": "scout",
            "task": "notify run",
            "async": true,
            "timeoutMs": 30000
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        let wait = rpi_ext_subagents::execute_tool_for_test(
            "subagent_wait",
            &json!({ "all": true, "timeoutMs": 30000 }),
        );
        assert_eq!(wait["isError"], Value::Bool(false), "{wait}");
        assert_eq!(wait["details"]["waited"], json!(1), "{wait}");
        assert!(
            wait["details"]["timedOut"].is_null(),
            "wait must observe the terminal transition, not time out: {wait}"
        );
        let messages = take_sent_messages();
        let notify = messages
            .iter()
            .find(|m| m["customType"] == json!("subagent-notify"))
            .expect("completion notification arrived as subagent-notify");
        // Wire shape (notify.ts sendCompletion): no details field on the
        // message; the renderer re-parses the content text. `display` is
        // false for completed background runs (only non-completed or
        // foreground sources force it true upstream).
        assert!(notify.get("details").is_none());
        assert_eq!(notify["display"], json!(false));
        // Upstream sendCompletion always passes an options object with
        // `triggerTurn: true` (notify.ts:178-182: `result.triggerTurn !==
        // false`, true by default) — the completion wakes the parent
        // session to process the result instead of silently appending.
        let call = take_sent_message_calls()
            .into_iter()
            .find(|call| call["message"]["customType"] == json!("subagent-notify"))
            .expect("subagent-notify sendMessage call recorded");
        assert_eq!(call["options"]["triggerTurn"], json!(true));
        let content = notify["content"].as_str().unwrap();
        assert!(
            content.starts_with("Background task completed: **scout**"),
            "{content}"
        );
        assert!(content.contains("Fixed child result: analysis complete"));

        // The registered renderer maps the message onto a ComponentTree
        // (host render dispatch).
        let tree = rpi_ext_subagents::render_message_for_test(
            "subagent-notify",
            notify,
            &json!({ "expanded": false }),
        );
        let children = tree["children"].as_array().unwrap();
        assert!(children[0]["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("✓ scout completed"));
        assert_eq!(children[0]["props"]["fg"], json!("success"));
        assert!(children[1]["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("  ⎿  Fixed child result"));
        // The old self-made type is gone.
        assert!(
            !messages
                .iter()
                .any(|m| m["customType"] == json!("subagent-async-complete")),
            "legacy type no longer injected"
        );
    }

    // ---- Scenario 12 (TE05): budget rejection paths (FR-P1-04/09) -----
    {
        let dump = sandbox.dump("budget");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        // Drain the session spawn ledger to its cap via the sandbox config
        // (maxSubagentSpawnsPerSession: 1): the ledger is keyed by session id
        // hash — a unique agent dir per scenario isolates prior counts. Use
        // the direct ledger API instead for determinism.
        let session_id = format!("e2e-budget-{}", std::process::id());
        let ledger = rpi_ext_subagents::test_support::SpawnBudgetLedgerProbe::open(&session_id);
        ledger.reset_for_test();
        ledger.reserve_for_test(1, Some(1)).unwrap();
        let _result = execute(json!({
            "agent": "scout",
            "task": "over budget",
            "async": true,
            "sessionDir": sandbox.root.join("budget-sessions").to_string_lossy().to_string(),
        }));
        // The spawn ledger in dispatch keys off the parent session file; with
        // no parent session the ledger is "no-session" — assert the direct
        // rejection path instead through a second reserve.
        assert!(ledger.reserve_for_test(1, Some(1)).is_err(), "cap enforced");
        // The run above still spawned a real background child (no parent
        // session → the in-dispatch ledger check passes). Wait it out so the
        // final no-residual-children sweep is not racing its exit — before
        // the wait-unregister fix this was masked by scenarios 11/14 burning
        // their full 30s timeouts.
        let wait = rpi_ext_subagents::execute_tool_for_test(
            "subagent_wait",
            &json!({ "all": true, "timeoutMs": 10000 }),
        );
        assert_eq!(wait["isError"], Value::Bool(false), "{wait}");
    }

    // ---- Scenario 15 (TE11): render dispatch + fleet widget lifecycle ----
    {
        let dump = sandbox.dump("te11");
        std::env::set_var("RPI_E2E_DUMP_DIR", &dump);
        std::env::set_var("RPI_E2E_MODE", "ok");
        // Shrink the fleet linger window so the empty-state removal is
        // observable without a 60 s wait (atomic seam — env updates race
        // the loop's worker thread).
        rpi_ext_subagents::fleet::set_linger_for_test(0);
        take_set_widget_calls();

        // FR-A: the call title renders through the tool render dispatch
        // (four shapes).
        let call = rpi_ext_subagents::render_tool_for_test(
            "toolCall",
            &json!({"agent": "researcher", "task": "map the module graph", "async": true}),
            &Value::Null,
            &Value::Null,
        );
        let title = call["children"][0]["props"]["text"].as_str().unwrap();
        assert!(title.starts_with("subagent · researcher"), "{title}");
        assert!(title.ends_with("[async]"), "{title}");
        assert_eq!(call["children"][0]["props"]["bold"], json!(true));
        assert!(
            call["children"][1]["props"]["text"]
                .as_str()
                .unwrap()
                .starts_with("map the module graph"),
            "{call}"
        );
        for shape in [
            json!({"tasks": [{}, {}, {}]}),
            json!({"steps": [{}, {}]}),
            json!({"action": "status"}),
        ] {
            let call = rpi_ext_subagents::render_tool_for_test(
                "toolCall",
                &shape,
                &Value::Null,
                &Value::Null,
            );
            let title = call["children"][0]["props"]["text"].as_str().unwrap();
            assert!(
                title.starts_with("subagent · ") && title != "subagent · ",
                "shape renders a title: {title}"
            );
        }

        // FR-B: a real foreground result renders the terminal card.
        let result = execute(json!({
            "agent": "scout",
            "task": "te11 card",
            "async": false,
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        let card = rpi_ext_subagents::render_tool_for_test(
            "toolResult",
            &Value::Null,
            &result,
            &json!({"expanded": false}),
        );
        let head = card["children"][0]["props"]["text"].as_str().unwrap();
        assert!(head.starts_with("✓ scout · Done"), "{head}");
        assert_eq!(card["children"][0]["props"]["fg"], json!("success"));
        assert!(
            card["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line["props"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("output: ")),
            "artifact path line present: {card}"
        );

        // FR-C: the fleet strip appears for a background run and removes
        // itself on the empty snapshot (linger window 0 above).
        let result = execute(json!({
            "agent": "scout",
            "task": "te11 fleet",
            "async": true,
            "timeoutMs": 30000
        }));
        assert_eq!(result["isError"], Value::Bool(false), "{result}");
        // First tick lands within ~one refresh period.
        let mut pushes = Vec::new();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            pushes = take_set_widget_calls();
            if pushes.iter().any(|call| !call["content"].is_null()) {
                break;
            }
        }
        let push = pushes
            .iter()
            .find(|call| !call["content"].is_null())
            .expect("fleet widget pushed with content");
        assert_eq!(push["key"], json!("subagent-fleet-status"), "{push}");
        assert_eq!(push["placement"], json!("belowEditor"), "{push}");
        assert!(
            push["content"]["children"].is_array(),
            "Component form: {push}"
        );

        // With the fixed-child run quickly terminal and linger 0, the loop
        // removes the widget and exits within a couple of ticks (no
        // subagent_wait here — by the time we poll the widget the run is
        // already terminal and wait would have nothing active to wait on).
        let mut removal = None;
        let mut all_calls = Vec::new();
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let calls = take_set_widget_calls();
            all_calls.extend(calls.iter().cloned());
            if let Some(call) = calls.iter().rev().find(|call| {
                call["content"].is_null() && call["key"] == json!("subagent-fleet-status")
            }) {
                removal = Some(call.clone());
                break;
            }
        }
        assert!(
            removal.is_some(),
            "fleet widget removed on the empty snapshot; calls seen: {all_calls:?}"
        );
        rpi_ext_subagents::fleet::set_linger_for_test(u64::MAX);
    }

    // Final sweep: nothing left running.
    assert!(
        assert_no_rpi_subagent_children(),
        "no residual children at end"
    );
    let _ = std::fs::remove_dir_all(&sandbox.root);
}

fn find_latest_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}
