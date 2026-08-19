//! In-process tool surface integration: management actions, schema shape,
//! ADR-0016/async rejections — against a fake host (`install_for_test`,
//! one install per binary because of the OnceLock plugin state; the
//! mcp-adapter `integration_wiring.rs` pattern).

use std::path::PathBuf;
use std::sync::Arc;

use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls};
use serde_json::{json, Value};

/// Fake host state: answers the host calls the plugin makes.
struct FakeHost {
    cwd: PathBuf,
    model: Value,
    tools: Vec<String>,
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
        "ctx.model" => json!({ "ok": host.model }),
        "getAllTools" => {
            json!({ "ok": host.tools.iter().map(|name| json!({"name": name})).collect::<Vec<_>>() })
        }
        _ => json!({ "error": { "kind": "unknownMethod", "message": method } }),
    };
    RVec::from(serde_json::to_vec(&response).unwrap_or_default())
}

fn sandbox() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rpi-sub-integration-{}-{nonce}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj/.rpi")).unwrap();
    dir
}

fn install(host: Arc<FakeHost>) {
    // Keep the host alive for the process lifetime (mem::forget pattern from
    // mcp-adapter integration_wiring.rs).
    let response = rpi_ext_subagents::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host) as PluginCookie,
    );
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)), "{response}");
}

fn execute(params: Value) -> Value {
    rpi_ext_subagents::execute_for_test(&params)
}

#[test]
fn tool_surface_integration() {
    let dir = sandbox();
    std::env::set_var("RPI_CODING_AGENT_DIR", dir.join("agent"));
    std::env::set_var("RPI_SUBAGENT_RPI_BINARY", "/nonexistent-rpi");
    // TE05: pin the foreground default (asyncByDefault defaults to true with
    // FR-P1-04); the async path has its own assertions above.
    std::fs::create_dir_all(dir.join("agent").join("extensions").join("subagent")).unwrap();
    std::fs::write(
        dir.join("agent")
            .join("extensions")
            .join("subagent")
            .join("config.json"),
        r#"{"asyncByDefault": false}"#,
    )
    .unwrap();
    let host = Arc::new(FakeHost {
        cwd: dir.join("proj"),
        model: json!({"provider": "faux", "id": "faux-1"}),
        tools: vec![
            "read".into(),
            "bash".into(),
            "edit".into(),
            "write".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
            "subagent".into(),
        ],
    });
    install(host);

    // list: six builtins under an isolated agent dir.
    let result = execute(json!({ "action": "list" }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    for name in [
        "delegate",
        "oracle",
        "researcher",
        "reviewer",
        "scout",
        "worker",
    ] {
        assert!(text.contains(&format!("- {name} (builtin")), "{text}");
    }
    assert!(text.contains("aliases: advisor"));

    // get with alias resolution.
    let result = execute(json!({ "action": "get", "agent": "advisor" }));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Agent: oracle (builtin)"), "{text}");

    // get on unknown agent.
    let result = execute(json!({ "action": "get", "agent": "nope" }));
    assert_eq!(result["isError"], Value::Bool(true));
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown agent: nope"));

    // doctor sections.
    let result = execute(json!({ "action": "doctor" }));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Runtime:"));
    assert!(text.contains("Discovery:"));
    assert!(text.contains("Depth / budget:"));

    // workflowScript placeholder fails loudly (ADR-0016).
    let result = execute(json!({ "workflowScript": "return runs.run('x', {})" }));
    assert_eq!(result["isError"], Value::Bool(true));
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("ADR-0016"));

    // async:true starts a background run (FR-P1-04): immediate receipt,
    // not an error; the run itself fails fast on the missing binary but the
    // receipt already returned.
    let result = execute(json!({ "agent": "scout", "task": "t", "async": true }));
    assert_eq!(result["isError"], Value::Bool(false));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Background run") && text.contains("started"),
        "{text}"
    );
    assert!(result["details"]["statusFile"].is_string());

    // status before any run.
    let result = execute(json!({ "action": "status" }));
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Foreground runs (this session):"));

    // Single delegation with a missing binary fails fast with a clear error.
    let result = execute(json!({ "agent": "scout", "task": "look around", "timeoutMs": 2000 }));
    assert_eq!(result["isError"], Value::Bool(true));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Failed to spawn subagent process"), "{text}");
    // details carry the run identity even for spawn failures.
    assert_eq!(result["details"]["mode"], "single");
    assert_eq!(result["details"]["timeoutMs"], json!(2000));

    // subagent_wait nonBlocking: the receipt must carry `details` — the
    // host deserializes every tool result into AgentToolResult, where a
    // missing details used to fail the whole execution with
    // "missing field `details`".
    let result =
        rpi_ext_subagents::execute_tool_for_test("subagent_wait", &json!({ "nonBlocking": true }));
    assert_eq!(result["isError"], Value::Bool(false), "{result}");
    assert_eq!(
        result["details"]["mode"], "management",
        "nonBlocking receipt carries management details: {result}"
    );
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("without blocking"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ADR-0021: the orchestration skill ships localized bodies (no workflowScript
// teaching). The layout marker must upgrade a pre-marker v1 byte-exact install
// in place, and must leave a same-marker directory (user-customized) alone.
// Uses the directory-parameterized installer: the agent-dir env var is
// process-global, so routing through it would race concurrent tests.
#[test]
fn orchestration_skill_marker_gates_upgrade_and_reinstall() {
    let dir = sandbox();
    let agent_dir = dir.join("agent");
    let skill_dir = agent_dir.join("skills").join("pi-subagents");
    std::fs::create_dir_all(skill_dir.join("references")).unwrap();
    // v1 install shape: upstream body mentioning workflowScript, no marker.
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pi-subagents\n---\n\nUse `workflowScript` for all execution.\n",
    )
    .unwrap();

    rpi_ext_subagents::test_support::install_orchestration_skill_at(&agent_dir);

    let upgraded = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    assert!(!upgraded.contains("workflowScript"), "{upgraded}");
    assert!(upgraded.contains("tasks"), "{upgraded}");
    for name in [
        "constraints-and-recipes.md",
        "execution-controls.md",
        "management-authoring-rpc.md",
        "prompting-and-roles.md",
    ] {
        let body = std::fs::read_to_string(skill_dir.join("references").join(name)).unwrap();
        assert!(
            !body.contains("workflowScript"),
            "{name} still teaches workflowScript"
        );
    }
    assert_eq!(
        std::fs::read_to_string(skill_dir.join(".rpi-layout-version")).unwrap(),
        "2"
    );

    // User customization at the current layout version must survive a
    // reinstall.
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pi-subagents\n---\n\nCustomized locally.\n",
    )
    .unwrap();
    rpi_ext_subagents::test_support::install_orchestration_skill_at(&agent_dir);
    assert_eq!(
        std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
        "---\nname: pi-subagents\n---\n\nCustomized locally.\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
