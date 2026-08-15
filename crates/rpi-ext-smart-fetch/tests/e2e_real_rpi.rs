//! Real-rpi-binary e2e (design §5.5): the actual `rpi --mode json -p`
//! process with the smart-fetch cdylib injected via `--extension`, driven by
//! a stub OpenAI-completions server whose FIRST turn issues a `web_fetch`
//! tool call against the local mock responder — asserting schema visibility,
//! real tool execution and result passthrough end to end.
//!
//! Skips when the rpi binary or the cdylib are missing (build
//! `cargo build --workspace` first — the gate order does).

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use serde_json::json;

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
        "librpi_ext_smart_fetch.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_smart_fetch.dll"
    } else {
        "librpi_ext_smart_fetch.so"
    };
    let path = target_dir().join("debug").join(name);
    path.is_file().then_some(path)
}

fn read_request(stream: &mut TcpStream) -> usize {
    let mut buffer = [0u8; 65536];
    let mut read = 0usize;
    loop {
        match stream.read(&mut buffer[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                read += n;
                let text = String::from_utf8_lossy(&buffer[..read]);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .filter_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
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
        }
    }
    read
}

/// Stub OpenAI-completions server: the first turn emits a `web_fetch`
/// tool_call for the mock URL, every later turn ends the conversation.
fn spawn_stub_server(mock_url: String) -> (String, std::thread::JoinHandle<()>) {
    spawn_stub_server_with_call("web_fetch", json!({ "url": mock_url }).to_string())
}

fn spawn_stub_server_with_call(
    tool_name: &str,
    arguments: String,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
    let address = listener.local_addr().expect("local addr").to_string();
    let tool_name = tool_name.to_string();
    let handle = std::thread::spawn(move || {
        for (turn, stream) in listener.incoming().flatten().enumerate() {
            let mut stream = stream;
            read_request(&mut stream);
            let body = if turn == 0 {
                // arguments ride as a JSON-escaped string inside the SSE delta
                let escaped = arguments.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    concat!(
                        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"arguments\":\"{}\"}}}}]}}}}]}}\n\n",
                        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
                        "data: [DONE]\n\n",
                    ),
                    tool_name,
                    escaped
                )
            } else {
                concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"fetched\"}}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
                    "data: [DONE]\n\n",
                )
                .to_string()
            };
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
                "stubsf": {
                    "baseUrl": base_url,
                    "api": "openai-completions",
                    "apiKey": "STUBSF_API_KEY",
                    "models": [
                        {
                            "id": "stub-1",
                            "name": "Stub SmartFetch 1",
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

#[test]
fn e2e_real_rpi_web_fetch_tool_roundtrip() {
    let Some(rpi) = rpi_binary() else {
        eprintln!("skipping: rpi binary missing (cargo build --workspace first)");
        return;
    };
    let Some(cdylib) = cdylib_path() else {
        eprintln!(
            "skipping: smart-fetch cdylib missing (cargo build -p rpi-ext-smart-fetch first)"
        );
        return;
    };

    // mock content server the tool call targets
    let mock = common::Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "e2e mock payload".to_string(),
    )]);
    let (stub_address, _stub) = spawn_stub_server(mock.url("/e2e"));

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sandbox = std::env::temp_dir().join(format!("rpi-sf-e2e-{}-{nonce}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let project = sandbox.join("proj");
    let agent_dir = sandbox.join("agent");
    std::fs::create_dir_all(project.join(".rpi")).unwrap();
    write_models_json(&agent_dir, &format!("http://{stub_address}/v1"));

    // a bare .so carries no manifest → capabilities=[] → registerTool is
    // denied; package the cdylib with its manifest (same layout as installs)
    let plugin_dir = sandbox.join("plug");
    std::fs::create_dir_all(plugin_dir.join("dist")).unwrap();
    let plugin_name = cdylib.file_name().unwrap().to_string_lossy().to_string();
    std::fs::copy(&cdylib, plugin_dir.join("dist").join(&plugin_name)).unwrap();
    std::fs::write(
        plugin_dir.join("rpi-extension.json"),
        json!({
            "name": "rpi-smart-fetch",
            "version": "0.1.0",
            "native": format!("dist/{plugin_name}"),
            "capabilities": ["tools", "session"],
            "rpiAbi": 1,
        })
        .to_string(),
    )
    .unwrap();

    let output = std::process::Command::new(&rpi)
        .arg("--mode")
        .arg("json")
        .arg("-p")
        .arg("--model")
        .arg("stubsf/stub-1")
        .arg("--extension")
        .arg(&plugin_dir)
        .arg("--no-context-files")
        .arg("Fetch it")
        .current_dir(&project)
        .env("RPI_CODING_AGENT_DIR", &agent_dir)
        .env("STUBSF_API_KEY", "stub-key")
        .output()
        .expect("spawn rpi");
    assert!(
        output.status.success(),
        "rpi failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // The agent issued the web_fetch tool call and the extension executed it
    // against the mock server — the tool result rides the JSON stream.
    assert!(
        stdout.contains("web_fetch"),
        "tool call visible in stream: {stdout}"
    );
    assert!(
        stdout.contains("e2e mock payload"),
        "tool result content visible: {stdout}"
    );
    assert!(
        stdout.contains("> Browser: chrome_145/windows"),
        "metadata header shape: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// TE07 FR-P1-1 end to end: the real rpi binary calls `batch_web_fetch`
/// (two requests, one invalid) — per-item isolation and the summary header
/// ride the JSON stream.
#[test]
fn e2e_real_rpi_batch_web_fetch_roundtrip() {
    let Some(rpi) = rpi_binary() else {
        eprintln!("skipping: rpi binary missing (cargo build --workspace first)");
        return;
    };
    let Some(cdylib) = cdylib_path() else {
        eprintln!(
            "skipping: smart-fetch cdylib missing (cargo build -p rpi-ext-smart-fetch first)"
        );
        return;
    };

    let mock = common::Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "e2e batch payload".to_string(),
    )]);
    let (stub_address, _stub) = spawn_stub_server_with_call(
        "batch_web_fetch",
        json!({
            "requests": [
                { "url": mock.url("/batch") },
                { "url": "not a url" },
            ]
        })
        .to_string(),
    );

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sandbox =
        std::env::temp_dir().join(format!("rpi-sf-e2e-batch-{}-{nonce}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let project = sandbox.join("proj");
    let agent_dir = sandbox.join("agent");
    std::fs::create_dir_all(project.join(".rpi")).unwrap();
    write_models_json(&agent_dir, &format!("http://{stub_address}/v1"));

    let plugin_dir = sandbox.join("plug");
    std::fs::create_dir_all(plugin_dir.join("dist")).unwrap();
    let plugin_name = cdylib.file_name().unwrap().to_string_lossy().to_string();
    std::fs::copy(&cdylib, plugin_dir.join("dist").join(&plugin_name)).unwrap();
    std::fs::write(
        plugin_dir.join("rpi-extension.json"),
        json!({
            "name": "rpi-smart-fetch",
            "version": "0.1.0",
            "native": format!("dist/{plugin_name}"),
            "capabilities": ["tools", "session"],
            "rpiAbi": 1,
        })
        .to_string(),
    )
    .unwrap();

    let output = std::process::Command::new(&rpi)
        .arg("--mode")
        .arg("json")
        .arg("-p")
        .arg("--model")
        .arg("stubsf/stub-1")
        .arg("--extension")
        .arg(&plugin_dir)
        .arg("--no-context-files")
        .arg("Fetch both")
        .current_dir(&project)
        .env("RPI_CODING_AGENT_DIR", &agent_dir)
        .env("STUBSF_API_KEY", "stub-key")
        .output()
        .expect("spawn rpi");
    assert!(
        output.status.success(),
        "rpi failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(
        stdout.contains("batch_web_fetch"),
        "batch tool call visible: {stdout}"
    );
    assert!(stdout.contains("> Requests: 2"), "summary header: {stdout}");
    assert!(
        stdout.contains("> Failed: 1"),
        "invalid item isolated as error: {stdout}"
    );
    assert!(
        stdout.contains("e2e batch payload"),
        "successful item content: {stdout}"
    );
    assert!(
        stdout.contains("Invalid URL: not a url"),
        "error item text: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}
