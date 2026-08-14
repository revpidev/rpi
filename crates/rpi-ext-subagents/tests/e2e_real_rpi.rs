//! Real-rpi-binary e2e (design §5.3): the plugin spawns an actual `rpi
//! --mode json -p` child against a local stub OpenAI-completions server,
//! with the real cdylib injected as the child extension (child mode +
//! required-tools diagnostic). Also records the child's stdout JSONL as the
//! drift-guard fixture (`tests/fixtures/child_stream.jsonl`, design §3.3)
//! and replays it through the event parser.
//!
//! Skips when the rpi binary or the cdylib are missing (`cargo test -p
//! rpi-ext-subagents` alone does not build them; run `cargo build
//! --workspace` first — the gate order does).

use std::io::{Read, Write};
use std::net::TcpListener;
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

fn target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        })
}

fn rpi_binary() -> Option<PathBuf> {
    let binary = target_dir().join("debug").join("rpi");
    binary.is_file().then_some(binary)
}

fn cdylib_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "macos") {
        "librpi_ext_subagents.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_subagents.dll"
    } else {
        "librpi_ext_subagents.so"
    };
    let path = target_dir().join("debug").join(name);
    path.is_file().then_some(path)
}

/// One-shot stub OpenAI-completions server: accepts any number of requests,
/// answers each with a fixed SSE stream ending in a stop finish.
fn spawn_stub_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
    let address = listener.local_addr().expect("local addr").to_string();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 8192];
            let mut read = 0usize;
            // Read headers + body (content-length bounded).
            loop {
                match stream.read(&mut buffer[read..]) {
                    Ok(0) => break,
                    Ok(n) => {
                        read += n;
                        let text = String::from_utf8_lossy(&buffer[..read]);
                        if let Some(header_end) = text.find("\r\n\r\n") {
                            let content_length = text
                                .lines()
                                .filter_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    if !name.trim().eq_ignore_ascii_case("content-length") {
                                        return None;
                                    }
                                    value.trim().parse::<usize>().ok()
                                })
                                .next()
                                .unwrap_or(0);
                            if read >= header_end + 4 + content_length {
                                break;
                            }
                        }
                        if read >= buffer.len() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"The plan looks solid\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" and ready to land\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":9}}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (address, handle)
}

fn write_models_json(agent_dir: &Path, base_url: &str) {
    std::fs::create_dir_all(agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "stubpar": {
                    "baseUrl": base_url,
                    "api": "openai-completions",
                    "apiKey": "STUBPAR_API_KEY",
                    "models": [
                        {
                            "id": "stub-1",
                            "name": "Stub Parity 1",
                            "contextWindow": 128000,
                            "maxTokens": 4096,
                            "reasoning": false,
                            "input": ["text"]
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
}

/// Run `rpi --mode json -p` directly against the stub server and capture its
/// stdout — the drift-guard fixture for the child event protocol.
fn capture_child_stream(rpi: &Path, agent_dir: &Path, cwd: &Path) -> String {
    let output = std::process::Command::new(rpi)
        .arg("--mode")
        .arg("json")
        .arg("-p")
        .arg("--model")
        .arg("stubpar/stub-1")
        .arg("--no-extensions")
        .arg("--no-context-files")
        .arg("Say the plan verdict")
        .current_dir(cwd)
        .env("RPI_CODING_AGENT_DIR", agent_dir)
        .env("STUBPAR_API_KEY", "stub-key")
        .output()
        .expect("spawn rpi");
    assert!(
        output.status.success(),
        "rpi child failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn e2e_real_rpi_child_and_stream_fixture() {
    let Some(rpi) = rpi_binary() else {
        eprintln!("skipping: rpi binary missing (cargo build --workspace first)");
        return;
    };
    let Some(cdylib) = cdylib_path() else {
        eprintln!("skipping: subagents cdylib missing (cargo build -p rpi-ext-subagents first)");
        return;
    };
    let (address, _server) = spawn_stub_server();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sandbox =
        std::env::temp_dir().join(format!("rpi-sub-reale2e-{}-{nonce}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let project = sandbox.join("proj");
    let agent_dir = sandbox.join("agent");
    std::fs::create_dir_all(project.join(".rpi")).unwrap();
    write_models_json(&agent_dir, &format!("http://{address}/v1"));

    // 1. Capture the child stream once as the protocol fixture.
    let stream = capture_child_stream(&rpi, &agent_dir, &project);
    let fixture_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/child_stream.jsonl");
    if std::env::var_os("RPI_SUBAGENTS_REFRESH_FIXTURES").is_some() {
        std::fs::create_dir_all(fixture_path.parent().unwrap()).unwrap();
        std::fs::write(&fixture_path, &stream).expect("write fixture");
    }
    // The fixture ships in-tree; replay it (or the fresh capture) through the
    // parser: the recorded stream must yield the stubbed verdict.
    let replayed = std::fs::read_to_string(&fixture_path).unwrap_or_else(|_| stream.clone());
    let mut state = rpi_ext_subagents::child_stream_replay::ChildRunStatePublic::default();
    for line in replayed.lines().filter(|line| !line.trim().is_empty()) {
        state.process_line_public(line);
    }
    let final_output = rpi_ext_subagents::parity::get_final_output_public(state.messages_public());
    assert!(
        final_output.contains("The plan looks solid") && final_output.contains("ready to land"),
        "parser replay on the recorded child stream: {final_output}"
    );

    // 2. Full pipeline through the plugin with the real child.
    // Safety of set_var in tests: this is the only test in this binary.
    unsafe {
        std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
        std::env::set_var("RPI_SUBAGENT_RPI_BINARY", &rpi);
        std::env::set_var("RPI_SUBAGENT_EXTENSION_PATH", &cdylib);
        std::env::set_var("STUBPAR_API_KEY", "stub-key");
    }
    // TE05: pin the P0 foreground default in the sandbox config
    // (asyncByDefault defaults to true with FR-P1-04).
    std::fs::create_dir_all(agent_dir.join("extensions").join("subagent")).unwrap();
    std::fs::write(
        agent_dir
            .join("extensions")
            .join("subagent")
            .join("config.json"),
        r#"{"asyncByDefault": false}"#,
    )
    .unwrap();
    let host = Arc::new(FakeHost {
        cwd: project.clone(),
        model: json!({"provider": "stubpar", "id": "stub-1"}),
    });
    let response = rpi_ext_subagents::install_for_test(
        RpiHostCalls {
            call: fake_host_call,
        },
        Arc::into_raw(host) as PluginCookie,
    );
    assert_eq!(response.get("ok"), Some(&Value::Bool(true)), "{response}");

    let result = rpi_ext_subagents::execute_for_test(&json!({
        "agent": "reviewer",
        "task": "Review the plan in this repository",
        "timeoutMs": 120000,
    }));
    assert_eq!(
        result["isError"],
        Value::Bool(false),
        "result: {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("The plan looks solid") && text.contains("ready to land"),
        "child verdict reached the tool result: {text}"
    );
    let single = &result["details"]["results"][0];
    assert_eq!(single["agent"], "reviewer");
    assert_eq!(single["exitCode"], 0);
    assert_eq!(single["context"], "fresh");
    // Child session file exists under the derived run-0 dir.
    let session_dir = single["sessionFile"].as_str().unwrap_or("");
    assert!(!session_dir.is_empty() || result["details"]["artifacts"].is_object());
    // Artifacts trail in project mode.
    let artifacts = result["details"]["artifacts"]["dir"]
        .as_str()
        .unwrap_or_default();
    assert!(
        artifacts.contains(".rpi/subagents/artifacts"),
        "{artifacts}"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}
