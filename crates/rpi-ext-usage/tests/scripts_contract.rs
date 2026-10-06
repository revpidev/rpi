//! TE44 FR-B/FR-F: provider-script contract tests, offline.
//!
//! Each test drives the real `scripts/*.py` (python3 stdlib) against a local
//! stub HTTP server that replays a recorded fixture, then asserts the
//! schemaVersion=1 envelope and the degradation matrix (missing key, 401,
//! in-band errors, malformed body, timeout). The endpoint URL comes from the
//! script context `baseUrl`, so no network access is needed. python3 is
//! skipped gracefully when unavailable (the host's own usage framework tests
//! are unix-gated for the same reason).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

const KEY_ENVS: [&str; 8] = [
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "GLM_API_KEY",
    "ZAI_CODING_CN_API_KEY",
    "MINIMAX_API_KEY",
    "MINIMAX_CN_API_KEY",
    "KIMI_API_KEY",
    "KIMI_CODING_API_KEY",
];

// ---------------------------------------------------------------------------
// Stub HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

enum StubReply {
    Status(u16, String),
    Hang,
}

struct Stub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Stub {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn base(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

fn stub(replies: Vec<StubReply>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stub bind");
    let addr = listener.local_addr().expect("stub addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = requests.clone();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming().flatten() {
            let reply = replies
                .next()
                .unwrap_or_else(|| StubReply::Status(200, "{}".to_owned()));
            handle_connection(stream, reply, &sink);
        }
    });
    Stub { addr, requests }
}

fn handle_connection(stream: TcpStream, reply: StubReply, sink: &Arc<Mutex<Vec<RecordedRequest>>>) {
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    sink.lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(RecordedRequest {
            method,
            path,
            headers,
        });
    let mut stream = stream;
    match reply {
        StubReply::Hang => {
            std::thread::sleep(Duration::from_secs(2));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        }
        StubReply::Status(code, body) => {
            let reason = if code == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    }
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Script harness
// ---------------------------------------------------------------------------

struct AgentDir(PathBuf);

impl AgentDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rpi-usage-contract-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("agent dir");
        AgentDir(dir)
    }
}

impl Drop for AgentDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn script_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

#[allow(clippy::too_many_arguments)]
fn run_script(
    script: &str,
    provider: &str,
    base_url: &str,
    key_env: &str,
    agent_dir: &AgentDir,
    timeout_ms: u64,
) -> Run {
    run_script_with_env(
        script,
        provider,
        base_url,
        agent_dir,
        &[(key_env, "test-key")],
        timeout_ms,
    )
}

/// Run a script with an exact environment overlay (all conventional key
/// variables removed) — the auth-store and failure-matrix tests need full
/// control over which credentials are visible.
fn run_script_with_env(
    script: &str,
    provider: &str,
    base_url: &str,
    agent_dir: &AgentDir,
    env: &[(&str, &str)],
    timeout_ms: u64,
) -> Run {
    let mut command = Command::new("python3");
    command
        .arg(script_path(script))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RPI_CODING_AGENT_DIR", &agent_dir.0)
        .env("RPI_USAGE_TIMEOUT_MS", timeout_ms.to_string());
    for name in KEY_ENVS {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("spawn python3");
    let context = serde_json::json!({ "provider": provider, "baseUrl": base_url }).to_string();
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(context.as_bytes())
        .expect("write context");
    let output = child.wait_with_output().expect("wait for script");
    Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn envelope(run: &Run) -> Value {
    assert_eq!(run.status, Some(0), "script failed: {}", run.stderr);
    serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("invalid envelope: {error}: {}", run.stdout))
}

fn assert_no_key_leak(run: &Run) {
    assert!(!run.stdout.contains("test-key"), "stdout leaked the key");
    assert!(!run.stderr.contains("test-key"), "stderr leaked the key");
}

// ---------------------------------------------------------------------------
// Success contracts (recorded fixtures)
// ---------------------------------------------------------------------------

#[test]
fn deepseek_parses_the_recorded_balance() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(
        200,
        fixture("deepseek_balance.json"),
    )]);
    let agent = AgentDir::new("deepseek");
    let run = run_script(
        "deepseek.py",
        "deepseek",
        &stub.base("/user/balance"),
        "DEEPSEEK_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["provider"], "deepseek");
    assert_eq!(value["balance"][0]["currency"], "CNY");
    assert_eq!(value["balance"][0]["total"], 12.5);
    assert_eq!(value["balance"][1]["currency"], "USD");
    assert_eq!(value["displayText"], "deepseek: CNY 12.50 · USD 3.00");
    assert_no_key_leak(&run);

    let requests = stub.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/user/balance");
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
}

#[test]
fn glm_parses_the_recorded_quota_and_uses_the_bearer_shape() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(200, fixture("glm_quota.json"))]);
    let agent = AgentDir::new("glm");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &stub.base("/api/monitor/usage/quota/limit"),
        "ZAI_CODING_CN_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["provider"], "glm-coding-plan");
    assert_eq!(value["plan"], "max");
    assert_eq!(value["quota"]["used"], 0.0);
    assert_eq!(value["quota"]["total"], 100.0);
    assert_eq!(
        value["displayText"],
        "glm-coding-plan: 5h 0% used · MCP 22% used"
    );
    assert_no_key_leak(&run);
    assert_eq!(stub.requests()[0].path, "/api/monitor/usage/quota/limit");
    // The CN-friendly host is not z.ai, so the Bearer shape goes first.
    assert_eq!(
        stub.requests()[0].header("authorization"),
        Some("Bearer test-key")
    );
}

#[test]
fn glm_weekly_window_prefers_the_weekly_quota() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(
        200,
        fixture("glm_quota_weekly.json"),
    )]);
    let agent = AgentDir::new("glm-weekly");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &stub.base("/api/monitor/usage/quota/limit"),
        "ZAI_CODING_CN_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "pro");
    assert_eq!(value["quota"]["used"], 9.0);
    assert_eq!(
        value["displayText"],
        "glm-coding-plan: 5h 22% used · 7d 9% used · MCP 22% used"
    );
    assert!(
        value["resetAt"]
            .as_str()
            .is_some_and(|text| text.ends_with('Z')),
        "{value}"
    );
}

#[test]
fn minimax_parses_the_recorded_remains() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(
        200,
        fixture("minimax_remains.json"),
    )]);
    let agent = AgentDir::new("minimax");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &stub.base("/v1/token_plan/remains"),
        "MINIMAX_CN_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "Token Plan");
    assert_eq!(value["quota"]["used"], 1.0);
    assert_eq!(
        value["displayText"],
        "minimax-token-plan: 5h 1% used · 7d unlimited"
    );
    assert_eq!(value["resetAt"], "2026-10-05T07:00:00Z");
    assert_no_key_leak(&run);
    assert_eq!(stub.requests()[0].path, "/v1/token_plan/remains");
    assert_eq!(
        stub.requests()[0].header("authorization"),
        Some("Bearer test-key")
    );
}

#[test]
fn kimi_parses_the_recorded_usages_with_the_api_key_header() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(200, fixture("kimi_usages.json"))]);
    let agent = AgentDir::new("kimi");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &stub.base("/coding/v1/usages"),
        "KIMI_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "Kimi Code");
    assert_eq!(value["quota"]["total"], 100.0);
    assert_eq!(value["quota"]["unit"], "requests");
    assert_eq!(value["displayText"], "kimi-code: 5h 0% used · 7d 0% used");
    assert_no_key_leak(&run);
    assert_eq!(stub.requests()[0].path, "/coding/v1/usages");
    assert_eq!(stub.requests()[0].header("x-api-key"), Some("test-key"));
    assert_eq!(stub.requests()[0].header("authorization"), None);
}

#[test]
fn kimi_used_windows_and_membership_plan_parse() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(
        200,
        fixture("kimi_usages_used.json"),
    )]);
    let agent = AgentDir::new("kimi-used");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &stub.base("/coding/v1/usages"),
        "KIMI_API_KEY",
        &agent,
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "Intermediate");
    assert_eq!(value["used"], 20.0);
    assert_eq!(value["displayText"], "kimi-code: 5h 45% used · 7d 20% used");
}

// ---------------------------------------------------------------------------
// Degradation matrix
// ---------------------------------------------------------------------------

#[test]
fn missing_key_exits_non_zero_with_guidance() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(
        200,
        fixture("deepseek_balance.json"),
    )]);
    let agent = AgentDir::new("no-key");
    let run = run_script(
        "deepseek.py",
        "deepseek",
        &stub.base("/user/balance"),
        "RPI_USAGE_CONTRACT_NO_SUCH_KEY",
        &agent,
        5_000,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    assert!(run.stderr.contains("DEEPSEEK_API_KEY"), "{}", run.stderr);
    assert!(stub.requests().is_empty(), "no request without a key");
}

#[test]
fn http_401_and_in_band_errors_degrade_to_non_zero_exit() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let agent = AgentDir::new("errors");
    // Plain 401.
    let server = stub(vec![StubReply::Status(
        401,
        r#"{"error":"unauthorized"}"#.to_owned(),
    )]);
    let run = run_script(
        "deepseek.py",
        "deepseek",
        &server.base("/user/balance"),
        "DEEPSEEK_API_KEY",
        &agent,
        5_000,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("HTTP 401"), "{}", run.stderr);
    // GLM in-band failure (HTTP 200, success:false).
    let server = stub(vec![StubReply::Status(200, fixture("glm_error.json"))]);
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &server.base("/api/monitor/usage/quota/limit"),
        "ZAI_CODING_CN_API_KEY",
        &agent,
        5_000,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty());
    // MiniMax in-band status code (HTTP 200, base_resp.status_code=1001).
    let server = stub(vec![StubReply::Status(200, fixture("minimax_error.json"))]);
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &server.base("/v1/token_plan/remains"),
        "MINIMAX_CN_API_KEY",
        &agent,
        5_000,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("status 1001"), "{}", run.stderr);
}

#[test]
fn malformed_body_is_a_failure() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Status(200, "not json".to_owned())]);
    let agent = AgentDir::new("malformed");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &stub.base("/coding/v1/usages"),
        "KIMI_API_KEY",
        &agent,
        5_000,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("non-JSON"), "{}", run.stderr);
}

#[test]
fn timeout_is_a_failure() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let stub = stub(vec![StubReply::Hang]);
    let agent = AgentDir::new("timeout");
    let run = run_script(
        "deepseek.py",
        "deepseek",
        &stub.base("/user/balance"),
        "DEEPSEEK_API_KEY",
        &agent,
        500,
    );
    assert_eq!(run.status, Some(1));
    assert!(run.stdout.is_empty());
    assert!(
        run.stderr.contains("request failed"),
        "timeout degrades with a concise message: {}",
        run.stderr
    );
    assert_no_key_leak(&run);
}

/// Round-2: an `auth.json` api_key stored as the braced form
/// `${VAR}` resolves through the environment exactly like `$VAR`.
#[test]
fn auth_store_braced_env_references_resolve() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    for (script, provider, auth_id) in [
        ("deepseek.py", "deepseek", "deepseek"),
        ("glm_coding_plan.py", "glm-coding-plan", "zai"),
        ("kimi_code.py", "kimi-code", "kimi-coding"),
        ("minimax_token_plan.py", "minimax-token-plan", "minimax"),
    ] {
        let stub = stub(vec![StubReply::Status(401, "{}".to_owned())]);
        let agent = AgentDir::new(&format!("auth-{auth_id}"));
        std::fs::write(
            agent.0.join("auth.json"),
            format!(r#"{{"{auth_id}": {{"type": "api_key", "key": "${{CONTRACT_AUTH_KEY}}"}}}}"#),
        )
        .expect("auth.json");
        let run = run_script_with_env(
            script,
            provider,
            &stub.base("/probe"),
            &agent,
            &[("CONTRACT_AUTH_KEY", "resolved-secret")],
            5_000,
        );
        let requests = stub.requests();
        assert!(
            !requests.is_empty(),
            "{script}: the script must attempt a request"
        );
        assert!(
            requests.iter().any(|request| request
                .header("authorization")
                .is_some_and(|value| value == "Bearer resolved-secret")),
            "{script}: the braced env reference must resolve (status {:?})",
            run.status
        );
        assert_no_key_leak(&run);
    }
}
