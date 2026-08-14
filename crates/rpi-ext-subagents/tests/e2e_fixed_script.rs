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
}

// Safety: the trampoline only dereferences the cookie as `&FakeHost`.
extern "C" fn fake_host_call(host_ptr: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    let host = unsafe { &*(host_ptr as *const FakeHost) };
    let parsed: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
    let method = parsed.get("call").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "registerTool" | "registerCommand" | "on" | "registerFlag" => json!({ "ok": true }),
        "ctx.cwd" => json!({ "ok": host.cwd.to_string_lossy() }),
        "ctx.model" => json!({ "ok": host.model }),
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
        model: json!({"provider": "faux", "id": "faux-1"}),
    });
    let response = rpi_ext_subagents::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host) as PluginCookie,
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
    // tools allowlist is the scout frontmatter list, single comma flag.
    let tools_pos = find_flag("--tools").expect("scout declares tools");
    assert_eq!(
        argv_lines[tools_pos + 1],
        "read,grep,find,ls,bash,write,intercom"
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
    assert!(env_text.contains("RPI_SUBAGENT_REQUIRED_TOOLS=[\"read\",\"grep\",\"find\",\"ls\",\"bash\",\"write\",\"intercom\"]"));

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
    // Point the plugin at the sessions dir via project settings.
    std::fs::write(
        sandbox.project.join(".rpi/settings.json"),
        json!({ "sessionDir": sessions.to_string_lossy() }).to_string(),
    )
    .unwrap();

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
